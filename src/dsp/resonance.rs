//! Adaptive resonance suppression.
//!
//! # What a resonance is, for this purpose
//!
//! A resonance is a narrow peak that stands out from the spectrum around it: a
//! room mode, a boxy guitar body, a sibilant that only bites on some words. What
//! makes it a problem is never its absolute level — it is that it sits *louder
//! than its own neighbourhood*. That is also why a static EQ cut is the wrong
//! tool: the resonance moves with the note and with the take, and the cut does
//! not, so a notch deep enough to tame the worst syllable hollows out every
//! other one.
//!
//! So this stage measures the spectrum through a bank of [`RES_BANDS`] filters,
//! builds a smoothed reference out of those measurements, and cuts only where a
//! band stands proud of it. Material whose spectrum is merely *sloped* — which
//! is nearly all music — reads as flat against its own smoothed self and comes
//! through untouched. A peak riding on top of it does not. Nothing here has an
//! opinion about which frequencies are bad; the signal decides, moment to
//! moment, which is what "adaptive" is doing in the name.
//!
//! # Why there is no latency
//!
//! Nothing looks ahead. Detection is a bank of bandpasses, suppression is a bank
//! of peaking filters, and both are biquads — so every output sample is a
//! function of input samples up to and including itself, and the plugin reports
//! zero latency because it genuinely has none.
//!
//! The usual way to build this is an FFT with overlap-add, which buys frequency
//! resolution cheaply and pays for it with a window of delay. A filter bank pays
//! in arithmetic instead, and arithmetic is the cheaper currency for anything
//! that has to sit on a monitoring path or in front of a performer.
//!
//! One ordering detail keeps that honest. A block is suppressed with the plan
//! worked out from the *previous* block, not from itself: measuring a block and
//! then acting on it would let the gain applied to its first sample depend on
//! its last, which is a lookahead of two thirds of a millisecond however little
//! it looks like one. Paying a control block of reaction time instead costs
//! nothing audible under any usable attack setting.
//!
//! # The three steps
//!
//! 1. **Measure.** Every band's level, through a constant-Q bandpass and the
//!    same frequency-tied integrator the band dynamics use.
//! 2. **Compare.** Smooth those levels across log-frequency into a reference,
//!    and take each band's excess over it. How wide that smoothing is *is* the
//!    sharpness control: a wide reference lets broad humps count as resonances,
//!    a narrow one hugs the spectrum so closely that only spikes stand out.
//! 3. **Cut.** Attack/release ballistics on each band's reduction, a correction
//!    for the fact that neighbouring filters overlap, then a peaking filter per
//!    band that is actually doing something.

use crate::dsp::biquad::{Biquad, Coeffs};
use crate::dsp::dynamics::{step_toward, LevelDetector};
use crate::dsp::engine::CONTROL_BLOCK;
use crate::params::BandKind;

/// Bands in the analysis and suppression bank.
pub const RES_BANDS: usize = 60;
/// Resolution of the bank. Six per octave is a semitone and a half — fine enough
/// to sit inside a resonance without being so fine that the bank costs more than
/// the problem is worth.
pub const RES_BANDS_PER_OCTAVE: f32 = 6.0;
/// Centre of the lowest band. The highest lands near 18 kHz.
pub const RES_F_LO: f32 = 20.0;

/// Ceiling on how much any one band may be cut.
pub const RES_MAX_CUT_DB: f32 = 36.0;

/// Q putting a band's half-power points on its neighbours' centres:
/// `1 / (2^(1/12) - 2^(-1/12))` for a sixth-octave spacing.
const BAND_Q: f32 = 8.651;

/// Sections in each detection filter.
///
/// One is not enough. A second-order bandpass falls away at only 6 dB an octave,
/// so a lone sine still reads within 20 dB of its peak a whole octave out — the
/// bank sees a broad hill where there is a spike, the reference built from those
/// readings sits barely 10 dB under the peak, and the most obvious resonance
/// there is registers as a mild one. Two sections give 12 dB an octave, which is
/// the difference between a stage that finds resonances and one that shrugs.
const DET_SECTIONS: usize = 2;

/// Q of each detection section.
///
/// Cascading two identical bandpasses narrows the combined passband by
/// `sqrt(2^(1/2) - 1)`, so each section is made correspondingly wider and the
/// pair ends up with the sixth-octave bandwidth the bank is spaced for.
const DET_Q: f32 = BAND_Q * 0.6436;

/// How far the reference's smoothing kernel may reach, in bands. Nine is an
/// octave and a half either side.
const MAX_KERNEL_RADIUS: usize = 9;
/// And the narrowest it goes — a third of an octave either side.
const MIN_KERNEL_RADIUS: usize = 2;

/// How far a peaking filter still measurably affects its neighbours, in bands.
/// Two octaves out an 8.65-Q bell is under a hundredth of a dB.
const SHAPE_RADIUS: usize = 12;

/// A band this quiet has nothing audible in it to suppress, whatever it does
/// relative to its neighbours. Without this the bank chases the noise floor
/// between notes, where a 15 dB "resonance" is 15 dB of nothing.
const RES_FLOOR_DB: f32 = -72.0;

/// Below this a peaking filter is indistinguishable from a wire, so the band
/// stops being processed at all.
const MIN_CUT_DB: f32 = 0.02;

