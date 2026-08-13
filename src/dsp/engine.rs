//! The EQ itself.
//!
//! # Two domains, one chain
//!
//! A band can act on the stereo signal, on left or right alone, or on mid or
//! side alone. The last two pairs are different *views* of the same signal, and
//! a left-only filter is not expressible as a pair of independent mid and side
//! filters — so no single domain can serve every band.
//!
//! What makes that cheap is that the two views are related by an exactly
//! invertible transform costing four operations a sample. So the chain carries
//! the signal in whichever domain the band it is about to run needs, converting
//! in place when it crosses from one to the other, and converts back to
//! left/right at the end. A stereo band needs neither domain in particular: the
//! same filter on both buses gives the same answer either way, because the
//! transform is linear.
//!
//! In practice the conversions are rare — bands of a kind tend to sit together,
//! and a session that only uses stereo and mid/side bands does exactly one
//! encode and one decode, the same as before left/right existed.
//!
//! # Control rate
//!
//! Coefficients are recomputed once per [`CONTROL_BLOCK`] samples from the
//! smoothed parameter values, and the dynamics envelope advances on the same
//! grid. At 48 kHz that is a 0.67 ms grid — fine enough that parameter moves
//! are inaudible, coarse enough that the trig in the coefficient formulas
//! doesn't dominate. The envelope loses nothing to that grid: its one-pole is
//! solved exactly for a target held constant across the block, which is what a
//! target built from control-rate settings always is.
//!
//! Level *detection*, though, runs at audio rate — see
//! [`crate::dsp::dynamics::LevelDetector`]. A block is a fraction of a cycle at
//! the bottom of the range, so a level taken block by block is a reading of the
//! waveform rather than of its loudness.
//!
//! The engine deliberately knows nothing about [`crate::params`] types beyond
//! the plain enums: it is driven by a [`Settings`] snapshot, which
//! [`settings_for_block`] builds once per block from the parameters.

use std::sync::Arc;

use crate::dsp::biquad::{band_cascade, Biquad, Coeffs, MAX_SECTIONS};
use crate::dsp::dynamics::{dynamic_step, step_toward, DynSettings, LevelDetector};
use crate::dsp::resonance::{
    band_freq, band_region_weight, crossfade, BandOverlays, ResonanceBank, ResonanceSettings,
    RES_BANDS,
};
use crate::dsp::spectral::{
    AdaptivePool, BandCtl, BandRegionView, ConfigView, PoolControls, PoolTuning, SharedSpectral,
    TargetView, MAX_TARGETS,
};
use crate::params::{
    BandChannel, BandKind, BandResMode, Domain, DynMode, EquzxParams, ResMode, ResQuality,
    TransientState, MAX_BANDS,
};

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
    /// Adaptive resonance suppression inside this band's own region, 0..1.
    pub resonance: f32,
    /// How that amount finds its resonances — see [`BandResMode`].
    pub res_mode: BandResMode,
    /// Ceiling on this band's cut, in dB.
    pub res_range: f32,
    /// dB taken off the detection threshold inside this band's region.
    pub res_sens: f32,
    /// Half-width of the spectral search region, octaves either side.
    pub res_width: f32,
    /// Ballistics for this band's resonance attenuation.
    pub res_attack: f32,
    pub res_release: f32,
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
            q: crate::dsp::biquad::FLAT_Q,
            dynamic: false,
            dyn_mode: DynMode::Above,
            dyn_range: 0.0,
            threshold: -24.0,
            attack: 20.0,
            release: 200.0,
            resonance: 0.0,
            res_mode: BandResMode::Adaptive,
            res_range: 36.0,
            res_sens: 0.0,
            res_width: 1.0,
            res_attack: 5.0,
            res_release: 40.0,
        }
    }
}

/// Everything the engine needs for one control block.
#[derive(Clone, Copy)]
pub struct Settings {
    pub bands: [BandPlan; MAX_BANDS],
    pub output_gain_db: f32,
    pub bypass: bool,
    pub phase_invert: bool,
    pub solo: Option<usize>,
    pub resonance: ResonanceSettings,
    /// Which engine the global resonance switch arms.
    pub res_mode: ResMode,
    /// Adaptive filter budget for the spectral engine.
    pub res_quality: ResQuality,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            bands: [BandPlan::default(); MAX_BANDS],
            output_gain_db: 0.0,
            bypass: false,
            phase_invert: false,
            solo: None,
            resonance: ResonanceSettings::default(),
            res_mode: ResMode::Adaptive,
            res_quality: ResQuality::Ultra,
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

    let res = &params.resonance;
    let mut settings = Settings {
        bands: [BandPlan::default(); MAX_BANDS],
        output_gain_db: params.output_gain.smoothed.next_step(steps),
        bypass: params.bypass.value(),
        phase_invert: params.phase_invert.value(),
        solo: transient.solo(),
        resonance: ResonanceSettings {
            enabled: res.enabled.value(),
            depth: res.depth.smoothed.next_step(steps),
            sharpness: res.sharpness.smoothed.next_step(steps),
            threshold_db: res.threshold.smoothed.next_step(steps),
            attack_ms: res.attack.value(),
            release_ms: res.release.value(),
            low_hz: res.low.smoothed.next_step(steps),
            high_hz: res.high.smoothed.next_step(steps),
            range_db: res.range.smoothed.next_step(steps),
            mix: res.mix.smoothed.next_step(steps),
            delta: res.delta.value(),
        },
        res_mode: res.mode.value(),
        res_quality: res.quality.value(),
    };

