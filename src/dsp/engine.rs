//! The EQ itself.
//!
//! # Why everything runs in mid/side
//!
//! The Web Audio prototype rewired its graph whenever a band switched between
//! stereo and mid/side. Here the signal is *always* encoded to mid/side, every
//! band runs on the buses it belongs to, and the result is decoded back. That
//! costs nothing: a stereo band running on both M and S is the same amount of
//! filtering as running on L and R, and because the M/S transform is linear and
//! the two filters are identical, the result is the L/R answer to within
//! rounding. What it buys is a topology that never changes — no rebuild, no
//! reconnection, and no click when a band's channel changes.
//!
//! # Control rate
//!
//! Coefficients are recomputed once per [`CONTROL_BLOCK`] samples from the
//! smoothed parameter values, and the dynamics envelope advances on the same
//! grid. At 48 kHz that is a 0.67 ms grid — fine enough that parameter moves
//! are inaudible, coarse enough that the trig in the coefficient formulas
//! doesn't dominate.
//!
//! The engine deliberately knows nothing about [`crate::params`] types beyond
//! the plain enums: it is driven by a [`Settings`] snapshot, which
//! [`settings_for_block`] builds once per block from the parameters.

use crate::dsp::biquad::{butterworth_qs, Biquad, Coeffs, MAX_SECTIONS};
use crate::dsp::dynamics::{dynamic_step, ms_to_db, DynSettings};
use crate::params::{BandChannel, BandKind, DynMode, EquzxParams, TransientState, MAX_BANDS};

/// Samples between coefficient updates. A power of two, and small enough that a
/// 1 ms dynamics attack still resolves to a handful of steps.
pub const CONTROL_BLOCK: usize = 32;

/// One band's resolved settings for a single control block.
#[derive(Clone, Copy, Debug)]
pub struct BandPlan {
    /// Slot is in use *and* the band is switched on.
    pub running: bool,
    pub kind: BandKind,
    pub channel: BandChannel,
    /// Filter order for cut types; half this many second-order sections.
    pub order: usize,
    pub freq: f32,
    pub gain: f32,
    pub q: f32,
    pub dynamic: bool,
    pub dyn_mode: DynMode,
    pub dyn_range: f32,
    pub threshold: f32,
    pub attack: f32,
    pub release: f32,
}

impl Default for BandPlan {
    fn default() -> Self {
        Self {
            running: false,
            kind: BandKind::Bell,
            channel: BandChannel::Stereo,
            order: 4,
            freq: 1000.0,
            gain: 0.0,
            q: 1.0,
            dynamic: false,
            dyn_mode: DynMode::Above,
            dyn_range: 0.0,
            threshold: -24.0,
            attack: 20.0,
            release: 200.0,
        }
    }
}

/// Everything the engine needs for one control block.
#[derive(Clone, Copy)]
pub struct Settings {
    pub bands: [BandPlan; MAX_BANDS],
    pub output_gain_db: f32,
    pub bypass: bool,
    pub solo: Option<usize>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            bands: [BandPlan::default(); MAX_BANDS],
            output_gain_db: 0.0,
            bypass: false,
            solo: None,
        }
    }
}

/// Advance every smoother by `n` samples and read off this block's settings.
///
/// Smoothers advance whether or not the band they belong to is running, so a
/// band switched on part-way through a move doesn't start from a stale value.
pub fn settings_for_block(
    params: &EquzxParams,
    transient: &TransientState,
    n: usize,
    sr: f32,
) -> Settings {
    let steps = n as u32;
    let nyquist = sr / 2.0 - 1.0;

    let mut settings = Settings {
        bands: [BandPlan::default(); MAX_BANDS],
        output_gain_db: params.output_gain.smoothed.next_step(steps),
        bypass: params.bypass.value(),
        solo: transient.solo(),
    };

    for (plan, p) in settings.bands.iter_mut().zip(params.bands.iter()) {
        let freq = p.freq.smoothed.next_step(steps).clamp(10.0, nyquist);
        let gain = p.gain.smoothed.next_step(steps);
        let q = p.q.smoothed.next_step(steps);
        let dyn_range = p.dyn_range.smoothed.next_step(steps);
        let threshold = p.threshold.smoothed.next_step(steps);
        let kind = p.kind.value();

        *plan = BandPlan {
            running: p.active.value() && p.enabled.value(),
            kind,
            channel: p.channel.value(),
            order: p.slope.value().order(),
            freq,
            // A cut or notch has no gain; carrying the leftover value would make
            // the dynamics section think it had something to move.
            gain: if kind.uses_gain() { gain } else { 0.0 },
            q,
            dynamic: p.dynamic.value() && kind.uses_gain(),
            dyn_mode: p.dyn_mode.value(),
            dyn_range,
            threshold,
            attack: p.attack.value(),
            release: p.release.value(),
        };
    }

    settings
}