/// Blocks a band keeps running after it goes quiet, so its delay line drains
/// through near-identity coefficients rather than being cut off mid-ring.
const IDLE_BLOCKS: u8 = 2;

/// Centre frequency of band `i`.
pub fn band_freq(i: usize) -> f32 {
    RES_F_LO * 2f32.powf(i as f32 / RES_BANDS_PER_OCTAVE)
}

/// Everything the stage needs for one control block.
#[derive(Clone, Copy, Debug)]
pub struct ResonanceSettings {
    pub enabled: bool,
    /// Fraction of a band's excess to remove, 0..1.
    pub depth: f32,
    /// 0..1. Higher narrows the reference, so only tighter peaks read as
    /// resonances.
    pub sharpness: f32,
    /// dB above the reference at which suppression starts.
    pub threshold_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    /// Frequency range the stage works over, faded in over an octave at each end.
    pub low_hz: f32,
    pub high_hz: f32,
    /// 0..1 blend between the signal as it arrived and the suppressed one.
    pub mix: f32,
    /// Monitor what is being removed instead of what is being kept.
    pub delta: bool,
}

impl Default for ResonanceSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            depth: 0.5,
            sharpness: 0.5,
            threshold_db: 3.0,
            attack_ms: 5.0,
            release_ms: 40.0,
            low_hz: RES_F_LO,
            high_hz: 20_000.0,
            mix: 1.0,
            delta: false,
        }
    }
}

/// Weight of a band at `freq` inside the working range.
///
/// Faded over an octave at each end with a smoothstep, because a range control
/// with a wall at each edge just moves the artefact to the edge.
fn range_weight(freq: f32, low: f32, high: f32) -> f32 {
    // An inverted range is off rather than inside-out, and a NaN edge — which
    // the parameter ranges make impossible, but the type does not — would
    // otherwise propagate straight through the clamp into a NaN gain.
    if low.is_nan() || high.is_nan() || low >= high {
        return 0.0;
    }
    let ease = |x: f32| {
        let t = x.clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    };
    ease((freq / low).log2()).min(ease((high / freq).log2()))
}

fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Half the bandwidth of a bell, in octaves.
fn bell_half_width_oct(q: f32) -> f32 {
    // BW = (2 / ln 2) * asinh(1 / 2Q), the standard cookbook relation.
    let x = 1.0 / (2.0 * q.max(0.01));
    (x + (x * x + 1.0).sqrt()).ln() / std::f32::consts::LN_2
}

/// How much of an EQ band's own resonance amount lands on the bank band at `freq`.
///
/// A band's *region* is the slice of spectrum it works on — the same notion the
/// dynamics sidechain already uses: a shelf or a cut owns everything on its side
/// of the corner, and everything else owns its bump. Weighting by that is what
/// keeps a per-band suppressor inside the band the user pointed it at, instead
/// of quietly becoming a second global one.
pub fn band_region_weight(kind: BandKind, band_freq: f32, q: f32, freq: f32) -> f32 {
    // A log of zero or of a NaN would spread that NaN across the whole bank.
    if band_freq.is_nan() || freq.is_nan() || band_freq <= 0.0 || freq <= 0.0 {
        return 0.0;
    }
    let octaves = (freq / band_freq).log2();
    match kind {
        // Fades across the corner rather than at it: half weight on the corner,
        // full an octave inside, nothing an octave out.
        BandKind::LowShelf | BandKind::LowCut => smoothstep(0.5 - octaves),
        BandKind::HighShelf | BandKind::HighCut => smoothstep(0.5 + octaves),
        _ => {
            // Floored at one bank band, or a surgical bell would fall between
            // the bank's teeth and suppress nothing at all.
            let half = bell_half_width_oct(q).max(1.0 / RES_BANDS_PER_OCTAVE);
            // Full weight out to the band's own half-bandwidth, gone by twice it.
            1.0 - smoothstep((octaves.abs() - half) / half)
        }
    }
}

pub struct ResonanceBank {
    sr: f32,
    freqs: [f32; RES_BANDS],
    /// Bands whose centre is far enough below Nyquist to be worth filtering at.
    live: usize,

    /// Detection: a cascaded bandpass per band, per channel, into one integrator.
    det_coeffs: [Coeffs; RES_BANDS],
    det_l: [[Biquad; DET_SECTIONS]; RES_BANDS],
    det_r: [[Biquad; DET_SECTIONS]; RES_BANDS],
    level: [LevelDetector; RES_BANDS],
    levels_db: [f32; RES_BANDS],

    /// Working space for the reference, padded so the kernel never runs off the
    /// end of the real data.
    padded: [f32; RES_BANDS + 2 * MAX_KERNEL_RADIUS],
    reference_db: [f32; RES_BANDS],
    /// Where each band's reduction is heading, before ballistics.
    target_db: [f32; RES_BANDS],
    /// And where it has got to, in dB of cut — positive.
    reduction_db: [f32; RES_BANDS],
    /// What was actually asked of the filter, after the overlap correction.
    applied_db: [f32; RES_BANDS],