    for (plan, p) in settings.bands.iter_mut().zip(params.bands.iter()) {
        let freq = p.freq.smoothed.next_step(steps).clamp(10.0, nyquist);
        let gain = p.gain.smoothed.next_step(steps);
        let q = p.q.smoothed.next_step(steps);
        let dyn_range = p.dyn_range.smoothed.next_step(steps);
        let threshold = p.threshold.smoothed.next_step(steps);
        let resonance = p.resonance.smoothed.next_step(steps);
        let res_range = p.res_range.smoothed.next_step(steps);
        let res_sens = p.res_sens.smoothed.next_step(steps);
        let res_width = p.res_width.smoothed.next_step(steps);
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
            resonance,
            res_mode: p.res_mode.value(),
            res_range,
            res_sens,
            res_width,
            res_attack: p.res_attack.value(),
            res_release: p.res_release.value(),
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
///
/// Which signal a bus carries depends on the domain the band runs in, which is
/// why they are named by position rather than by channel.
struct BandRuntime {
    coeffs: [Coeffs; MAX_SECTIONS],
    sections: usize,
    /// Filter state for the first bus of the band's domain — left, or mid.
    bus_a: [Biquad; MAX_SECTIONS],
    /// And for the second — right, or side.
    bus_b: [Biquad; MAX_SECTIONS],
    key: CoeffKey,

    /// Sidechain filter isolating the region this band acts on. One per bus, so
    /// a band routed to both hears both — an anti-phase pair is silent summed to
    /// mono while still being filtered at full level on each side.
    det_coeffs: Coeffs,
    det_a: Biquad,
    det_b: Biquad,
    det_key: CoeffKey,
    /// Integrates the sidechain into a level that means something at the band's
    /// own frequency, rather than one control block's worth of waveform.
    level: LevelDetector,
    env: f32,

    /// Gain offset the dynamics section is currently applying, in dB.
    delta_db: f32,
    /// Level measured on the band-filtered input, in dBFS. Drives the UI meter.
    level_db: f32,

    ran_a: bool,
    ran_b: bool,
    /// Domain the band last ran in. Filter state carried across a domain change
    /// describes a different signal, so it is dropped rather than reused.
    ran_domain: Option<Domain>,
}

impl Default for BandRuntime {
    fn default() -> Self {
        Self {
            coeffs: [Coeffs::identity(); MAX_SECTIONS],
            sections: 0,
            bus_a: [Biquad::new(); MAX_SECTIONS],
            bus_b: [Biquad::new(); MAX_SECTIONS],
            key: CoeffKey::NONE,
            det_coeffs: Coeffs::identity(),
            det_a: Biquad::new(),
            det_b: Biquad::new(),
            det_key: CoeffKey::NONE,
            level: LevelDetector::new(),
            env: 0.0,
            delta_db: 0.0,
            level_db: -100.0,
            ran_a: false,
            ran_b: false,
            ran_domain: None,
        }
    }
}

impl BandRuntime {
    /// Drop the delay lines of the band's own filters, and nothing else.
    ///
    /// This is what a routing change calls for: the state describes a signal the
    /// band is no longer looking at, but the sidechain taps the input and never
    /// did care which domain the chain is in, so its level and envelope are
    /// still current. Wiping those too would drop the gain offset to zero for a
    /// block — an audible step, on a change the user made for other reasons.
    fn reset_filters(&mut self) {
        for s in self.bus_a.iter_mut().chain(self.bus_b.iter_mut()) {
            s.reset();
        }
    }

    fn reset(&mut self) {
        self.reset_filters();
        self.det_a.reset();
        self.det_b.reset();
        self.level.reset();
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
        self.coeffs = [Coeffs::identity(); MAX_SECTIONS];
        self.sections = band_cascade(kind, freq, sr, order, q, gain, &mut self.coeffs);
    }

    /// The sidechain listens through the slice of spectrum the band acts on:
    /// a shelf hears everything on its side of the corner, a bell hears its bump.
    fn update_detector(&mut self, kind: BandKind, freq: f32, q: f32, sr: f32) {
        // A detector as narrow as a band is allowed to be — Q goes to 40 — is an
        // oscillator rather than a meter: it rings for the best part of a second
        // and reads next to nothing off real material, so the surgical bands
        // that most want dynamics would be the ones that never engaged. Widen
        // the listening filter without widening the band it drives.
        let det_q = q.clamp(0.5, 4.0);
        let key = CoeffKey {
            kind: kind as u8,
            order: 0,
            freq,
            q: det_q,
            gain: 0.0,
        };
        if key == self.det_key {
            return;
        }
        self.det_key = key;
        self.det_coeffs = match kind {
            BandKind::LowShelf => Coeffs::lowpass(freq, 1.0, sr),
            BandKind::HighShelf => Coeffs::highpass(freq, 1.0, sr),
            _ => Coeffs::bandpass(freq, det_q, sr),
        };
    }

    #[inline]
    fn run(&mut self, buf: &mut [f32], bus: Bus) {
        let states = match bus {
            Bus::A => &mut self.bus_a,
            Bus::B => &mut self.bus_b,
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

/// Which of a band's two filter-state sets to run through. What the bus carries
/// — left or mid, right or side — depends on the current [`Domain`].
#[derive(Clone, Copy)]
enum Bus {
    A,
    B,
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
    /// Runs after the bands, on whatever they left behind.
    resonance: Box<ResonanceBank>,
    /// Suppression the EQ's own bands are asking for, per bank band. Rebuilt
    /// each block and owned here so `process_block` never allocates.
    overlays: Box<BandOverlays>,
    /// The spectral engine's audio half: the preallocated adaptive filters.
    pool: Box<AdaptivePool>,
    /// What the audio thread shares with the spectral worker — the analysis
    /// ring it feeds, the config it publishes, the targets it reads back.
    shared: Arc<SharedSpectral>,
    /// Solo listens through the band's own region, applied after the decode.
    solo_coeffs: Coeffs,
    solo_key: CoeffKey,
    solo_l: Biquad,
    solo_r: Biquad,

    /// The block's input, kept so the sidechains measure the signal as it
    /// arrived rather than as earlier bands left it.
    pre_l: [f32; CONTROL_BLOCK],
    pre_r: [f32; CONTROL_BLOCK],
    /// What the band about to be metered is listening to, one buffer per bus.
    sc_a: [f32; CONTROL_BLOCK],
    sc_b: [f32; CONTROL_BLOCK],
    /// The signal as it entered the resonance stage, for the stage-wide
    /// mix/delta blend across both suppression engines.
    res_dry_l: [f32; CONTROL_BLOCK],
    res_dry_r: [f32; CONTROL_BLOCK],
    /// Where the delta monitor stands, 0..1. The parameter is a switch; this
    /// eases toward it so flipping the monitor is a fade, not a click.
    delta_mix: f32,
    /// Smoothed output polarity. A hard +1/-1 switch can click anywhere except
    /// a zero crossing, so the button moves through zero over a few ms.
    phase_gain: f32,
}

/// Fade between the stage's normal output (dry crossfaded toward wet by
/// `mix`) and its delta monitor (what was removed), by `d`.
///
/// At `d = 1` this is exactly `dry - wet` — the same identity the bank's delta
/// always satisfied — and at `d = 0` exactly the mix blend, so the eased
/// switch lands on both endpoints bit-for-bit apart from float rounding.
#[inline]
fn blend_delta(wet: &mut [f32], dry: &[f32], mix: f32, d: f32) {
    for (out, dry) in wet.iter_mut().zip(dry) {
        let kept = dry + (*out - dry) * mix;
        let removed = dry - *out;
        *out = kept + (removed - kept) * d;
    }
}

/// Move a stereo pair between the two domains, in place.
///
/// Exactly invertible: encoding gives `(l+r)/2` and `(l-r)/2`, decoding sums and
/// differences them straight back. Four operations a sample, which is what makes
/// switching domains mid-chain cheap enough to do per band.
#[inline]
fn convert(a: &mut [f32], b: &mut [f32], n: usize, to: Domain) {
    match to {
        Domain::MidSide => {
            for i in 0..n {
                let (l, r) = (a[i], b[i]);
                a[i] = 0.5 * (l + r);
                b[i] = 0.5 * (l - r);
            }
        }
        Domain::LeftRight => {
            for i in 0..n {
                let (m, side) = (a[i], b[i]);
                a[i] = m + side;
                b[i] = m - side;
            }
        }
    }
}

impl Default for EqEngine {
    fn default() -> Self {
        Self::new(48_000.0)
    }
}

impl EqEngine {
    pub fn new(sr: f32) -> Self {
        let shared = Arc::new(SharedSpectral::default());
        shared.cfg.publish(&ConfigView {
            sample_rate: sr,
            ..ConfigView::default()
        });
        Self {
            sr,
            bands: Box::new(std::array::from_fn(|_| BandRuntime::default())),
            resonance: Box::new(ResonanceBank::new(sr)),
            overlays: Box::new(BandOverlays::none()),
            pool: Box::new(AdaptivePool::new(sr)),
            shared,
            solo_coeffs: Coeffs::identity(),
            solo_key: CoeffKey::NONE,
            solo_l: Biquad::new(),
            solo_r: Biquad::new(),
            pre_l: [0.0; CONTROL_BLOCK],
            pre_r: [0.0; CONTROL_BLOCK],
            sc_a: [0.0; CONTROL_BLOCK],
            sc_b: [0.0; CONTROL_BLOCK],
            res_dry_l: [0.0; CONTROL_BLOCK],
            res_dry_r: [0.0; CONTROL_BLOCK],
            delta_mix: 0.0,
            phase_gain: 1.0,
        }
    }

    /// The channel the spectral worker communicates over. Hand a clone to
    /// [`crate::dsp::spectral::SpectralWorker::spawn`]; the engine keeps
    /// feeding it either way.
    pub fn spectral_shared(&self) -> Arc<SharedSpectral> {
        self.shared.clone()
    }

    /// Snapshot of the adaptive pool for the UI. `out` should hold
    /// [`MAX_TARGETS`] entries.
    pub fn spectral_view(&self, out: &mut [TargetView]) {
        self.pool.view(out);
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
        self.resonance.set_sample_rate(sr);
        self.pool.set_sample_rate(sr);
        self.reset();
    }

    pub fn reset(&mut self) {
        for band in self.bands.iter_mut() {
            band.reset();
        }
        self.resonance.reset();
        self.pool.reset();
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

    /// dB of cut each resonance band is applying, for the UI.
    pub fn resonance_reduction(&self, out: &mut [f32]) {
        self.resonance.reduction(out);
    }

    /// The deepest cut the resonance stage is making anywhere — either
    /// engine's, in dB.
    pub fn resonance_peak(&self) -> f32 {
        self.resonance.peak_reduction().max(self.pool.peak_cut())
    }

    /// Process one control block in place.
    ///
    /// Pass `None` for `right` on mono input. There is no second bus then, so
    /// right- and side-only bands have nothing to act on and are skipped, while
    /// a mid-only band sees the whole signal — which is what mid means when
    /// there is only one channel.
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
                if band.ran_a || band.ran_b {
                    band.reset();
                    band.ran_a = false;
                    band.ran_b = false;
                    band.ran_domain = None;
                }
            }
            if self.resonance.peak_reduction() != 0.0 {
                self.resonance.reset();
            }
            if self.pool.busy() {
                self.pool.reset();
            }
            self.phase_gain = 1.0;
            return;
        }

        let mut right = right;
        let stereo = right.is_some();
        let sr = self.sr;

        // Snapshot the input for the sidechains before any band touches it.
        // Mono copies left into both, which makes the mid the signal and the
        // side silent — exactly what the M/S transform would give.
        self.pre_l[..n].copy_from_slice(&left[..n]);
        match right.as_deref() {
            Some(r) => self.pre_r[..n].copy_from_slice(&r[..n]),
            None => self.pre_r[..n].copy_from_slice(&left[..n]),
        }

        // --- dynamics -------------------------------------------------------
        // Detectors tap the input, before any band has touched it — the same
        // sidechain point the prototype used, and what stops one dynamic band
        // from chasing another's gain reduction.
        for slot in 0..MAX_BANDS {
            let p = s.bands[slot];
            let band = &mut self.bands[slot];

            if !p.running {
                // The band is out of the chain entirely; there is no gain for an
                // offset to sit on, so nothing has to be let down gently.
                band.delta_db = 0.0;
                band.env = 0.0;
                band.level_db = -100.0;
                band.level.reset();
                continue;
            }

            if !p.dynamic {
                // Still audible, just no longer moving. Walk any offset out at
                // the release time rather than dropping it: switching dynamics
                // off part-way through a gain reduction is one click of a button
                // and shouldn't also be one click in the audio. Bands that were
                // never dynamic — most of them, most of the time — are already
                // at rest and skip the whole thing.
                if band.env != 0.0 {
                    band.env = step_toward(band.env, 0.0, p.release, dt);
                    band.delta_db = band.env * p.dyn_range;
                } else {
                    band.delta_db = 0.0;
                }
                band.level_db = -100.0;
                band.level.reset();
                continue;
            }

            // Each band listens to the signal it actually filters. A stereo band
            // filters left and right alike, so it listens to both — folding them
            // to the mono sum first would leave an anti-phase pair silent to the
            // detector while the band was working on it at full level.
            let both_buses = matches!(p.channel, BandChannel::Stereo);
            for i in 0..n {
                let (l, r) = (self.pre_l[i], self.pre_r[i]);
                match p.channel {
                    BandChannel::Left => self.sc_a[i] = l,
                    BandChannel::Right => self.sc_a[i] = r,
                    BandChannel::Mid => self.sc_a[i] = 0.5 * (l + r),
                    BandChannel::Side => self.sc_a[i] = 0.5 * (l - r),
                    BandChannel::Stereo => {
                        self.sc_a[i] = l;
                        self.sc_b[i] = r;
                    }
                }
            }

            band.update_detector(p.kind, p.freq, p.q, sr);
            band.level.set_window(p.freq, sr);
            let c = band.det_coeffs;
            if both_buses {
                for i in 0..n {
                    let a = band.det_a.process(self.sc_a[i], &c);
                    let b = band.det_b.process(self.sc_b[i], &c);
                    band.level.push_ms(0.5 * (a * a + b * b));
                }
            } else {
                for i in 0..n {
                    band.level.push(band.det_a.process(self.sc_a[i], &c));
                }
            }
            band.level_db = band.level.level_db();

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
        // The signal starts, and must end, in left/right. In between it is
        // carried in whichever domain the band about to run needs.
        let mut domain = Domain::LeftRight;

        for slot in 0..MAX_BANDS {
            let p = s.bands[slot];
            let soloed_out = s.solo.is_some() && s.solo != Some(slot);
            let running = p.running && !soloed_out;

            let run_a = running && p.channel.uses_first_bus();
            let run_b = running && stereo && p.channel.uses_second_bus();

            if run_a || run_b {
                // A stereo band asks for no domain in particular, so it runs in
                // whichever one the chain is already in — no conversion, and the
                // same answer, since one filter on both buses commutes with the
                // transform between them.
                if let (Some(want), Some(r)) = (p.channel.domain(), right.as_deref_mut()) {
                    if domain != want {
                        convert(left, r, n, want);
                        domain = want;
                    }
                }
            }

            let band = &mut self.bands[slot];
            // Coming back from silence with stale state in the delay line is a
            // click, and so is state that describes the other domain's signal.
            let domain_changed = band.ran_domain.is_some_and(|was| was != domain);
            if (band.ran_a && !run_a) || (band.ran_b && !run_b) || domain_changed {
                band.reset_filters();
            }
            band.ran_a = run_a;
            band.ran_b = run_b;
            if !run_a && !run_b {
                band.ran_domain = None;
                continue;
            }
            band.ran_domain = Some(domain);

            band.update_coeffs(p.kind, p.order, p.freq, p.q, p.gain + band.delta_db, sr);
            if run_a {
                band.run(&mut left[..n], Bus::A);
            }
            if run_b {
                if let Some(r) = right.as_deref_mut() {
                    band.run(&mut r[..n], Bus::B);
                }
            }
        }

        // Soloing a band that acts on one bus isolates that bus, so you hear the
        // slice on its own rather than folded back into its opposite. In mono
        // there is no opposite to drop.
        if let (Some(slot), Some(r)) = (s.solo, right.as_deref_mut()) {
            if let Some(want) = s.bands[slot].channel.domain() {
                if domain != want {
                    convert(left, r, n, want);
                    domain = want;
                }
                if s.bands[slot].channel.uses_first_bus() {
                    r[..n].fill(0.0);
                } else {
                    left[..n].fill(0.0);
                }
            }
        }

        // --- back to left/right ----------------------------------------------
        if let Some(r) = right.as_deref_mut() {
            if domain != Domain::LeftRight {
                convert(left, r, n, Domain::LeftRight);
            }
        }

        // --- resonance suppression -------------------------------------------
        // Last in the chain and in left/right, because it is judging the signal
        // as it will be heard — including whatever the bands just did to it.
        // Soloing is a way of hearing one band plainly, so a suppressor chewing
        // on the thing being listened for would defeat the point.
        //
        // Two engines share the stage. The Adaptive one is the sixth-octave
        // bank; the Spectral one is the adaptive filter pool fed by the
        // background FFT worker. Both run in the time domain; both sit under
        // one stage-wide mix/delta blend, so "listen to what's removed" plays
        // the combined removal whichever engine made it.
        let spectral_global = s.resonance.enabled && s.res_mode == ResMode::Spectral;
        let adaptive_global = s.resonance.enabled && s.res_mode == ResMode::Adaptive;
        let mut any_band_adaptive = false;
        let mut any_band_spectral = false;
        for p in s.bands.iter() {
            if !p.running || p.resonance <= 0.0 {
                continue;
            }
            match p.res_mode {
                BandResMode::Adaptive => any_band_adaptive = true,
                BandResMode::Spectral => any_band_spectral = true,
                BandResMode::Off => {}
            }
        }
        let spectral_on = spectral_global || any_band_spectral;
        let bank_on = adaptive_global || any_band_adaptive;

        // The worker's marching orders go out every block — including "stand
        // down", which is what lets it idle and the pool drain when the last
        // spectral consumer is switched off.
        self.publish_spectral_config(s, spectral_global, any_band_spectral);

        // The detector listens to the signal as it enters the stage: after the
        // EQ bands (it should judge what will be heard) but before suppression
        // (or each filter would chase its own cut, let go, and grab again).
        if spectral_on {
            match right.as_deref() {
                Some(r) => {
                    for i in 0..n {
                        self.shared.ring.push(left[i], r[i]);
                    }
                }
                None => {
                    for &x in left[..n].iter() {
                        self.shared.ring.push(x, x);
                    }
                }
            }
        }

        if s.solo.is_none() && (bank_on || spectral_on || self.pool.busy()) {
            self.res_dry_l[..n].copy_from_slice(&left[..n]);
            match right.as_deref() {
                Some(r) => self.res_dry_r[..n].copy_from_slice(&r[..n]),
                None => self.res_dry_r[..n].copy_from_slice(&left[..n]),
            }

            // The bank gets the stage settings with the blend neutralised —
            // the blend belongs to the stage, not to either engine, and the
            // bank's global side only runs in Adaptive mode.
            self.plan_band_resonance(s);
            let bank_settings = ResonanceSettings {
                enabled: adaptive_global,
                mix: 1.0,
                delta: false,
                ..s.resonance
            };
            self.resonance.process(
                &mut left[..n],
                right.as_deref_mut().map(|r| &mut r[..n]),
                &bank_settings,
                &self.overlays,
            );

            let ctl = self.pool_controls(s, spectral_global);
            self.pool.update(&self.shared.frames, &ctl, dt);
            self.pool
                .process(&mut left[..n], right.as_deref_mut().map(|r| &mut r[..n]), n);

            // Stage-wide blend: delta hands back what both engines removed.
            // The switch itself is eased over a few milliseconds — flipping a
            // monitor is a listening action, and it must not click.
            self.delta_mix = step_toward(
                self.delta_mix,
                if s.resonance.delta { 1.0 } else { 0.0 },
                8.0,
                dt,
            );
            let d = self.delta_mix;
            let mix = s.resonance.mix.clamp(0.0, 1.0);
            if d > 0.0001 {
                blend_delta(&mut left[..n], &self.res_dry_l[..n], mix, d);
                if let Some(r) = right.as_deref_mut() {
                    blend_delta(&mut r[..n], &self.res_dry_r[..n], mix, d);
                }
            } else if mix < 1.0 {
                crossfade(&mut left[..n], &self.res_dry_l[..n], mix);
                if let Some(r) = right.as_deref_mut() {
                    crossfade(&mut r[..n], &self.res_dry_r[..n], mix);
                }
            }
        } else {
            // Soloed, or nothing left for the stage to do. State describing
            // whatever was playing before would come back as a burst of stale
            // suppression, so it is dropped rather than kept warm.
            if self.resonance.peak_reduction() != 0.0 {
                self.resonance.reset();
            }
            if self.pool.busy() {
                self.pool.reset();
            }
        }

        let out_gain = 10f32.powf(s.output_gain_db / 20.0);
        let phase_target = if s.phase_invert { -1.0 } else { 1.0 };
        let phase_step = 2.0 / (self.sr * 0.005).max(1.0);
        for i in 0..n {
            self.phase_gain = if self.phase_gain < phase_target {
                (self.phase_gain + phase_step).min(phase_target)
            } else {
                (self.phase_gain - phase_step).max(phase_target)
            };
            let gain = out_gain * self.phase_gain;
            left[i] *= gain;
        if let Some(r) = right.as_deref_mut() {
                r[i] *= gain;
            }
        }

        match s.solo {
            Some(slot) => {
                let plan = s.bands[slot];
                let mut right = right;
                self.apply_solo(plan, left, right.as_deref_mut(), n);
            }
            None => self.clear_solo(),
        }
    }

    /// Spread each band's own resonance amount over the bank bands it covers.
    ///
    /// A band asking for suppression is asking for it *in its own region*, so
    /// what reaches the bank is the amount shaped by that region — see
    /// [`band_region_weight`]. Overlapping bands add, because two bands pointed
    /// at the same spot both wanting it gone is not a reason to want it gone
    /// less; the sum is capped by the bank. Range, sensitivity and ballistics
    /// travel alongside, blended by each band's share of the depth.
    fn plan_band_resonance(&mut self, s: &Settings) {
        let any = s
            .bands
            .iter()
            .any(|p| p.running && p.resonance > 0.0 && p.res_mode == BandResMode::Adaptive);
        if !any {
            // Nothing to spread, and the bank reads this to decide whether it
            // has any reason to run at all.
            self.overlays.clear();
            return;
        }
        let o = &mut self.overlays;
        for i in 0..RES_BANDS {
            let freq = band_freq(i);
            let mut depth = 0.0f32;
            let mut cap = 0.0f32;
            let mut sens = 0.0f32;
            let mut attack = 0.0f32;
            let mut release = 0.0f32;
            for p in s.bands.iter() {
                if !p.running || p.resonance <= 0.0 || p.res_mode != BandResMode::Adaptive {
                    continue;
                }
                let w = p.resonance * band_region_weight(p.kind, p.freq, p.q, freq);
                if w <= 0.0 {
                    continue;
                }
                depth += w;
                cap = cap.max(p.res_range);
                sens += w * p.res_sens;
                attack += w * p.res_attack;
                release += w * p.res_release;
            }
            o.depth[i] = depth;
            o.cap_db[i] = cap;
            if depth > 0.0 {
                o.sens_db[i] = sens / depth;
                o.attack_ms[i] = attack / depth;
                o.release_ms[i] = release / depth;
            } else {
                o.sens_db[i] = 0.0;
                o.attack_ms[i] = 0.0;
                o.release_ms[i] = 0.0;
            }
        }
    }

    /// The spectral worker's marching orders for this block: which regions to
    /// search, on which channels, how eagerly. Plain relaxed stores.
    fn publish_spectral_config(&self, s: &Settings, global_on: bool, any_band: bool) {
        let mut cfg = ConfigView {
            sample_rate: self.sr,
            global_on,
            quality: s.res_quality as u32,
            threshold_db: s.resonance.threshold_db,
            selectivity: s.resonance.sharpness,
            low_hz: s.resonance.low_hz,
            high_hz: s.resonance.high_hz,
            bands: [BandRegionView::default(); MAX_BANDS],
        };
        if any_band {
            for (slot, p) in s.bands.iter().enumerate() {
                if !p.running || p.resonance <= 0.0 || p.res_mode != BandResMode::Spectral {
                    continue;
                }
                cfg.bands[slot] = BandRegionView {
                    active: true,
                    channel: p.channel as u8,
                    freq: p.freq,
                    width_oct: p.res_width,
                    sens_db: p.res_sens,
                };
            }
        }
        self.shared.cfg.publish(&cfg);
    }

    /// How the pool should treat each target it holds this block.
    fn pool_controls(&self, s: &Settings, global_on: bool) -> PoolControls {
        let mut ctl = PoolControls {
            global_on,
            amount: s.resonance.depth,
            range_db: s.resonance.range_db,
            attack_ms: s.resonance.attack_ms,
            release_ms: s.resonance.release_ms,
            max_slots: s.res_quality.max_targets().min(MAX_TARGETS),
            tuning: PoolTuning::for_quality(s.res_quality),
            bands: [BandCtl::default(); MAX_BANDS],
        };
        for (slot, p) in s.bands.iter().enumerate() {
            if !p.running || p.res_mode != BandResMode::Spectral {
                continue;
            }
            ctl.bands[slot] = BandCtl {
                active: p.resonance > 0.0,
                amount: p.resonance,
                range_db: p.res_range,
                attack_ms: p.res_attack,
                release_ms: p.res_release,
            };
        }
        ctl
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

    fn bell_coeffs(freq: f32, q: f32, gain: f32) -> Coeffs {
        let mut sections = [Coeffs::identity(); MAX_SECTIONS];
        let n = band_cascade(BandKind::Bell, freq, SR, 2, q, gain, &mut sections);
        assert_eq!(n, 1, "a bell is one section");
        sections[0]
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
    fn phase_invert_reverses_both_channels_without_changing_level() {
        let mut engine = EqEngine::new(SR);
        let s = Settings {
            phase_invert: true,
            ..Settings::default()
        };
        let mut l = [0.25; CONTROL_BLOCK];
        let mut r = [-0.5; CONTROL_BLOCK];

        // Let the click-safe polarity ramp reach its destination.
        for _ in 0..12 {
            l.fill(0.25);
            r.fill(-0.5);
            engine.process_block(&mut l, Some(&mut r), &s);
        }

        assert!((l[CONTROL_BLOCK - 1] + 0.25).abs() < 1e-6);
        assert!((r[CONTROL_BLOCK - 1] - 0.5).abs() < 1e-6);
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
        for slope in [Slope::S6, Slope::S12, Slope::S24, Slope::S36, Slope::S48] {
            let s = one_band(BandPlan {
                running: true,
                kind: BandKind::LowCut,
                freq: 1000.0,
                order: slope.order(),
                ..BandPlan::default()
            });
            let mut engine = EqEngine::new(SR);
            let out = rms_db(&mut engine, &s, 500.0, true);
            // One octave down, each step of the list is a further 6 dB/oct of
            // asymptote. Except the first, which is still inside the knee.
            assert!(
                out < last - 4.5,
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
        let c = bell_coeffs(800.0, 2.0, 9.0);
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

    /// Feed uncorrelated tones into L and R and return both channels of the tail.
    fn run_stereo(engine: &mut EqEngine, s: &Settings, blocks: usize) -> (Vec<f32>, Vec<f32>) {
        let (mut out_l, mut out_r) = (Vec::new(), Vec::new());
        for block in 0..blocks {
            let mut l = [0.0f32; CONTROL_BLOCK];
            let mut r = [0.0f32; CONTROL_BLOCK];
            for i in 0..CONTROL_BLOCK {
                let t = (block * CONTROL_BLOCK + i) as f32 / SR;
                l[i] = (2.0 * PI * 1000.0 * t).sin();
                r[i] = (2.0 * PI * 300.0 * t).sin() * 0.7;
            }
            engine.process_block(&mut l, Some(&mut r), s);
            out_l.extend_from_slice(&l);
            out_r.extend_from_slice(&r);
        }
        (out_l, out_r)
    }

    #[test]
    fn a_left_band_touches_only_the_left_channel() {
        let mut engine = EqEngine::new(SR);
        let s = one_band(BandPlan {
            channel: BandChannel::Left,
            ..bell(1000.0, 12.0, 1.0)
        });

        let (out_l, out_r) = run_stereo(&mut engine, &s, 64);

        // Right is bit-identical to the untouched input.
        for (i, sample) in out_r.iter().enumerate() {
            let t = i as f32 / SR;
            let expected = (2.0 * PI * 300.0 * t).sin() * 0.7;
            assert!(
                (sample - expected).abs() < 1e-5,
                "right channel moved at {i}: {sample} vs {expected}"
            );
        }
        // Left is boosted at the band's centre, so it must be visibly bigger.
        let peak = out_l[out_l.len() / 2..]
            .iter()
            .fold(0.0f32, |acc, x| acc.max(x.abs()));
        assert!(peak > 3.0, "left was not boosted, peak {peak}");
    }

    #[test]
    fn a_right_band_touches_only_the_right_channel() {
        let mut engine = EqEngine::new(SR);
        let s = one_band(BandPlan {
            channel: BandChannel::Right,
            ..bell(300.0, -24.0, 1.0)
        });

        let (out_l, out_r) = run_stereo(&mut engine, &s, 64);

        for (i, sample) in out_l.iter().enumerate() {
            let t = i as f32 / SR;
            let expected = (2.0 * PI * 1000.0 * t).sin();
            assert!(
                (sample - expected).abs() < 1e-5,
                "left channel moved at {i}: {sample} vs {expected}"
            );
        }
        // Right's only content sits at the band's centre and was cut hard.
        let peak = out_r[out_r.len() / 2..]
            .iter()
            .fold(0.0f32, |acc, x| acc.max(x.abs()));
        assert!(peak < 0.15, "right was not cut, peak {peak}");
    }

    /// The chain has to hop domains when a left band is followed by a mid band.
    /// This is the whole reason the engine no longer lives in one domain, so it
    /// is checked against the transform written out by hand.
    #[test]
    fn a_left_band_followed_by_a_mid_band_matches_the_transform_by_hand() {
        let mut s = Settings::default();
        s.bands[0] = BandPlan {
            channel: BandChannel::Left,
            ..bell(1500.0, 8.0, 1.5)
        };
        s.bands[1] = BandPlan {
            channel: BandChannel::Mid,
            ..bell(400.0, -7.0, 0.9)
        };

        let left_coeffs = bell_coeffs(1500.0, 1.5, 8.0);
        let mid_coeffs = bell_coeffs(400.0, 0.9, -7.0);
        let mut left_filter = Biquad::new();
        let mut mid_filter = Biquad::new();

        let mut engine = EqEngine::new(SR);
        for block in 0..64 {
            let mut l = [0.0f32; CONTROL_BLOCK];
            let mut r = [0.0f32; CONTROL_BLOCK];
            let mut want_l = [0.0f32; CONTROL_BLOCK];
            let mut want_r = [0.0f32; CONTROL_BLOCK];

            for i in 0..CONTROL_BLOCK {
                let t = (block * CONTROL_BLOCK + i) as f32 / SR;
                l[i] = (2.0 * PI * 1000.0 * t).sin();
                r[i] = (2.0 * PI * 300.0 * t).sin() * 0.7;

                // Left band first, on left alone...
                let filtered_l = left_filter.process(l[i], &left_coeffs);
                // ...then encode, filter the mid, and decode.
                let mid = 0.5 * (filtered_l + r[i]);
                let side = 0.5 * (filtered_l - r[i]);
                let filtered_mid = mid_filter.process(mid, &mid_coeffs);
                want_l[i] = filtered_mid + side;
                want_r[i] = filtered_mid - side;
            }

            engine.process_block(&mut l, Some(&mut r), &s);
            for i in 0..CONTROL_BLOCK {
                assert!(
                    (l[i] - want_l[i]).abs() < 1e-4 && (r[i] - want_r[i]).abs() < 1e-4,
                    "block {block} sample {i}: ({}, {}) vs ({}, {})",
                    l[i],
                    r[i],
                    want_l[i],
                    want_r[i]
                );
            }
        }
    }

    #[test]
    fn the_chain_ends_in_left_right_however_many_domain_hops_it_took() {
        // Alternating channels forces a conversion before nearly every band.
        let mut s = Settings::default();
        let channels = [
            BandChannel::Left,
            BandChannel::Mid,
            BandChannel::Right,
            BandChannel::Side,
            BandChannel::Stereo,
            BandChannel::Left,
        ];
        for (slot, channel) in channels.into_iter().enumerate() {
            s.bands[slot] = BandPlan {
                channel,
                ..bell(200.0 * (slot + 1) as f32, 0.0, 1.0)
            };
        }

        // Every band is at 0 dB, so whatever route the signal took through the
        // domains, it has to come out exactly as it went in.
        let mut engine = EqEngine::new(SR);
        let (out_l, out_r) = run_stereo(&mut engine, &s, 32);
        for (i, (&got_l, &got_r)) in out_l.iter().zip(out_r.iter()).enumerate() {
            let t = i as f32 / SR;
            let want_l = (2.0 * PI * 1000.0 * t).sin();
            let want_r = (2.0 * PI * 300.0 * t).sin() * 0.7;
            assert!(
                (got_l - want_l).abs() < 1e-4 && (got_r - want_r).abs() < 1e-4,
                "sample {i}: ({got_l}, {got_r}) vs ({want_l}, {want_r})"
            );
        }
    }

    #[test]
    fn soloing_a_left_band_silences_the_right_channel() {
        let mut s = one_band(BandPlan {
            channel: BandChannel::Left,
            ..bell(1000.0, 0.0, 1.0)
        });
        s.solo = Some(0);

        let mut engine = EqEngine::new(SR);
        let (out_l, out_r) = run_stereo(&mut engine, &s, 64);

        let tail = out_r.len() / 2;
        assert!(
            out_r[tail..].iter().all(|x| x.abs() < 1e-6),
            "right channel survived a left solo"
        );
        // And the left channel still carries its own region.
        let peak = out_l[tail..].iter().fold(0.0f32, |acc, x| acc.max(x.abs()));
        assert!(peak > 0.3, "soloed left band passed nothing, peak {peak}");
    }

    #[test]
    fn soloing_a_right_band_silences_the_left_channel() {
        let mut s = one_band(BandPlan {
            channel: BandChannel::Right,
            ..bell(300.0, 0.0, 1.0)
        });
        s.solo = Some(0);

        let mut engine = EqEngine::new(SR);
        let (out_l, _) = run_stereo(&mut engine, &s, 64);
        let tail = out_l.len() / 2;
        assert!(
            out_l[tail..].iter().all(|x| x.abs() < 1e-6),
            "left channel survived a right solo"
        );
    }

    #[test]
    fn mono_applies_a_left_band_and_ignores_a_right_one() {
        let left_only = one_band(BandPlan {
            channel: BandChannel::Left,
            ..bell(1000.0, 6.0, 1.0)
        });
        let mut engine = EqEngine::new(SR);
        let out = rms_db(&mut engine, &left_only, 1000.0, false);
        assert!(
            (out - (SINE_RMS_DB + 6.0)).abs() < 0.1,
            "left band on mono gave {out} dB"
        );

        // There is no right channel to act on, so the band has nothing to do.
        let right_only = one_band(BandPlan {
            channel: BandChannel::Right,
            ..bell(1000.0, 6.0, 1.0)
        });
        let mut engine = EqEngine::new(SR);
        let out = rms_db(&mut engine, &right_only, 1000.0, false);
        assert!(
            (out - SINE_RMS_DB).abs() < 0.01,
            "right band touched mono material: {out} dB"
        );
    }

    #[test]
    fn a_dynamic_left_band_listens_to_the_left_channel_alone() {
        // Loud on the left, near-silent on the right, with the threshold between.
        let mut s = Settings::default();
        s.bands[0] = BandPlan {
            channel: BandChannel::Left,
            dynamic: true,
            dyn_range: -12.0,
            threshold: -30.0,
            attack: 1.0,
            release: 10.0,
            ..bell(1000.0, 0.0, 1.0)
        };
        s.bands[1] = BandPlan {
            channel: BandChannel::Right,
            dynamic: true,
            dyn_range: -12.0,
            threshold: -30.0,
            attack: 1.0,
            release: 10.0,
            ..bell(1000.0, 0.0, 1.0)
        };

        let mut engine = EqEngine::new(SR);
        for block in 0..400 {
            let mut l = [0.0f32; CONTROL_BLOCK];
            let mut r = [0.0f32; CONTROL_BLOCK];
            for i in 0..CONTROL_BLOCK {
                let t = (block * CONTROL_BLOCK + i) as f32 / SR;
                let x = (2.0 * PI * 1000.0 * t).sin();
                l[i] = x;
                r[i] = x * 0.001; // -60 dB
            }
            engine.process_block(&mut l, Some(&mut r), &s);
        }

        assert!(
            engine.meter(0).delta_db < -11.0,
            "left band did not engage: {}",
            engine.meter(0).delta_db
        );
        assert!(
            engine.meter(1).delta_db.abs() < 0.01,
            "right band engaged on a quiet channel: {}",
            engine.meter(1).delta_db
        );
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

    /// Feed a mono tone at `freq` for `blocks` blocks and report the min and max
    /// gain offset the band settled on over the second half of the run.
    fn delta_swing(engine: &mut EqEngine, s: &Settings, freq: f32, blocks: usize) -> (f32, f32) {
        let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
        for block in 0..blocks {
            let mut l = [0.0f32; CONTROL_BLOCK];
            let mut r = [0.0f32; CONTROL_BLOCK];
            for i in 0..CONTROL_BLOCK {
                let t = (block * CONTROL_BLOCK + i) as f32 / SR;
                let x = (2.0 * PI * freq * t).sin();
                l[i] = x;
                r[i] = x;
            }
            engine.process_block(&mut l, Some(&mut r), s);
            if block >= blocks / 2 {
                let d = engine.meter(0).delta_db;
                lo = lo.min(d);
                hi = hi.max(d);
            }
        }
        (lo, hi)
    }

    /// A 50 Hz cycle is 960 samples — thirty control blocks. A level taken one
    /// block at a time is therefore a reading of the waveform, and a band with a
    /// fast attack follows it: the gain modulates at the tone's own rate, which
    /// is distortion rather than dynamics. The threshold sits half a knee below
    /// the tone so a wobble in the level would show up undiluted by the clamp.
    #[test]
    fn a_low_band_holds_a_steady_offset_on_a_steady_tone() {
        let mut engine = EqEngine::new(SR);
        let s = one_band(BandPlan {
            dynamic: true,
            dyn_range: -12.0,
            threshold: SINE_RMS_DB - crate::params::DYN_KNEE_DB / 2.0,
            attack: 1.0,
            release: 10.0,
            ..bell(50.0, 0.0, 1.0)
        });

        let (lo, hi) = delta_swing(&mut engine, &s, 50.0, 800);
        // Half a knee in is half the range.
        assert!(
            (lo + 6.0).abs() < 1.5 && (hi + 6.0).abs() < 1.5,
            "offset settled at {lo}..{hi} dB, expected about -6"
        );
        assert!(hi - lo < 1.5, "the offset wobbled over {} dB", hi - lo);
    }

    /// The same at the top of the range, where the old block-at-a-time reading
    /// happened to work — the fix must not have cost anything up here.
    #[test]
    fn a_high_band_holds_a_steady_offset_too() {
        let mut engine = EqEngine::new(SR);
        let s = one_band(BandPlan {
            dynamic: true,
            dyn_range: -12.0,
            threshold: SINE_RMS_DB - crate::params::DYN_KNEE_DB / 2.0,
            attack: 1.0,
            release: 10.0,
            ..bell(6000.0, 0.0, 1.0)
        });

        let (lo, hi) = delta_swing(&mut engine, &s, 6000.0, 400);
        assert!(
            (lo + 6.0).abs() < 1.0 && (hi + 6.0).abs() < 1.0,
            "offset settled at {lo}..{hi} dB, expected about -6"
        );
        assert!(hi - lo < 1.0, "the offset wobbled over {} dB", hi - lo);
    }

    /// A stereo band filters both channels, so it has to hear both. An
    /// anti-phase pair is silent summed to mono and full scale on each side.
    #[test]
    fn a_dynamic_stereo_band_hears_an_anti_phase_pair() {
        let mut engine = EqEngine::new(SR);
        let s = one_band(BandPlan {
            channel: BandChannel::Stereo,
            dynamic: true,
            dyn_range: -12.0,
            threshold: -30.0,
            attack: 1.0,
            release: 10.0,
            ..bell(1000.0, 0.0, 1.0)
        });

        for block in 0..400 {
            let mut l = [0.0f32; CONTROL_BLOCK];
            let mut r = [0.0f32; CONTROL_BLOCK];
            for i in 0..CONTROL_BLOCK {
                let t = (block * CONTROL_BLOCK + i) as f32 / SR;
                let x = (2.0 * PI * 1000.0 * t).sin();
                l[i] = x;
                r[i] = -x;
            }
            engine.process_block(&mut l, Some(&mut r), &s);
        }

        assert!(
            engine.meter(0).delta_db < -11.0,
            "an anti-phase pair left the band asleep: {}",
            engine.meter(0).delta_db
        );
    }

    /// Turning dynamics off is a button press, not a cue for a step in the gain.
    #[test]
    fn switching_dynamics_off_walks_the_offset_out() {
        let engaged = BandPlan {
            dynamic: true,
            dyn_range: -12.0,
            threshold: -30.0,
            attack: 1.0,
            release: 60.0,
            ..bell(1000.0, 0.0, 1.0)
        };
        let mut s = one_band(engaged);

        let mut engine = EqEngine::new(SR);
        delta_swing(&mut engine, &s, 1000.0, 200);
        assert!(engine.meter(0).delta_db < -11.0, "the band never engaged");

        // One block after the switch the offset must still be nearly all there.
        s.bands[0].dynamic = false;
        delta_swing(&mut engine, &s, 1000.0, 2);
        let just_after = engine.meter(0).delta_db;
        assert!(
            just_after < -10.0,
            "the offset dropped to {just_after} dB in a block"
        );

        // And several releases later it is gone, exactly rather than nearly —
        // an envelope left trailing off is an envelope decaying into denormals.
        delta_swing(&mut engine, &s, 1000.0, 2000);
        assert_eq!(engine.meter(0).delta_db, 0.0, "the offset never let go");
    }

    /// A routing change drops filter state that describes the wrong signal. It
    /// must not also drop the sidechain's, which taps the input either way.
    #[test]
    fn a_routing_change_keeps_the_dynamics_engaged() {
        let mut s = one_band(BandPlan {
            channel: BandChannel::Mid,
            dynamic: true,
            dyn_range: -12.0,
            threshold: -30.0,
            attack: 1.0,
            release: 400.0,
            ..bell(1000.0, 0.0, 1.0)
        });

        let mut engine = EqEngine::new(SR);
        delta_swing(&mut engine, &s, 1000.0, 200);
        assert!(engine.meter(0).delta_db < -11.0, "the band never engaged");

        // Mid to left: a different domain, so the filter state goes.
        s.bands[0].channel = BandChannel::Left;
        delta_swing(&mut engine, &s, 1000.0, 2);
        assert!(
            engine.meter(0).delta_db < -11.0,
            "the routing change reset the envelope: {}",
            engine.meter(0).delta_db
        );
    }

    /// A surgical band is the one that most wants dynamics, so its detector may
    /// not be so narrow it hears nothing.
    #[test]
    fn a_very_narrow_band_still_engages() {
        let mut engine = EqEngine::new(SR);
        let s = one_band(BandPlan {
            dynamic: true,
            dyn_range: -12.0,
            threshold: -30.0,
            attack: 1.0,
            release: 10.0,
            ..bell(1000.0, 0.0, 30.0)
        });

        delta_swing(&mut engine, &s, 1000.0, 400);
        assert!(
            engine.meter(0).delta_db < -11.0,
            "a Q of 30 left the detector deaf: {}",
            engine.meter(0).delta_db
        );
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

    fn suppressing() -> ResonanceSettings {
        ResonanceSettings {
            enabled: true,
            depth: 1.0,
            threshold_db: 3.0,
            attack_ms: 2.0,
            release_ms: 20.0,
            ..ResonanceSettings::default()
        }
    }

    #[test]
    fn the_resonance_stage_runs_in_the_chain() {
        let mut engine = EqEngine::new(SR);
        let s = Settings {
            resonance: suppressing(),
            ..Settings::default()
        };
        let out = rms_db(&mut engine, &s, 1000.0, true);
        assert!(
            out < SINE_RMS_DB - 8.0,
            "a lone tone came through the stage at {out} dB"
        );
        assert!(engine.resonance_peak() > 8.0);
    }

    /// Soloing is a way of hearing one band plainly. A suppressor working on
    /// exactly the thing being listened for would defeat the point of it.
    #[test]
    fn soloing_stands_the_resonance_stage_down() {
        let mut s = one_band(bell(1000.0, 0.0, 1.0));
        s.resonance = suppressing();
        s.solo = Some(0);

        let mut engine = EqEngine::new(SR);
        let out = rms_db(&mut engine, &s, 1000.0, true);
        assert_eq!(engine.resonance_peak(), 0.0);
        assert!(
            out > SINE_RMS_DB - 3.0,
            "the soloed band was suppressed anyway, at {out} dB"
        );
    }

    /// A band's own resonance amount works with the global stage switched off,
    /// and only where the band is.
    #[test]
    fn a_band_resonance_amount_works_on_its_own_region() {
        let mut s = one_band(BandPlan {
            resonance: 1.0,
            ..bell(1000.0, 0.0, 2.0)
        });
        assert!(!s.resonance.enabled, "the global stage should be off here");

        let mut engine = EqEngine::new(SR);
        let out = rms_db(&mut engine, &s, 1000.0, true);
        assert!(
            out < SINE_RMS_DB - 8.0,
            "the band's own amount did nothing: {out} dB"
        );
        assert!(engine.resonance_peak() > 8.0);

        // Move the band two and a half octaves down and the tone is left alone.
        s.bands[0].freq = 180.0;
        let mut engine = EqEngine::new(SR);
        let out = rms_db(&mut engine, &s, 1000.0, true);
        assert!(
            (out - SINE_RMS_DB).abs() < 1.0,
            "a 180 Hz band reached a 1 kHz tone: {out} dB"
        );

        // And with the amount back at zero, nothing runs at all.
        s.bands[0].freq = 1000.0;
        s.bands[0].resonance = 0.0;
        let mut engine = EqEngine::new(SR);
        let out = rms_db(&mut engine, &s, 1000.0, true);
        assert_eq!(engine.resonance_peak(), 0.0);
        assert!((out - SINE_RMS_DB).abs() < 0.01, "got {out} dB");
    }

    #[test]
    fn bypass_stands_the_resonance_stage_down_too() {
        let mut s = Settings {
            resonance: suppressing(),
            ..Settings::default()
        };
        let mut engine = EqEngine::new(SR);
        rms_db(&mut engine, &s, 1000.0, true);
        assert!(engine.resonance_peak() > 8.0);

        s.bypass = true;
        let out = rms_db(&mut engine, &s, 1000.0, true);
        assert_eq!(engine.resonance_peak(), 0.0);
        assert!((out - SINE_RMS_DB).abs() < 0.02, "got {out} dB");
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
            order: Slope::S48.order(),
            ..BandPlan::default()
        };
        s.bands[23] = BandPlan {
            running: true,
            kind: BandKind::HighCut,
            freq: 18_000.0,
            order: Slope::S48.order(),
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

    // --- the spectral engine, end to end ---------------------------------

    use crate::dsp::spectral::{Detector, TargetFrame, TargetView, WireTarget};
    use crate::params::BandResMode;

    /// Run the engine with the detector driven synchronously — what the worker
    /// thread does, minus the thread, so the test is deterministic.
    fn run_spectral(
        engine: &mut EqEngine,
        detector: &mut Detector,
        s: &Settings,
        freq: f32,
        blocks: usize,
    ) -> f32 {
        let shared = engine.spectral_shared();
        let mut sum = 0.0f64;
        let mut counted = 0usize;
        let mut fed = 0usize;
        for block in 0..blocks {
            let mut l = [0.0f32; CONTROL_BLOCK];
            let mut r = [0.0f32; CONTROL_BLOCK];
            for i in 0..CONTROL_BLOCK {
                let t = (block * CONTROL_BLOCK + i) as f64 / SR as f64;
                let x = (2.0 * std::f64::consts::PI * freq as f64 * t).sin() as f32;
                l[i] = x;
                r[i] = x;
            }
            engine.process_block(&mut l, Some(&mut r), s);
            fed += CONTROL_BLOCK;
            if fed >= detector.hop {
                fed = 0;
                detector.analyze(&shared);
            }
            if block >= blocks / 2 {
                for x in l {
                    sum += (x * x) as f64;
                    counted += 1;
                }
            }
        }
        20.0 * ((sum / counted as f64).sqrt().max(1e-9) as f32).log10()
    }

    fn spectral_settings() -> Settings {
        Settings {
            resonance: ResonanceSettings {
                enabled: true,
                depth: 1.0,
                threshold_db: 6.0,
                attack_ms: 2.0,
                release_ms: 20.0,
                range_db: 12.0,
                ..ResonanceSettings::default()
            },
            res_mode: ResMode::Spectral,
            // High: the tests measure the mechanism at the user's Range, so
            // they run the tier whose safety cap sits above it. Ultra's
            // conservative live caps get their own assertions.
            res_quality: crate::params::ResQuality::High,
            ..Settings::default()
        }
    }

    /// Global Spectral mode: the detector finds the tone, the pool cuts it,
    /// and the cut respects the Range ceiling.
    #[test]
    fn global_spectral_mode_suppresses_a_tone() {
        let mut engine = EqEngine::new(SR);
        let mut detector = Detector::new(SR);
        let s = spectral_settings();

        let out = run_spectral(&mut engine, &mut detector, &s, 1000.0, 1200);
        assert!(
            (out - (SINE_RMS_DB - 12.0)).abs() < 1.5,
            "expected the ~12 dB Range cap of cut, got {out} dB"
        );
        assert!(engine.resonance_peak() > 10.0);

        // The bank stayed out of it — this was the pool.
        let mut curve = [0.0f32; RES_BANDS];
        engine.resonance_reduction(&mut curve);
        assert!(
            curve.iter().all(|c| *c < 0.1),
            "the bank ran in Spectral mode"
        );
    }

    /// A band in Spectral mode tracks a resonance inside its search region —
    /// including one well off the band centre — and leaves material outside
    /// the region alone.
    #[test]
    fn a_spectral_band_tracks_inside_its_region() {
        let band = BandPlan {
            resonance: 1.0,
            res_mode: BandResMode::Spectral,
            res_range: 9.0,
            res_width: 1.0,
            res_attack: 2.0,
            res_release: 20.0,
            ..bell(3200.0, 0.0, 1.0)
        };
        let s = one_band(band);
        assert!(!s.resonance.enabled, "the global stage should be off here");

        // 3.84 kHz sits inside the ±1 octave region around 3.2 kHz.
        let mut engine = EqEngine::new(SR);
        let mut detector = Detector::new(SR);
        let out = run_spectral(&mut engine, &mut detector, &s, 3840.0, 1200);
        assert!(
            (out - (SINE_RMS_DB - 9.0)).abs() < 1.5,
            "expected the band Range of 9 dB of cut, got {out} dB"
        );

        // Confirm the filter went to the resonance, not the band centre.
        let mut views = [TargetView::default(); MAX_TARGETS];
        engine.spectral_view(&mut views);
        let active: Vec<&TargetView> = views.iter().filter(|v| v.is_active()).collect();
        assert_eq!(active.len(), 1, "expected one active target");
        assert!(
            (active[0].freq - 3840.0).abs() < 120.0,
            "the filter sat at {} Hz",
            active[0].freq
        );

        // A tone three octaves below the region comes through untouched.
        let mut engine = EqEngine::new(SR);
        let mut detector = Detector::new(SR);
        let out = run_spectral(&mut engine, &mut detector, &s, 400.0, 800);
        assert!(
            (out - SINE_RMS_DB).abs() < 0.5,
            "material outside the search region moved: {out} dB"
        );
    }

    /// The Ultra-mode claim, tested the way the bank tests it: no sample of
    /// output changes before an impulse arrives (no lookahead), and the
    /// impulse reaches its own sample (no delay). The pool holds a live,
    /// deep target while it happens.
    #[test]
    fn the_spectral_path_neither_delays_nor_looks_ahead() {
        const AT: usize = 5;
        let s = spectral_settings();

        let make = || {
            let engine = EqEngine::new(SR);
            // A target planted by hand, standing in for the worker.
            let mut frame = TargetFrame {
                serial: 1,
                count: 1,
                ..TargetFrame::default()
            };
            frame.targets[0] = WireTarget {
                track: 1,
                freq: 1000.0,
                q: 8.0,
                excess_db: 12.0,
                confidence: 1.0,
                owner: -1,
                channel: 0,
            };
            let mut back = 0u8;
            engine.spectral_shared().frames.publish(&mut back, frame);
            engine
        };
        let settle = |engine: &mut EqEngine| {
            for block in 0..400 {
                let mut l = [0.0f32; CONTROL_BLOCK];
                for (i, x) in l.iter_mut().enumerate() {
                    let t = (block * CONTROL_BLOCK + i) as f64 / SR as f64;
                    *x = (2.0 * std::f64::consts::PI * 1000.0 * t).sin() as f32;
                }
                engine.process_block(&mut l, None, &s);
            }
        };

        let mut plain = make();
        let mut poked = make();
        settle(&mut plain);
        settle(&mut poked);
        assert!(plain.resonance_peak() > 8.0, "the pool never engaged");

        let mut a = [0.0f32; CONTROL_BLOCK];
        for (i, x) in a.iter_mut().enumerate() {
            let t = (400 * CONTROL_BLOCK + i) as f64 / SR as f64;
            *x = (2.0 * std::f64::consts::PI * 1000.0 * t).sin() as f32;
        }
        let mut b = a;
        b[AT] += 1.0;

        plain.process_block(&mut a, None, &s);
        poked.process_block(&mut b, None, &s);

        for i in 0..AT {
            assert_eq!(a[i], b[i], "sample {i} moved before the impulse reached it");
        }
        assert!(
            (b[AT] - a[AT]).abs() > 0.05,
            "the impulse did not reach its own sample: {} against {}",
            b[AT],
            a[AT]
        );
    }

    /// Delta over the whole stage: what is kept plus what is removed must be
    /// exactly what went in, with the spectral pool doing the removing.
    #[test]
    fn delta_covers_the_spectral_cut() {
        let s = spectral_settings();
        let delta = Settings {
            resonance: ResonanceSettings {
                delta: true,
                ..s.resonance
            },
            ..s
        };

        let plant = |engine: &EqEngine| {
            let mut frame = TargetFrame {
                serial: 1,
                count: 1,
                ..TargetFrame::default()
            };
            frame.targets[0] = WireTarget {
                track: 1,
                freq: 700.0,
                q: 8.0,
                excess_db: 10.0,
                confidence: 1.0,
                owner: -1,
                channel: 0,
            };
            let mut back = 0u8;
            engine.spectral_shared().frames.publish(&mut back, frame);
        };

        let mut kept = EqEngine::new(SR);
        let mut removed = EqEngine::new(SR);
        plant(&kept);
        plant(&removed);

        for block in 0..400 {
            let mut a = [0.0f32; CONTROL_BLOCK];
            for (i, x) in a.iter_mut().enumerate() {
                let t = (block * CONTROL_BLOCK + i) as f64 / SR as f64;
                *x = (2.0 * std::f64::consts::PI * 700.0 * t).sin() as f32;
            }
            let mut b = a;
            let dry = a;
            kept.process_block(&mut a, None, &s);
            removed.process_block(&mut b, None, &delta);
            if block > 200 {
                for i in 0..CONTROL_BLOCK {
                    assert!(
                        (a[i] + b[i] - dry[i]).abs() < 1e-4,
                        "block {block} sample {i}: {} + {} != {}",
                        a[i],
                        b[i],
                        dry[i]
                    );
                }
            }
        }
        assert!(removed.resonance_peak() > 6.0, "nothing was being removed");
    }

    /// Ultra is the live tier: however hard the user turns Range up, the
    /// global scan's automatic cuts stay conservative — deep, narrow, moving
    /// notches are exactly what a live audience notices.
    #[test]
    fn ultra_quality_keeps_automatic_cuts_conservative() {
        let mut engine = EqEngine::new(SR);
        let mut detector = Detector::new(SR);
        let mut s = spectral_settings();
        s.res_quality = crate::params::ResQuality::Ultra;
        s.resonance.range_db = 36.0;

        let out = run_spectral(&mut engine, &mut detector, &s, 1000.0, 2400);
        assert!(
            (out - (SINE_RMS_DB - 6.0)).abs() < 1.0,
            "Ultra should cap the automatic cut at 6 dB, got {out} dB"
        );
        assert!(engine.resonance_peak() < 6.5);

        // And the filters live inside the tier's Q corridor.
        let mut views = [TargetView::default(); MAX_TARGETS];
        engine.spectral_view(&mut views);
        for v in views.iter().filter(|v| v.is_active()) {
            assert!(
                (1.4..=10.1).contains(&v.q),
                "an Ultra filter ran at Q {}",
                v.q
            );
        }
    }

    /// How long the detector takes from a resonance appearing to the filter
    /// actually biting — the *reaction time*, which is a different quantity
    /// from the audio path's latency (0 samples) and is reported separately
    /// on purpose.
    #[test]
    fn detector_reaction_time_is_bounded() {
        let mut engine = EqEngine::new(SR);
        let mut detector = Detector::new(SR);
        let s = spectral_settings();
        let shared = engine.spectral_shared();

        let mut at = 0usize;
        let mut fed = 0usize;
        let mut onset = None;
        let mut reacted = None;
        // Half a second of silence, then the tone starts.
        let silence = (SR * 0.5) as usize;
        for block in 0..3000 {
            let mut l = [0.0f32; CONTROL_BLOCK];
            for (i, x) in l.iter_mut().enumerate() {
                let n = block * CONTROL_BLOCK + i;
                *x = if n >= silence {
                    (2.0 * std::f64::consts::PI * 1000.0 * at as f64 / SR as f64).sin() as f32
                } else {
                    0.0
                };
                if n >= silence {
                    at += 1;
                }
            }
            engine.process_block(&mut l, None, &s);
            fed += CONTROL_BLOCK;
            if fed >= detector.hop {
                fed = 0;
                detector.analyze(&shared);
            }
            let n = (block + 1) * CONTROL_BLOCK;
            if n >= silence && onset.is_none() {
                onset = Some(n);
            }
            if onset.is_some() && reacted.is_none() && engine.resonance_peak() > 1.0 {
                reacted = Some(n);
            }
        }

        let (onset, reacted) = (onset.unwrap(), reacted.expect("never engaged"));
        let ms = (reacted - onset) as f32 / SR * 1000.0;
        println!("detector reaction: {ms:.0} ms from onset to >1 dB of cut");
        // Window (43 ms) + confidence build + attack: comfortably under a
        // quarter second, and it must not be instant either — instant would
        // mean the hysteresis was bypassed.
        assert!((30.0..250.0).contains(&ms), "reaction time was {ms} ms");
    }

    /// Flipping the delta monitor mid-cut must fade, not click: no sample
    /// step in the output beyond what the tone itself moves per sample.
    #[test]
    fn toggling_delta_is_click_safe() {
        let mut on = spectral_settings();
        let mut engine = EqEngine::new(SR);

        let mut frame = TargetFrame {
            serial: 1,
            count: 1,
            ..TargetFrame::default()
        };
        frame.targets[0] = WireTarget {
            track: 1,
            freq: 1000.0,
            q: 8.0,
            excess_db: 10.0,
            confidence: 1.0,
            owner: -1,
            channel: 0,
        };
        let mut back = 0u8;
        engine.spectral_shared().frames.publish(&mut back, frame);

        let mut last = 0.0f32;
        let mut max_step = 0.0f32;
        for block in 0..1200 {
            // Toggle the monitor twice, mid-suppression.
            if block == 600 {
                on.resonance.delta = true;
            }
            if block == 900 {
                on.resonance.delta = false;
            }
            let mut l = [0.0f32; CONTROL_BLOCK];
            for (i, x) in l.iter_mut().enumerate() {
                let t = (block * CONTROL_BLOCK + i) as f64 / SR as f64;
                *x = (2.0 * std::f64::consts::PI * 1000.0 * t).sin() as f32 * 0.5;
            }
            engine.process_block(&mut l, None, &on);
            for x in l {
                if block > 2 {
                    max_step = max_step.max((x - last).abs());
                }
                last = x;
            }
        }
        // A 1 kHz half-scale sine moves at most ~0.065 a sample; a hard delta
        // switch would step by the full removed component at once.
        assert!(max_step < 0.1, "delta toggling stepped by {max_step}");
    }

    /// The whole spectral loop with the real worker thread in it: audio fed
    /// through the engine, the worker finding the tone on its own clock, the
    /// pool engaging — and the thread joining cleanly when the worker drops.
    #[test]
    fn the_worker_thread_feeds_the_pool() {
        use crate::dsp::spectral::SpectralWorker;

        let mut engine = EqEngine::new(SR);
        let worker = SpectralWorker::spawn(engine.spectral_shared());
        let s = spectral_settings();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut at = 0usize;
        let mut engaged = false;
        while std::time::Instant::now() < deadline {
            let mut l = [0.0f32; CONTROL_BLOCK];
            let mut r = [0.0f32; CONTROL_BLOCK];
            for i in 0..CONTROL_BLOCK {
                let t = (at + i) as f64 / SR as f64;
                let x = (2.0 * std::f64::consts::PI * 1000.0 * t).sin() as f32;
                l[i] = x;
                r[i] = x;
            }
            at += CONTROL_BLOCK;
            engine.process_block(&mut l, Some(&mut r), &s);
            if engine.resonance_peak() > 6.0 {
                engaged = true;
                break;
            }
            // Pace the feed near real time so the worker's own cadence — a
            // few milliseconds of wall clock per pass — gets its turns.
            std::thread::sleep(std::time::Duration::from_micros(300));
        }
        assert!(engaged, "the worker never drove the pool into a cut");
        drop(worker); // joins, or this test hangs — which is the assertion
    }

    /// Rough throughput of the whole engine, printed as a multiple of real
    /// time. Not an assertion — numbers this depends on belong to the machine
    /// it runs on. `cargo test --release --lib engine_throughput -- --ignored --nocapture`
    #[test]
    #[ignore = "prints timings; run explicitly in release"]
    fn engine_throughput() {
        let seconds = 30.0f32;
        let blocks = (seconds * SR / CONTROL_BLOCK as f32) as usize;

        let mut cases: Vec<(&str, Settings)> = vec![
            ("idle (no resonance)", Settings::default()),
            (
                "adaptive (bank)",
                Settings {
                    resonance: suppressing(),
                    ..Settings::default()
                },
            ),
            ("spectral (pool, 8 targets)", spectral_settings()),
        ];
        // Give the spectral case a full pool to run.
        if let Some((_, s)) = cases.last_mut() {
            s.res_quality = crate::params::ResQuality::Ultra;
        }

        for (name, s) in cases {
            let mut engine = EqEngine::new(SR);
            if name.starts_with("spectral") {
                let mut frame = TargetFrame {
                    serial: 1,
                    count: 8,
                    ..TargetFrame::default()
                };
                for (i, t) in frame.targets[..8].iter_mut().enumerate() {
                    *t = WireTarget {
                        track: i as u32 + 1,
                        freq: 200.0 * 2f32.powf(i as f32 * 0.8),
                        q: 8.0,
                        excess_db: 9.0,
                        confidence: 1.0,
                        owner: -1,
                        channel: 0,
                    };
                }
                let mut back = 0u8;
                engine.spectral_shared().frames.publish(&mut back, frame);
            }

            let mut l = [0.25f32; CONTROL_BLOCK];
            let mut r = [0.25f32; CONTROL_BLOCK];
            let start = std::time::Instant::now();
            for block in 0..blocks {
                for i in 0..CONTROL_BLOCK {
                    let t = (block * CONTROL_BLOCK + i) as f32 / SR;
                    let x =
                        (2.0 * PI * 220.0 * t).sin() * 0.4 + (2.0 * PI * 3130.0 * t).sin() * 0.2;
                    l[i] = x;
                    r[i] = x * 0.8;
                }
                engine.process_block(&mut l, Some(&mut r), &s);
            }
            let took = start.elapsed().as_secs_f32();
            println!(
                "{name}: {seconds} s of stereo audio in {took:.3} s — {:.0}x real time",
                seconds / took
            );
        }

        // And the detector on its own, per analysis pass.
        let shared = SharedSpectral::default();
        shared.cfg.publish(&crate::dsp::spectral::ConfigView {
            sample_rate: SR,
            global_on: true,
            quality: 2,
            ..crate::dsp::spectral::ConfigView::default()
        });
        let mut detector = Detector::new(SR);
        for i in 0..detector.hop * 8 {
            let x = (2.0 * PI * 1000.0 * i as f32 / SR).sin() * 0.5;
            shared.ring.push(x, x);
        }
        let passes = 2000;
        let start = std::time::Instant::now();
        for _ in 0..passes {
            detector.analyze(&shared);
        }
        let per_pass = start.elapsed().as_secs_f32() / passes as f32;
        println!(
            "detector: {:.0} µs per pass, one pass every {:.1} ms of audio",
            per_pass * 1e6,
            detector.hop as f32 / SR * 1000.0
        );
    }

    /// The spectral engines follow the sample rate: detector sizing and pool
    /// coefficients both derive from it, at every supported rate.
    #[test]
    fn spectral_mode_works_across_sample_rates() {
        for sr in [44_100.0f32, 96_000.0, 192_000.0] {
            let mut engine = EqEngine::new(sr);
            let mut detector = Detector::new(sr);
            let s = spectral_settings();

            let shared = engine.spectral_shared();
            let mut sum = 0.0f64;
            let mut counted = 0usize;
            let mut fed = 0usize;
            let blocks = (sr / 48_000.0 * 1200.0) as usize;
            for block in 0..blocks {
                let mut l = [0.0f32; CONTROL_BLOCK];
                let mut r = [0.0f32; CONTROL_BLOCK];
                for i in 0..CONTROL_BLOCK {
                    let t = (block * CONTROL_BLOCK + i) as f64 / sr as f64;
                    let x = (2.0 * std::f64::consts::PI * 1000.0 * t).sin() as f32;
                    l[i] = x;
                    r[i] = x;
                }
                engine.process_block(&mut l, Some(&mut r), &s);
                fed += CONTROL_BLOCK;
                if fed >= detector.hop {
                    fed = 0;
                    detector.analyze(&shared);
                }
                if block >= blocks / 2 {
                    for x in l {
                        sum += (x * x) as f64;
                        counted += 1;
                    }
                }
            }
            let out = 20.0 * ((sum / counted as f64).sqrt().max(1e-9) as f32).log10();
            assert!(
                (out - (SINE_RMS_DB - 12.0)).abs() < 2.0,
                "{sr} Hz: expected the 12 dB Range of cut, got {out} dB"
            );
        }
    }
}