/// Everything about a band that determines its coefficients. Recomputing eight
/// sections of trig for a band nobody is touching is the easiest work to skip,
/// so the inputs are cached and compared.
#[derive(Clone, Copy, PartialEq)]
struct CoeffKey {
    kind: u8,
    order: usize,
    freq: f32,
    q: f32,
    gain: f32,
}

impl CoeffKey {
    const NONE: Self = Self {
        kind: u8::MAX,
        order: 0,
        freq: 0.0,
        q: 0.0,
        gain: 0.0,
    };
}

/// Per-band filter state, split by bus. Both buses share one coefficient set —
/// a band is one filter, applied wherever it is routed.
struct BandRuntime {
    coeffs: [Coeffs; MAX_SECTIONS],
    sections: usize,
    mid: [Biquad; MAX_SECTIONS],
    side: [Biquad; MAX_SECTIONS],
    key: CoeffKey,

    /// Sidechain filter isolating the region this band acts on.
    det_coeffs: Coeffs,
    det: Biquad,
    det_key: CoeffKey,
    env: f32,

    /// Gain offset the dynamics section is currently applying, in dB.
    delta_db: f32,
    /// Level measured on the band-filtered input, in dBFS. Drives the UI meter.
    level_db: f32,

    ran_mid: bool,
    ran_side: bool,
}

impl Default for BandRuntime {
    fn default() -> Self {
        Self {
            coeffs: [Coeffs::identity(); MAX_SECTIONS],
            sections: 0,
            mid: [Biquad::new(); MAX_SECTIONS],
            side: [Biquad::new(); MAX_SECTIONS],
            key: CoeffKey::NONE,
            det_coeffs: Coeffs::identity(),
            det: Biquad::new(),
            det_key: CoeffKey::NONE,
            env: 0.0,
            delta_db: 0.0,
            level_db: -100.0,
            ran_mid: false,
            ran_side: false,
        }
    }
}

impl BandRuntime {
    fn reset(&mut self) {
        for s in self.mid.iter_mut().chain(self.side.iter_mut()) {
            s.reset();
        }
        self.det.reset();
        self.env = 0.0;
        self.delta_db = 0.0;
        self.level_db = -100.0;
    }

    /// Rebuild the cascade for the current settings, unless nothing moved.
    fn update_coeffs(
        &mut self,
        kind: BandKind,
        order: usize,
        freq: f32,
        q: f32,
        gain: f32,
        sr: f32,
    ) {
        let key = CoeffKey {
            kind: kind as u8,
            order,
            freq,
            q,
            gain,
        };
        if key == self.key {
            return;
        }
        self.key = key;

        match kind {
            BandKind::Bell => {
                self.coeffs[0] = Coeffs::peaking(freq, q, gain, sr);
                self.sections = 1;
            }
            BandKind::LowShelf => {
                self.coeffs[0] = Coeffs::low_shelf(freq, gain, sr);
                self.sections = 1;
            }
            BandKind::HighShelf => {
                self.coeffs[0] = Coeffs::high_shelf(freq, gain, sr);
                self.sections = 1;
            }
            BandKind::Notch => {
                self.coeffs[0] = Coeffs::notch(freq, q, sr);
                self.sections = 1;
            }
            BandKind::BandPass => {
                self.coeffs[0] = Coeffs::bandpass(freq, q, sr);
                self.sections = 1;
            }
            BandKind::LowCut | BandKind::HighCut => {
                let mut qs = [0.0f32; MAX_SECTIONS];
                let n = butterworth_qs(order, &mut qs);
                for i in 0..n {
                    self.coeffs[i] = if kind == BandKind::LowCut {
                        Coeffs::highpass(freq, qs[i], sr)
                    } else {
                        Coeffs::lowpass(freq, qs[i], sr)
                    };
                }
                self.sections = n;
            }
        }
    }