    /// Suppression: one peaking filter per band, per channel.
    cut_coeffs: [Coeffs; RES_BANDS],
    cut_l: [Biquad; RES_BANDS],
    cut_r: [Biquad; RES_BANDS],
    /// dB the coefficients were last built for, so a still band costs no trig.
    cut_key: [f32; RES_BANDS],
    idle: [u8; RES_BANDS],

    /// Smoothing kernel for the reference, and the sharpness it was built for.
    kernel: [f32; 2 * MAX_KERNEL_RADIUS + 1],
    kernel_radius: usize,
    kernel_key: f32,

    /// dB one band's peaking filter contributes at each neighbour's centre.
    shape: [f32; SHAPE_RADIUS + 1],

    dry_l: [f32; CONTROL_BLOCK],
    dry_r: [f32; CONTROL_BLOCK],
}

impl Default for ResonanceBank {
    fn default() -> Self {
        Self::new(48_000.0)
    }
}

impl ResonanceBank {
    pub fn new(sr: f32) -> Self {
        let mut bank = Self {
            sr,
            freqs: std::array::from_fn(band_freq),
            live: 0,
            det_coeffs: [Coeffs::identity(); RES_BANDS],
            det_l: [[Biquad::new(); DET_SECTIONS]; RES_BANDS],
            det_r: [[Biquad::new(); DET_SECTIONS]; RES_BANDS],
            level: [LevelDetector::new(); RES_BANDS],
            levels_db: [-120.0; RES_BANDS],
            padded: [-120.0; RES_BANDS + 2 * MAX_KERNEL_RADIUS],
            reference_db: [-120.0; RES_BANDS],
            target_db: [0.0; RES_BANDS],
            reduction_db: [0.0; RES_BANDS],
            applied_db: [0.0; RES_BANDS],
            cut_coeffs: [Coeffs::identity(); RES_BANDS],
            cut_l: [Biquad::new(); RES_BANDS],
            cut_r: [Biquad::new(); RES_BANDS],
            cut_key: [0.0; RES_BANDS],
            idle: [IDLE_BLOCKS; RES_BANDS],
            kernel: [0.0; 2 * MAX_KERNEL_RADIUS + 1],
            kernel_radius: 0,
            kernel_key: f32::NAN,
            shape: overlap_shape(),
            dry_l: [0.0; CONTROL_BLOCK],
            dry_r: [0.0; CONTROL_BLOCK],
        };
        bank.rebuild(sr);
        bank
    }

    pub fn set_sample_rate(&mut self, sr: f32) {
        if (sr - self.sr).abs() > f32::EPSILON {
            self.rebuild(sr);
        }
        self.reset();
    }

    /// Recompute everything that depends on the sample rate.
    fn rebuild(&mut self, sr: f32) {
        self.sr = sr;
        // Well clear of Nyquist: a constant-Q bandpass right up against it is
        // badly warped, and there is nothing up there worth suppressing anyway.
        let ceiling = sr * 0.45;
        self.live = self.freqs.iter().take_while(|f| **f < ceiling).count();
        for i in 0..self.live {
            self.det_coeffs[i] = Coeffs::bandpass(self.freqs[i], DET_Q, sr);
            self.level[i].set_window(self.freqs[i], sr);
        }
        for i in 0..RES_BANDS {
            self.cut_coeffs[i] = Coeffs::identity();
            self.cut_key[i] = 0.0;
        }
    }

    pub fn reset(&mut self) {
        for i in 0..RES_BANDS {
            for sec in 0..DET_SECTIONS {
                self.det_l[i][sec].reset();
                self.det_r[i][sec].reset();
            }
            self.cut_l[i].reset();
            self.cut_r[i].reset();
            self.level[i].reset();
            self.levels_db[i] = -120.0;
            self.reduction_db[i] = 0.0;
            self.applied_db[i] = 0.0;
            self.idle[i] = IDLE_BLOCKS;
        }
    }

    /// dB of cut each band is applying, for the UI. Positive is a cut.
    pub fn reduction(&self, out: &mut [f32]) {
        for (slot, value) in out.iter_mut().enumerate().take(RES_BANDS) {
            *value = self.reduction_db[slot];
        }
    }

    /// The deepest cut anywhere in the bank, in dB.
    pub fn peak_reduction(&self) -> f32 {
        self.reduction_db[..self.live]
            .iter()
            .fold(0.0f32, |acc, v| acc.max(*v))
    }

    /// Process one control block in place. `right` is `None` on mono.
    ///
    /// `band_depth` is what the EQ's own bands are asking for, per bank band —
    /// see [`band_region_weight`]. It stands on its own: a band with a resonance
    /// amount works whether or not the global stage is switched on, and is not
    /// subject to the global frequency range, which would otherwise let a
    /// narrowed range silently cancel a suppressor aimed at one band.
    pub fn process(
        &mut self,
        left: &mut [f32],
        right: Option<&mut [f32]>,
        s: &ResonanceSettings,
        band_depth: &[f32],
    ) {
        let n = left.len().min(CONTROL_BLOCK);
        if n == 0 {
            return;
        }
        if !s.enabled && !band_depth.iter().any(|d| *d > 0.0) {
            // Coming back on with a bank full of state that describes whatever
            // was playing before the bypass is a burst of stale suppression.
            if self.peak_reduction() != 0.0 || self.idle.iter().any(|i| *i < IDLE_BLOCKS) {
                self.reset();
            }
            return;
        }

        let mut right = right;
        let dt = n as f32 / self.sr;

        // The detectors read the signal as it arrived, not as the bank has left
        // it. Feeding them the output instead would make each band chase its own
        // reduction: cut the peak, stop seeing the peak, let go, hear it again.
        self.dry_l[..n].copy_from_slice(&left[..n]);
        match right.as_deref() {
            Some(r) => self.dry_r[..n].copy_from_slice(&r[..n]),
            None => self.dry_r[..n].copy_from_slice(&left[..n]),
        }

        // Suppress first, with the plan built at the end of the *previous*
        // block. Measuring this block and then acting on it inside the same
        // block would be a lookahead — small, at two thirds of a millisecond,
        // but real: the gain on the first sample would depend on the last. This
        // way every output sample is a function of strictly earlier input, and
        // the stage pays one control block of reaction time for it, which
        // disappears under an attack of even a couple of milliseconds.
        self.suppress(left, right.as_deref_mut(), n);
        self.blend(left, right, n, s);

        self.measure(n);
        self.build_reference(s.sharpness);
        self.plan(s, band_depth, dt);
    }

    /// Step 1 — every band's level, through its bandpass.
    fn measure(&mut self, n: usize) {
        for i in 0..self.live {
            let c = self.det_coeffs[i];
            for j in 0..n {
                let mut a = self.dry_l[j];
                let mut b = self.dry_r[j];
                for sec in 0..DET_SECTIONS {
                    a = self.det_l[i][sec].process(a, &c);
                    b = self.det_r[i][sec].process(b, &c);
                }
                // Both channels, averaged as power. A resonance that happens to
                // sit anti-phase across the pair is still a resonance, and it
                // would be invisible to a detector listening to the mono sum.
                self.level[i].push_ms(0.5 * (a * a + b * b));
            }
            self.levels_db[i] = self.level[i].level_db();
        }
    }

    /// Step 2 — the smoothed reference each band is judged against.
    fn build_reference(&mut self, sharpness: f32) {
        self.update_kernel(sharpness);
        let r = self.kernel_radius;
        let live = self.live;
        if live == 0 {
            return;
        }

        // Pad by extrapolating the spectrum's own slope rather than by repeating
        // the edge value. Constant-Q analysis of anything broadband rises with
        // frequency — six dB an octave for white noise — and a flat pad would
        // drag the reference below the real levels at the top of the bank,
        // inventing a resonance out of the whole top octave.
        let fit = r.min(live.saturating_sub(1)).max(1);
        let slope_lo = (self.levels_db[fit] - self.levels_db[0]) / fit as f32;
        let slope_hi = (self.levels_db[live - 1] - self.levels_db[live - 1 - fit]) / fit as f32;
        for k in 0..r {
            self.padded[r - 1 - k] = self.levels_db[0] - slope_lo * (k + 1) as f32;
            self.padded[r + live + k] = self.levels_db[live - 1] + slope_hi * (k + 1) as f32;
        }
        self.padded[r..r + live].copy_from_slice(&self.levels_db[..live]);

        for i in 0..live {
            let mut sum = 0.0;
            for k in 0..=2 * r {
                sum += self.padded[i + k] * self.kernel[k];
            }
            self.reference_db[i] = sum;
        }
    }

    /// A normalized triangular kernel, as wide as sharpness says.
    fn update_kernel(&mut self, sharpness: f32) {
        if sharpness == self.kernel_key {
            return;
        }
        self.kernel_key = sharpness;
        // Sharp means "only tight peaks count", which is a *narrow* reference:
        // one that hugs the spectrum so closely that a broad hump reads as its
        // own average and only a spike stands above it.
        let span = MAX_KERNEL_RADIUS as f32
            - sharpness.clamp(0.0, 1.0) * (MAX_KERNEL_RADIUS - MIN_KERNEL_RADIUS) as f32;
        let r = (span.round() as usize).clamp(MIN_KERNEL_RADIUS, MAX_KERNEL_RADIUS);
        self.kernel_radius = r;

        let mut total = 0.0;
        for k in 0..=2 * r {
            let w = 1.0 - (k as f32 - r as f32).abs() / (r + 1) as f32;
            self.kernel[k] = w;
            total += w;
        }
        for k in 0..=2 * r {
            self.kernel[k] /= total;
        }
    }