    /// The sidechain listens through the slice of spectrum the band acts on:
    /// a shelf hears everything on its side of the corner, a bell hears its bump.
    fn update_detector(&mut self, kind: BandKind, freq: f32, q: f32, sr: f32) {
        let key = CoeffKey {
            kind: kind as u8,
            order: 0,
            freq,
            q,
            gain: 0.0,
        };
        if key == self.det_key {
            return;
        }
        self.det_key = key;
        self.det_coeffs = match kind {
            BandKind::LowShelf => Coeffs::lowpass(freq, 1.0, sr),
            BandKind::HighShelf => Coeffs::highpass(freq, 1.0, sr),
            _ => Coeffs::bandpass(freq, q.max(0.5), sr),
        };
    }

    #[inline]
    fn run(&mut self, buf: &mut [f32], bus: Bus) {
        let states = match bus {
            Bus::Mid => &mut self.mid,
            Bus::Side => &mut self.side,
        };
        for section in 0..self.sections {
            let c = self.coeffs[section];
            let state = &mut states[section];
            for x in buf.iter_mut() {
                *x = state.process(*x, &c);
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Bus {
    Mid,
    Side,
}

/// Snapshot of the per-band meters the editor polls each frame.
#[derive(Clone, Copy, Default, Debug)]
pub struct BandMeter {
    pub level_db: f32,
    pub delta_db: f32,
}

pub struct EqEngine {
    sr: f32,
    bands: Box<[BandRuntime; MAX_BANDS]>,
    /// Solo listens through the band's own region, applied after the decode.
    solo_coeffs: Coeffs,
    solo_key: CoeffKey,
    solo_l: Biquad,
    solo_r: Biquad,

    mid_buf: [f32; CONTROL_BLOCK],
    side_buf: [f32; CONTROL_BLOCK],
    det_buf: [f32; CONTROL_BLOCK],
}

impl Default for EqEngine {
    fn default() -> Self {
        Self::new(48_000.0)
    }
}

impl EqEngine {
    pub fn new(sr: f32) -> Self {
        Self {
            sr,
            bands: Box::new(std::array::from_fn(|_| BandRuntime::default())),
            solo_coeffs: Coeffs::identity(),
            solo_key: CoeffKey::NONE,
            solo_l: Biquad::new(),
            solo_r: Biquad::new(),
            mid_buf: [0.0; CONTROL_BLOCK],
            side_buf: [0.0; CONTROL_BLOCK],
            det_buf: [0.0; CONTROL_BLOCK],
        }
    }

    pub fn sample_rate(&self) -> f32 {
        self.sr
    }

    pub fn set_sample_rate(&mut self, sr: f32) {
        if (sr - self.sr).abs() > f32::EPSILON {
            self.sr = sr;
            // Every cached coefficient was computed against the old rate.
            for band in self.bands.iter_mut() {
                band.key = CoeffKey::NONE;
                band.det_key = CoeffKey::NONE;
            }
            self.solo_key = CoeffKey::NONE;
        }
        self.reset();
    }

    pub fn reset(&mut self) {
        for band in self.bands.iter_mut() {
            band.reset();
        }
        self.solo_l.reset();
        self.solo_r.reset();
        self.solo_key = CoeffKey::NONE;
    }

    pub fn meter(&self, slot: usize) -> BandMeter {
        let band = &self.bands[slot];
        BandMeter {
            level_db: band.level_db,
            delta_db: band.delta_db,
        }
    }

    /// Process one control block in place. Pass `None` for `right` on mono input,
    /// in which case the side bus is skipped entirely.
    pub fn process_block(&mut self, left: &mut [f32], right: Option<&mut [f32]>, s: &Settings) {
        let n = left.len().min(CONTROL_BLOCK);
        if n == 0 {
            return;
        }
        let dt = n as f32 / self.sr;

        // Bypass is a real bypass: no filtering, no output trim. Hosts wire their
        // own bypass button to this parameter and expect the plugin to vanish.
        if s.bypass {
            for band in self.bands.iter_mut() {
                if band.ran_mid || band.ran_side {
                    band.reset();
                    band.ran_mid = false;
                    band.ran_side = false;
                }
            }
            return;
        }

        let stereo = right.is_some();
        let sr = self.sr;

        // --- encode ---------------------------------------------------------
        {
            let mid = &mut self.mid_buf[..n];
            let side = &mut self.side_buf[..n];
            match right.as_ref() {
                Some(r) => {
                    for i in 0..n {
                        mid[i] = 0.5 * (left[i] + r[i]);
                        side[i] = 0.5 * (left[i] - r[i]);
                    }
                }
                None => {
                    mid.copy_from_slice(&left[..n]);
                    side.fill(0.0);
                }
            }
        }

        // --- dynamics -------------------------------------------------------
        // Detectors tap the input, before any band has touched it — the same
        // sidechain point the prototype used, and what stops one dynamic band
        // from chasing another's gain reduction.
        for slot in 0..MAX_BANDS {
            let p = s.bands[slot];
            let band = &mut self.bands[slot];

            if !p.running || !p.dynamic {
                band.delta_db = 0.0;
                band.env = 0.0;
                band.level_db = -100.0;
                continue;
            }

            // A stereo band hears the mono sum, which is exactly the mid bus.
            let src = match p.channel {
                BandChannel::Side => &self.side_buf[..n],
                _ => &self.mid_buf[..n],
            };
            self.det_buf[..n].copy_from_slice(src);

            band.update_detector(p.kind, p.freq, p.q, sr);
            let c = band.det_coeffs;
            let mut sum = 0.0f32;
            for x in self.det_buf[..n].iter_mut() {
                let y = band.det.process(*x, &c);
                *x = y;
                sum += y * y;
            }
            band.level_db = ms_to_db(sum / n as f32);

            let (env, delta) = dynamic_step(
                DynSettings {
                    threshold_db: p.threshold,
                    mode: p.dyn_mode,
                    range_db: p.dyn_range,
                    attack_ms: p.attack,
                    release_ms: p.release,
                },
                band.level_db,
                band.env,
                dt,
            );
            band.env = env;
            band.delta_db = delta;
        }

        // --- bands ----------------------------------------------------------
        for slot in 0..MAX_BANDS {
            let p = s.bands[slot];
            let band = &mut self.bands[slot];

            let soloed_out = s.solo.is_some() && s.solo != Some(slot);
            let (run_mid, run_side) = if !p.running || soloed_out {
                (false, false)
            } else {
                (
                    p.channel != BandChannel::Side,
                    stereo && p.channel != BandChannel::Mid,
                )
            };

            // Coming back from silence with stale state in the delay line is a click.
            if (band.ran_mid && !run_mid) || (band.ran_side && !run_side) {
                band.reset();
            }
            band.ran_mid = run_mid;
            band.ran_side = run_side;
            if !run_mid && !run_side {
                continue;
            }

            band.update_coeffs(p.kind, p.order, p.freq, p.q, p.gain + band.delta_db, sr);
            if run_mid {
                band.run(&mut self.mid_buf[..n], Bus::Mid);
            }
            if run_side {
                band.run(&mut self.side_buf[..n], Bus::Side);
            }
        }

        // Soloing a mid- or side-only band isolates that bus too, so you hear the
        // slice on its own rather than folded back into the opposite channel.
        if let Some(slot) = s.solo {
            match s.bands[slot].channel {
                BandChannel::Mid => self.side_buf[..n].fill(0.0),
                BandChannel::Side => self.mid_buf[..n].fill(0.0),
                BandChannel::Stereo => {}
            }
        }

        // --- decode ---------------------------------------------------------
        let out_gain = 10f32.powf(s.output_gain_db / 20.0);
        match right {
            Some(r) => {
                for i in 0..n {
                    let (m, sd) = (self.mid_buf[i], self.side_buf[i]);
                    left[i] = (m + sd) * out_gain;
                    r[i] = (m - sd) * out_gain;
                }
                if let Some(slot) = s.solo {
                    self.apply_solo(s.bands[slot], left, Some(r), n);
                } else {
                    self.clear_solo();
                }
            }
            None => {
                for i in 0..n {
                    left[i] = self.mid_buf[i] * out_gain;
                }
                if let Some(slot) = s.solo {
                    self.apply_solo(s.bands[slot], left, None, n);
                } else {
                    self.clear_solo();
                }
            }
        }
    }

    fn apply_solo(&mut self, p: BandPlan, left: &mut [f32], right: Option<&mut [f32]>, n: usize) {
        let key = CoeffKey {
            kind: p.kind as u8,
            order: 0,
            freq: p.freq,
            q: p.q,
            gain: 0.0,
        };
        if key != self.solo_key {
            self.solo_key = key;
            self.solo_coeffs = match p.kind {
                BandKind::LowCut | BandKind::LowShelf => Coeffs::lowpass(p.freq, 1.0, self.sr),
                BandKind::HighCut | BandKind::HighShelf => Coeffs::highpass(p.freq, 1.0, self.sr),
                _ => Coeffs::bandpass(p.freq, p.q.max(0.7), self.sr),
            };
        }

        let c = self.solo_coeffs;
        for x in left[..n].iter_mut() {
            *x = self.solo_l.process(*x, &c);
        }
        if let Some(r) = right {
            for x in r[..n].iter_mut() {
                *x = self.solo_r.process(*x, &c);
            }
        }
    }

    fn clear_solo(&mut self) {
        if self.solo_key != CoeffKey::NONE {
            self.solo_l.reset();
            self.solo_r.reset();
            self.solo_key = CoeffKey::NONE;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::Slope;
    use std::f32::consts::PI;

    const SR: f32 = 48_000.0;
    /// A full-scale sine has an RMS of -3.01 dB.
    const SINE_RMS_DB: f32 = -3.0103;

    fn bell(freq: f32, gain: f32, q: f32) -> BandPlan {
        BandPlan {
            running: true,
            kind: BandKind::Bell,
            freq,
            gain,
            q,
            ..BandPlan::default()
        }
    }

    fn one_band(plan: BandPlan) -> Settings {
        let mut s = Settings::default();
        s.bands[0] = plan;
        s
    }

    /// Drive a sine through the engine and return the output RMS of L, in dB.
    /// The first half of the run lets the filter settle; only the tail is measured.
    fn rms_db(engine: &mut EqEngine, s: &Settings, freq: f32, stereo: bool) -> f32 {
        let mut sum = 0.0f64;
        let mut counted = 0usize;
        for block in 0..200 {
            let mut l = [0.0f32; CONTROL_BLOCK];
            let mut r = [0.0f32; CONTROL_BLOCK];
            for i in 0..CONTROL_BLOCK {
                let t = (block * CONTROL_BLOCK + i) as f32 / SR;
                let x = (2.0 * PI * freq * t).sin();
                l[i] = x;
                r[i] = x;
            }
            if stereo {
                engine.process_block(&mut l, Some(&mut r), s);
            } else {
                engine.process_block(&mut l, None, s);
            }
            if block >= 100 {
                for x in l {
                    sum += (x * x) as f64;
                    counted += 1;
                }
            }
        }
        20.0 * ((sum / counted as f64).sqrt().max(1e-9) as f32).log10()
    }

    #[test]
    fn an_empty_eq_is_transparent() {
        let mut engine = EqEngine::new(SR);
        let out = rms_db(&mut engine, &Settings::default(), 1000.0, true);
        assert!((out - SINE_RMS_DB).abs() < 0.01, "got {out} dB");
    }

    #[test]
    fn a_bell_applies_its_gain_at_center() {
        let mut engine = EqEngine::new(SR);
        let s = one_band(bell(1000.0, 6.0, 1.0));
        let out = rms_db(&mut engine, &s, 1000.0, true);
        assert!((out - (SINE_RMS_DB + 6.0)).abs() < 0.1, "got {out} dB");
    }

    #[test]
    fn a_band_that_is_not_running_does_nothing() {
        let mut engine = EqEngine::new(SR);
        let s = one_band(BandPlan {
            running: false,
            ..bell(1000.0, 12.0, 1.0)
        });
        let out = rms_db(&mut engine, &s, 1000.0, true);
        assert!((out - SINE_RMS_DB).abs() < 0.01, "got {out} dB");
    }

    #[test]
    fn bypass_is_transparent_even_with_bands_and_output_trim() {
        let mut engine = EqEngine::new(SR);
        let mut s = one_band(bell(1000.0, -20.0, 1.0));
        s.output_gain_db = -12.0;
        s.bypass = true;
        let out = rms_db(&mut engine, &s, 1000.0, true);
        assert!((out - SINE_RMS_DB).abs() < 0.02, "got {out} dB");
    }

    #[test]
    fn output_gain_scales_the_result() {
        let mut engine = EqEngine::new(SR);
        let s = Settings {
            output_gain_db: -6.0,
            ..Settings::default()
        };
        let out = rms_db(&mut engine, &s, 1000.0, true);
        assert!((out - (SINE_RMS_DB - 6.0)).abs() < 0.02, "got {out} dB");
    }

    #[test]
    fn a_low_cut_removes_the_low_end_and_keeps_the_top() {
        let s = one_band(BandPlan {
            running: true,
            kind: BandKind::LowCut,
            freq: 1000.0,
            order: Slope::S24.order(),
            ..BandPlan::default()
        });

        // An octave below cutoff a 24 dB/oct cut is ~24 dB down.
        let mut engine = EqEngine::new(SR);
        let low = rms_db(&mut engine, &s, 500.0, true);
        assert!(
            (low - (SINE_RMS_DB - 24.0)).abs() < 1.0,
            "500 Hz came out at {low} dB"
        );

        let mut engine = EqEngine::new(SR);
        let high = rms_db(&mut engine, &s, 8000.0, true);
        assert!(
            (high - SINE_RMS_DB).abs() < 0.2,
            "8 kHz came out at {high} dB"
        );
    }

    #[test]
    fn steeper_slopes_cut_harder() {
        let mut last = f32::INFINITY;
        for slope in [Slope::S12, Slope::S24, Slope::S48, Slope::S96] {
            let s = one_band(BandPlan {
                running: true,
                kind: BandKind::LowCut,
                freq: 1000.0,
                order: slope.order(),
                ..BandPlan::default()
            });
            let mut engine = EqEngine::new(SR);
            let out = rms_db(&mut engine, &s, 500.0, true);
            assert!(
                out < last - 5.0,
                "{:?} gave {out} dB, previous was {last}",
                slope
            );
            last = out;
        }
    }

    #[test]
    fn a_mid_band_leaves_a_side_only_signal_alone() {
        let mut engine = EqEngine::new(SR);
        let s = one_band(BandPlan {
            channel: BandChannel::Mid,
            ..bell(1000.0, -24.0, 1.0)
        });

        // Anti-phase pair: pure side, no mid at all.
        let mut peak = 0.0f32;
        for block in 0..200 {
            let mut l = [0.0f32; CONTROL_BLOCK];
            let mut r = [0.0f32; CONTROL_BLOCK];
            for i in 0..CONTROL_BLOCK {
                let t = (block * CONTROL_BLOCK + i) as f32 / SR;
                let x = (2.0 * PI * 1000.0 * t).sin();
                l[i] = x;
                r[i] = -x;
            }
            engine.process_block(&mut l, Some(&mut r), &s);
            if block >= 100 {
                for i in 0..CONTROL_BLOCK {
                    peak = peak.max(l[i].abs());
                    // The pair must stay anti-phase — a mid band cannot break that.
                    assert!((l[i] + r[i]).abs() < 1e-4);
                }
            }
        }
        assert!(
            (peak - 1.0).abs() < 0.01,
            "side signal was attenuated to {peak}"
        );
    }

    #[test]
    fn a_side_band_leaves_mono_material_alone() {
        let mut engine = EqEngine::new(SR);
        // A mono sine has no side content, so a side band cannot touch it.
        let s = one_band(BandPlan {
            channel: BandChannel::Side,
            ..bell(1000.0, -24.0, 1.0)
        });
        let out = rms_db(&mut engine, &s, 1000.0, true);
        assert!((out - SINE_RMS_DB).abs() < 0.01, "got {out} dB");
    }

    #[test]
    fn a_stereo_band_in_the_ms_domain_matches_plain_lr_filtering() {
        let mut engine = EqEngine::new(SR);
        let s = one_band(bell(800.0, 9.0, 2.0));

        // Reference: the same filter run straight on L.
        let c = Coeffs::peaking(800.0, 2.0, 9.0, SR);
        let mut reference = Biquad::new();

        for block in 0..64 {
            let mut l = [0.0f32; CONTROL_BLOCK];
            let mut r = [0.0f32; CONTROL_BLOCK];
            let mut expected = [0.0f32; CONTROL_BLOCK];
            for i in 0..CONTROL_BLOCK {
                let t = (block * CONTROL_BLOCK + i) as f32 / SR;
                // Uncorrelated channels, so mid and side both carry signal.
                l[i] = (2.0 * PI * 300.0 * t).sin();
                r[i] = (2.0 * PI * 1700.0 * t).sin() * 0.6;
                expected[i] = reference.process(l[i], &c);
            }
            engine.process_block(&mut l, Some(&mut r), &s);
            for i in 0..CONTROL_BLOCK {
                assert!(
                    (l[i] - expected[i]).abs() < 1e-4,
                    "block {block} sample {i}: {} vs {}",
                    l[i],
                    expected[i]
                );
            }
        }
    }

    #[test]
    fn a_dynamic_band_ducks_a_loud_signal() {
        let mut engine = EqEngine::new(SR);
        let s = one_band(BandPlan {
            dynamic: true,
            dyn_range: -12.0,
            threshold: -30.0,
            attack: 1.0,
            release: 10.0,
            ..bell(1000.0, 0.0, 1.0)
        });

        let out = rms_db(&mut engine, &s, 1000.0, true);
        // A full-scale tone sits far above -30 dBFS, so the band is fully engaged.
        assert!((out - (SINE_RMS_DB - 12.0)).abs() < 0.5, "got {out} dB");
        assert!(engine.meter(0).delta_db < -11.0);
        assert!(engine.meter(0).level_db > -10.0);
    }

    #[test]
    fn a_dynamic_band_stays_out_of_the_way_below_threshold() {
        let mut engine = EqEngine::new(SR);
        let s = one_band(BandPlan {
            dynamic: true,
            dyn_range: -12.0,
            threshold: -6.0,
            ..bell(1000.0, 0.0, 1.0)
        });

        // A -40 dBFS tone against a -6 dB threshold: nothing engages.
        let amp = 10f32.powf(-40.0 / 20.0);
        for block in 0..400 {
            let mut l = [0.0f32; CONTROL_BLOCK];
            let mut r = [0.0f32; CONTROL_BLOCK];
            for i in 0..CONTROL_BLOCK {
                let t = (block * CONTROL_BLOCK + i) as f32 / SR;
                let x = (2.0 * PI * 1000.0 * t).sin() * amp;
                l[i] = x;
                r[i] = x;
            }
            engine.process_block(&mut l, Some(&mut r), &s);
        }
        assert!(engine.meter(0).delta_db.abs() < 0.01);
    }

    #[test]
    fn dynamics_only_apply_to_gain_bearing_types() {
        // A notch has no gain, so `settings_for_block` refuses to make it dynamic;
        // the engine trusts that and leaves the meter parked.
        let mut engine = EqEngine::new(SR);
        let s = one_band(BandPlan {
            running: true,
            kind: BandKind::Notch,
            dynamic: false,
            ..BandPlan::default()
        });
        rms_db(&mut engine, &s, 1000.0, true);
        assert_eq!(engine.meter(0).delta_db, 0.0);
    }

    #[test]
    fn mono_processing_applies_the_band() {
        let mut engine = EqEngine::new(SR);
        let s = one_band(bell(1000.0, 6.0, 1.0));
        let out = rms_db(&mut engine, &s, 1000.0, false);
        assert!((out - (SINE_RMS_DB + 6.0)).abs() < 0.1, "got {out} dB");
    }

    #[test]
    fn solo_drops_the_other_bands_and_keeps_its_own_region() {
        let mut s = Settings::default();
        // Band 0: a high cut that would gut the test tone if it ran.
        s.bands[0] = BandPlan {
            running: true,
            kind: BandKind::HighCut,
            freq: 200.0,
            order: Slope::S24.order(),
            ..BandPlan::default()
        };
        // Band 1: soloed, sitting on the test tone.
        s.bands[1] = bell(1000.0, 0.0, 1.0);
        s.solo = Some(1);

        let mut engine = EqEngine::new(SR);
        let out = rms_db(&mut engine, &s, 1000.0, true);
        assert!(
            out > SINE_RMS_DB - 3.0,
            "soloed band should pass its own region, got {out}"
        );

        // Well outside the soloed band, the listening filter should reject.
        let mut engine = EqEngine::new(SR);
        let far = rms_db(&mut engine, &s, 60.0, true);
        assert!(
            far < SINE_RMS_DB - 20.0,
            "out-of-band material leaked at {far} dB"
        );
    }

    #[test]
    fn changing_sample_rate_rebuilds_coefficients() {
        let s = one_band(bell(1000.0, 6.0, 1.0));
        let mut engine = EqEngine::new(SR);
        rms_db(&mut engine, &s, 1000.0, true);

        engine.set_sample_rate(96_000.0);
        // Same tone at the new rate must still land on the band's center.
        let mut sum = 0.0f64;
        let mut counted = 0usize;
        for block in 0..400 {
            let mut l = [0.0f32; CONTROL_BLOCK];
            let mut r = [0.0f32; CONTROL_BLOCK];
            for i in 0..CONTROL_BLOCK {
                let t = (block * CONTROL_BLOCK + i) as f32 / 96_000.0;
                let x = (2.0 * PI * 1000.0 * t).sin();
                l[i] = x;
                r[i] = x;
            }
            engine.process_block(&mut l, Some(&mut r), &s);
            if block >= 200 {
                for x in l {
                    sum += (x * x) as f64;
                    counted += 1;
                }
            }
        }
        let out = 20.0 * ((sum / counted as f64).sqrt() as f32).log10();
        assert!(
            (out - (SINE_RMS_DB + 6.0)).abs() < 0.15,
            "got {out} dB at 96 kHz"
        );
    }

    #[test]
    fn a_full_bank_of_bands_stays_finite() {
        let mut s = Settings::default();
        for (i, plan) in s.bands.iter_mut().enumerate() {
            let freq = 30.0 * 1.28f32.powi(i as i32);
            *plan = BandPlan {
                running: true,
                channel: match i % 3 {
                    0 => BandChannel::Stereo,
                    1 => BandChannel::Mid,
                    _ => BandChannel::Side,
                },
                dynamic: i % 4 == 0,
                dyn_range: -6.0,
                threshold: -40.0,
                ..bell(freq.min(20_000.0), if i % 2 == 0 { 6.0 } else { -6.0 }, 1.4)
            };
        }
        // Two of them are steep cuts, for the cascade path.
        s.bands[0] = BandPlan {
            running: true,
            kind: BandKind::LowCut,
            freq: 30.0,
            order: Slope::S96.order(),
            ..BandPlan::default()
        };
        s.bands[23] = BandPlan {
            running: true,
            kind: BandKind::HighCut,
            freq: 18_000.0,
            order: Slope::S96.order(),
            ..BandPlan::default()
        };

        let mut engine = EqEngine::new(SR);
        for block in 0..500 {
            let mut l = [0.0f32; CONTROL_BLOCK];
            let mut r = [0.0f32; CONTROL_BLOCK];
            for i in 0..CONTROL_BLOCK {
                let t = (block * CONTROL_BLOCK + i) as f32 / SR;
                l[i] = (2.0 * PI * 220.0 * t).sin() * 0.5 + (2.0 * PI * 3300.0 * t).sin() * 0.3;
                r[i] = (2.0 * PI * 190.0 * t).sin() * 0.5;
            }
            engine.process_block(&mut l, Some(&mut r), &s);
            for i in 0..CONTROL_BLOCK {
                assert!(
                    l[i].is_finite() && r[i].is_finite(),
                    "block {block} went non-finite"
                );
                assert!(l[i].abs() < 50.0, "block {block} blew up to {}", l[i]);
            }
        }
    }
}