    /// Step 3 — excess, ballistics, and the correction for overlapping filters.
    fn plan(&mut self, s: &ResonanceSettings, band_depth: &[f32], dt: f32) {
        let global = if s.enabled { s.depth.max(0.0) } else { 0.0 };
        for i in 0..self.live {
            // The global stage works across its own frequency range; a band's
            // own amount works across the band, and adds to it.
            let depth = (global * range_weight(self.freqs[i], s.low_hz, s.high_hz)
                + band_depth.get(i).copied().unwrap_or(0.0).max(0.0))
            .min(1.0);

            let excess = self.levels_db[i] - self.reference_db[i] - s.threshold_db;
            let audible = self.levels_db[i] > RES_FLOOR_DB;
            let want = if excess > 0.0 && audible {
                excess * depth
            } else {
                0.0
            };
            self.target_db[i] = want.min(RES_MAX_CUT_DB);
        }

        for i in 0..self.live {
            let tau = if self.target_db[i] > self.reduction_db[i] {
                s.attack_ms
            } else {
                s.release_ms
            };
            self.reduction_db[i] = step_toward(self.reduction_db[i], self.target_db[i], tau, dt);
        }

        // A bell at one band's centre is still most of a dB down at its
        // neighbour's, so a run of bands all asking for the same cut would
        // deliver several times it. Predict what the bank as planned would
        // actually do at each centre and scale each band by how far that
        // overshoots — exact for a lone peak, and exactly the overlap factor
        // for a flat stretch, with everything real falling between the two.
        for i in 0..self.live {
            let want = self.reduction_db[i];
            if want < MIN_CUT_DB {
                self.applied_db[i] = 0.0;
                continue;
            }
            let mut predicted = 0.0;
            let lo = i.saturating_sub(SHAPE_RADIUS);
            let hi = (i + SHAPE_RADIUS + 1).min(self.live);
            for (j, item) in self.reduction_db[lo..hi].iter().enumerate() {
                predicted += item * self.shape[(lo + j).abs_diff(i)];
            }
            self.applied_db[i] = (want * want / predicted.max(1e-6)).min(RES_MAX_CUT_DB);
        }
        for i in self.live..RES_BANDS {
            self.applied_db[i] = 0.0;
        }
    }

    /// Run the filters for the bands that are doing something.
    fn suppress(&mut self, left: &mut [f32], right: Option<&mut [f32]>, n: usize) {
        let mut right = right;
        for i in 0..self.live {
            let cut = self.applied_db[i];
            if cut < MIN_CUT_DB {
                self.idle[i] = self.idle[i].saturating_add(1);
                if self.idle[i] == IDLE_BLOCKS {
                    // Drained through near-identity coefficients over the last
                    // couple of blocks, so there is nothing left to click.
                    self.cut_l[i].reset();
                    self.cut_r[i].reset();
                }
                if self.idle[i] >= IDLE_BLOCKS {
                    continue;
                }
            } else {
                self.idle[i] = 0;
            }

            if (cut - self.cut_key[i]).abs() > 0.005 {
                self.cut_key[i] = cut;
                self.cut_coeffs[i] = Coeffs::peaking(self.freqs[i], BAND_Q, -cut, self.sr);
            }
            let c = self.cut_coeffs[i];
            for x in left[..n].iter_mut() {
                *x = self.cut_l[i].process(*x, &c);
            }
            if let Some(r) = right.as_deref_mut() {
                for x in r[..n].iter_mut() {
                    *x = self.cut_r[i].process(*x, &c);
                }
            }
        }
    }

    /// Mix, or hand back only what was taken away.
    fn blend(
        &mut self,
        left: &mut [f32],
        right: Option<&mut [f32]>,
        n: usize,
        s: &ResonanceSettings,
    ) {
        if s.delta {
            subtract(&mut left[..n], &self.dry_l[..n]);
            if let Some(r) = right {
                subtract(&mut r[..n], &self.dry_r[..n]);
            }
            return;
        }

        let mix = s.mix.clamp(0.0, 1.0);
        if mix >= 1.0 {
            return;
        }
        crossfade(&mut left[..n], &self.dry_l[..n], mix);
        if let Some(r) = right {
            crossfade(&mut r[..n], &self.dry_r[..n], mix);
        }
    }
}

/// Replace the suppressed signal with what was taken out of it.
fn subtract(wet: &mut [f32], dry: &[f32]) {
    for (out, dry) in wet.iter_mut().zip(dry) {
        *out = dry - *out;
    }
}

/// Fade the suppressed signal back toward the one that arrived.
fn crossfade(wet: &mut [f32], dry: &[f32], mix: f32) {
    for (out, dry) in wet.iter_mut().zip(dry) {
        *out = dry + (*out - dry) * mix;
    }
}

/// dB a unit peaking filter contributes at each neighbouring band's centre.
///
/// Sampled once from the real coefficients rather than approximated, so the
/// overlap correction is describing the filters the bank actually runs. The
/// shape is a function of the ratio between the offset and the centre, so any
/// centre well below Nyquist gives the same answer.
fn overlap_shape() -> [f32; SHAPE_RADIUS + 1] {
    const SR: f32 = 48_000.0;
    const FC: f32 = 1000.0;
    let c = Coeffs::peaking(FC, BAND_Q, -1.0, SR);
    std::array::from_fn(|k| {
        let f = FC * 2f32.powf(k as f32 / RES_BANDS_PER_OCTAVE);
        // The filter was built at -1 dB, so its response in dB is the shape.
        -20.0 * c.magnitude(f, SR).log10()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    const SR: f32 = 48_000.0;
    /// No EQ band is asking for anything of its own.
    const NONE: [f32; RES_BANDS] = [0.0; RES_BANDS];

    fn on() -> ResonanceSettings {
        ResonanceSettings {
            enabled: true,
            depth: 1.0,
            attack_ms: 2.0,
            release_ms: 20.0,
            ..ResonanceSettings::default()
        }
    }

    /// Deterministic white-ish noise. A real RNG would make a failure
    /// unreproducible, and this only has to be broadband.
    struct Noise(u32);

    impl Noise {
        fn next(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (self.0 >> 8) as f32 / (1 << 23) as f32 - 1.0
        }
    }

    /// Run a generator through the bank and return the input and output RMS in
    /// dB, measured over the second half so the detectors have settled.
    fn run(
        bank: &mut ResonanceBank,
        s: &ResonanceSettings,
        blocks: usize,
        gen: impl FnMut(usize) -> f32,
    ) -> (f32, f32) {
        run_with(bank, s, &NONE, blocks, gen)
    }

    /// As [`run`], with the EQ bands asking for something of their own.
    fn run_with(
        bank: &mut ResonanceBank,
        s: &ResonanceSettings,
        band_depth: &[f32],
        blocks: usize,
        mut gen: impl FnMut(usize) -> f32,
    ) -> (f32, f32) {
        let (mut dry, mut wet) = (0.0f64, 0.0f64);
        let mut counted = 0usize;
        for block in 0..blocks {
            let mut l = [0.0f32; CONTROL_BLOCK];
            let mut r = [0.0f32; CONTROL_BLOCK];
            for i in 0..CONTROL_BLOCK {
                let x = gen(block * CONTROL_BLOCK + i);
                l[i] = x;
                r[i] = x;
            }
            let input = l;
            bank.process(&mut l, Some(&mut r), s, band_depth);
            if block >= blocks / 2 {
                for i in 0..CONTROL_BLOCK {
                    dry += (input[i] * input[i]) as f64;
                    wet += (l[i] * l[i]) as f64;
                    counted += 1;
                }
            }
        }
        let db = |sum: f64| 20.0 * ((sum / counted as f64).sqrt().max(1e-12) as f32).log10();
        (db(dry), db(wet))
    }

    fn sine(freq: f32) -> impl FnMut(usize) -> f32 {
        move |i| (2.0 * PI * freq * i as f32 / SR).sin()
    }

    #[test]
    fn a_lone_tone_is_the_clearest_resonance_there_is() {
        let mut bank = ResonanceBank::new(SR);
        let (dry, wet) = run(&mut bank, &on(), 600, sine(1000.0));
        assert!(
            wet < dry - 12.0,
            "a pure tone came through at {wet} dB against {dry} dB in"
        );
        assert!(bank.peak_reduction() > 12.0);
    }

    /// The point of the whole design: broadband material has no excess over its
    /// own smoothed spectrum, so there is nothing to suppress.
    #[test]
    fn broadband_noise_comes_through_almost_untouched() {
        let mut bank = ResonanceBank::new(SR);
        let mut noise = Noise(12345);
        let (dry, wet) = run(&mut bank, &on(), 600, move |_| noise.next() * 0.3);
        assert!(
            (wet - dry).abs() < 1.5,
            "noise came out {} dB off, at {wet} against {dry}",
            wet - dry
        );
    }

    /// And the case that separates this from a plain multiband compressor: the
    /// tone has to go while the noise it sits in stays.
    #[test]
    fn a_tone_riding_on_noise_is_cut_and_the_noise_is_not() {
        let mut bank = ResonanceBank::new(SR);
        let mut noise = Noise(999);
        let mut tone = sine(2000.0);
        let (_, with_tone) = run(&mut bank, &on(), 600, move |i| {
            noise.next() * 0.1 + tone(i) * 0.5
        });

        // The same noise on its own, through a fresh bank.
        let mut bank_ref = ResonanceBank::new(SR);
        let mut noise_ref = Noise(999);
        let (noise_dry, noise_wet) = run(&mut bank_ref, &on(), 600, move |_| noise_ref.next() * 0.1);

        assert!(
            (noise_wet - noise_dry).abs() < 1.5,
            "the noise bed alone moved {} dB",
            noise_wet - noise_dry
        );
        // A 0.5 sine is -9 dB RMS against a noise bed near -25; if the tone
        // survived, the total would still be dominated by it.
        assert!(
            with_tone < -14.0,
            "the tone survived the bank at {with_tone} dB"
        );
    }

    #[test]
    fn a_disabled_bank_is_a_bit_exact_passthrough() {
        let mut bank = ResonanceBank::new(SR);
        let s = ResonanceSettings::default();
        let mut gen = sine(1000.0);
        for block in 0..40 {
            let mut l = [0.0f32; CONTROL_BLOCK];
            let mut r = [0.0f32; CONTROL_BLOCK];
            for i in 0..CONTROL_BLOCK {
                let x = gen(block * CONTROL_BLOCK + i);
                l[i] = x;
                r[i] = x * 0.5;
            }
            let want = (l, r);
            bank.process(&mut l, Some(&mut r), &s, &NONE);
            assert_eq!(l, want.0);
            assert_eq!(r, want.1);
        }
    }

    #[test]
    fn mix_at_zero_is_transparent() {
        let mut bank = ResonanceBank::new(SR);
        let s = ResonanceSettings { mix: 0.0, ..on() };
        let (dry, wet) = run(&mut bank, &s, 400, sine(1000.0));
        assert!((wet - dry).abs() < 0.01, "{wet} against {dry}");
        // The bank was still working underneath, so the control really is a mix.
        assert!(bank.peak_reduction() > 6.0);
    }

    #[test]
    fn delta_hands_back_exactly_what_was_removed() {
        let mut kept = ResonanceBank::new(SR);
        let mut removed = ResonanceBank::new(SR);
        let s = on();
        let delta = ResonanceSettings { delta: true, ..s };
        let mut gen = sine(700.0);

        for block in 0..300 {
            let mut a = [0.0f32; CONTROL_BLOCK];
            let mut b = [0.0f32; CONTROL_BLOCK];
            for i in 0..CONTROL_BLOCK {
                a[i] = gen(block * CONTROL_BLOCK + i);
            }
            b.copy_from_slice(&a);
            let dry = a;
            kept.process(&mut a, None, &s, &NONE);
            removed.process(&mut b, None, &delta, &NONE);
            if block > 150 {
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
    }

    /// The claim in the module docs, tested rather than asserted.
    ///
    /// Two banks are driven identically until one of them gets an extra impulse
    /// part-way through a block. Everything before that sample has to come out
    /// bit-identical — if the stage looked ahead, the impulse would have changed
    /// them — and that sample itself has to differ, which it could not if the
    /// stage delayed its output. Between them the two rule out both halves of
    /// latency, and no overlap-add design passes either.
    #[test]
    fn the_stage_neither_delays_nor_looks_ahead() {
        const AT: usize = 5;
        let s = on();
        let settle = 200;

        let mut plain = ResonanceBank::new(SR);
        let mut poked = ResonanceBank::new(SR);
        run(&mut plain, &s, settle, sine(1000.0));
        run(&mut poked, &s, settle, sine(1000.0));

        let mut a = [0.0f32; CONTROL_BLOCK];
        let mut tone = sine(1000.0);
        for (i, x) in a.iter_mut().enumerate() {
            *x = tone(settle * CONTROL_BLOCK + i);
        }
        let mut b = a;
        b[AT] += 1.0;

        plain.process(&mut a, None, &s, &NONE);
        poked.process(&mut b, None, &s, &NONE);

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

    #[test]
    fn the_bank_stays_finite_on_hostile_material() {
        let mut bank = ResonanceBank::new(SR);
        let s = ResonanceSettings {
            depth: 1.0,
            sharpness: 1.0,
            threshold_db: -12.0,
            attack_ms: 0.5,
            release_ms: 5.0,
            ..on()
        };
        let mut noise = Noise(7);
        for block in 0..900 {
            let mut l = [0.0f32; CONTROL_BLOCK];
            let mut r = [0.0f32; CONTROL_BLOCK];
            for i in 0..CONTROL_BLOCK {
                let t = (block * CONTROL_BLOCK + i) as f32 / SR;
                // Square-ish edges, a swept tone and noise: everything that
                // makes a resonance detector unhappy at once.
                let sweep = (2.0 * PI * (200.0 + 4000.0 * t) * t).sin();
                l[i] = sweep + noise.next() * 0.5 + (2.0 * PI * 60.0 * t).sin().signum() * 0.4;
                r[i] = -l[i];
            }
            bank.process(&mut l, Some(&mut r), &s, &NONE);
            for i in 0..CONTROL_BLOCK {
                assert!(l[i].is_finite() && r[i].is_finite(), "block {block} blew up");
                assert!(l[i].abs() < 20.0, "block {block} reached {}", l[i]);
            }
        }
    }

    #[test]
    fn a_low_sample_rate_drops_the_bands_above_nyquist() {
        let bank = ResonanceBank::new(32_000.0);
        assert!(bank.live < RES_BANDS);
        assert!(band_freq(bank.live - 1) < 32_000.0 * 0.45);
        assert!(band_freq(bank.live) >= 32_000.0 * 0.45);
    }

    /// A band's own amount works on its own, with the global stage switched off.
    #[test]
    fn a_band_amount_suppresses_without_the_global_stage() {
        let off = ResonanceSettings::default();
        assert!(!off.enabled);

        // Full depth aimed squarely at the tone's own band.
        let mut depth = [0.0f32; RES_BANDS];
        let target = (1000f32 / RES_F_LO).log2() * RES_BANDS_PER_OCTAVE;
        for (i, slot) in depth.iter_mut().enumerate() {
            if (i as f32 - target).abs() < 2.0 {
                *slot = 1.0;
            }
        }

        let mut bank = ResonanceBank::new(SR);
        let (dry, wet) = run_with(&mut bank, &off, &depth, 600, sine(1000.0));
        assert!(
            wet < dry - 10.0,
            "a band amount alone did nothing: {wet} against {dry}"
        );

        // And with no band asking, the same disabled settings stay transparent.
        let mut idle = ResonanceBank::new(SR);
        let (dry, wet) = run_with(&mut idle, &off, &NONE, 300, sine(1000.0));
        assert!((wet - dry).abs() < 0.01, "{wet} against {dry}");
    }

    /// The point of weighting by region: a band pointed at one place must not
    /// quietly become a second global suppressor.
    #[test]
    fn a_band_amount_stays_inside_its_own_region() {
        let off = ResonanceSettings::default();
        // A bell at 200 Hz, asking for everything it can get.
        let mut depth = [0.0f32; RES_BANDS];
        for (i, slot) in depth.iter_mut().enumerate() {
            *slot = band_region_weight(BandKind::Bell, 200.0, 2.0, band_freq(i));
        }

        // A tone two and a half octaves above it is outside that region.
        let mut bank = ResonanceBank::new(SR);
        let (dry, wet) = run_with(&mut bank, &off, &depth, 500, sine(1100.0));
        assert!(
            (wet - dry).abs() < 1.0,
            "a 200 Hz band reached 1.1 kHz: {wet} against {dry}"
        );

        // The same band's own frequency is very much inside it.
        let mut bank = ResonanceBank::new(SR);
        let (dry, wet) = run_with(&mut bank, &off, &depth, 500, sine(200.0));
        assert!(
            wet < dry - 8.0,
            "the band did not work at its own centre: {wet} against {dry}"
        );
    }

    #[test]
    fn a_region_covers_the_band_and_fades_outside_it() {
        // A bell owns its bump, and a wide one owns more of the spectrum.
        let wide = band_region_weight(BandKind::Bell, 1000.0, 0.7, 1000.0 * 1.4);
        let narrow = band_region_weight(BandKind::Bell, 1000.0, 8.0, 1000.0 * 1.4);
        assert!(wide > narrow, "Q made no difference: {wide} vs {narrow}");
        assert_eq!(band_region_weight(BandKind::Bell, 1000.0, 2.0, 1000.0), 1.0);

        // Even a surgical bell has to cover a bank band, or it falls between
        // the bank's teeth and suppresses nothing.
        let step = 2f32.powf(1.0 / RES_BANDS_PER_OCTAVE);
        assert!(band_region_weight(BandKind::Bell, 1000.0, 40.0, 1000.0 * step) > 0.0);

        // A shelf owns its side of the corner, and only its side.
        assert!(band_region_weight(BandKind::LowShelf, 500.0, 1.0, 100.0) > 0.9);
        assert!(band_region_weight(BandKind::LowShelf, 500.0, 1.0, 4000.0) < 0.05);
        assert!(band_region_weight(BandKind::HighShelf, 500.0, 1.0, 4000.0) > 0.9);
        assert!(band_region_weight(BandKind::HighShelf, 500.0, 1.0, 100.0) < 0.05);
        // Cuts are shelves for this purpose — same side, same fade.
        assert!(band_region_weight(BandKind::HighCut, 500.0, 1.0, 4000.0) > 0.9);
    }

    #[test]
    fn the_range_control_fades_rather_than_walls() {
        // Inside, full weight; an octave outside, none; and in between, part.
        assert_eq!(range_weight(1000.0, 100.0, 10_000.0), 1.0);
        assert_eq!(range_weight(90.0, 100.0, 10_000.0), 0.0);
        assert_eq!(range_weight(20_000.0, 100.0, 10_000.0), 0.0);
        let edge = range_weight(140.0, 100.0, 10_000.0);
        assert!(edge > 0.0 && edge < 1.0, "the low edge stepped: {edge}");
        // An inverted range is off, not inside-out.
        assert_eq!(range_weight(1000.0, 8000.0, 500.0), 0.0);
    }

    #[test]
    fn the_overlap_shape_peaks_at_the_centre_and_decays() {
        let shape = overlap_shape();
        assert!((shape[0] - 1.0).abs() < 0.01, "centre was {}", shape[0]);
        for k in 1..=SHAPE_RADIUS {
            assert!(shape[k] < shape[k - 1], "shape rose again at {k}");
            assert!(shape[k] >= 0.0, "a peaking bell should not overshoot");
        }
        // Neighbours really do add up, which is what the correction is for.
        let total: f32 = shape[0] + 2.0 * shape[1..].iter().sum::<f32>();
        assert!(total > 1.3, "the bands barely overlap at all: {total}");
    }

    /// Sharper means a reference that hugs the spectrum, so less reads as excess.
    #[test]
    fn sharpness_narrows_what_counts_as_a_resonance() {
        // A broad hump, not a spike: two thirds of an octave wide.
        let hump = |i: usize| {
            let d = (i as f32 - 30.0) / 4.0;
            -40.0 + 12.0 * (-d * d).exp()
        };

        let mut blunt = ResonanceBank::new(SR);
        let mut sharp = ResonanceBank::new(SR);
        for i in 0..RES_BANDS {
            blunt.levels_db[i] = hump(i);
            sharp.levels_db[i] = hump(i);
        }
        blunt.build_reference(0.0);
        sharp.build_reference(1.0);

        let excess = |b: &ResonanceBank| b.levels_db[30] - b.reference_db[30];
        assert!(
            excess(&blunt) > excess(&sharp) + 2.0,
            "blunt saw {} dB of excess, sharp saw {}",
            excess(&blunt),
            excess(&sharp)
        );
    }

    /// The padding has to follow the spectrum's slope, or a tilt reads as a
    /// resonance at whichever end of the bank the kernel runs out of data.
    #[test]
    fn a_tilted_spectrum_has_no_excess_at_the_edges() {
        let mut bank = ResonanceBank::new(SR);
        // Six dB an octave, which is what white noise looks like to this bank.
        for i in 0..RES_BANDS {
            bank.levels_db[i] = -60.0 + i as f32 / RES_BANDS_PER_OCTAVE * 6.0;
        }
        bank.build_reference(0.5);
        for i in 0..bank.live {
            let excess = bank.levels_db[i] - bank.reference_db[i];
            assert!(
                excess.abs() < 0.5,
                "band {i} read {excess} dB of excess off a straight ramp"
            );
        }
    }
}


