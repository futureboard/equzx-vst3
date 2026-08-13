//! Biquad coefficients and a transposed-direct-form-II section.
//!
//! The EQ's bands are **matched** designs after Vicanek's *Matched Second Order
//! Digital Filters*: poles mapped through `z = e^{sT}` exactly, zeros solved so
//! `|H(e^jw)|` meets the analog prototype at DC, the corner and Nyquist. Nothing
//! is bilinear-transformed, so nothing is warped — a 10 kHz corner is 10 kHz at
//! every sample rate. [`design`] builds a whole cascade at a time.
//!
//! The RBJ sections ([`Coeffs::peaking`] and friends) are no longer EQ curves.
//! They are plumbing: dynamics sidechains, solo, and the resonance stage's cuts.
//!
//! [`crate::gui::curves`] draws whatever [`design`] returns, so the curve on
//! screen is the curve being applied. That is also why the design is a pure
//! function of its inputs — editor and audio thread must agree exactly.
//!
//! [`design`] runs on the audio thread. Everything is closed-form bar two
//! searches: the shelf corner (a root find) and the low-pass passband
//! calibration, precomputed into [`dewarp`].
//!
//! Every solve here is rearranged to survive a corner at the bottom of the audio
//! band; written the obvious way they are residues of order `w0^4` between terms
//! of order 1, and return noise below about a hundredth of a radian.

use std::f64::consts::TAU;

mod dewarp;

/// A normalized biquad (a0 divided out).
///
/// f64 because `1 + a1 + a2` is what the section does at low frequency, and it
/// is a residue: at 20 Hz on 192 kHz it is 2.4e-7, which is *one f32 ulp* next
/// to `a1 = -1.999578`. In f32 a swept corner jitters instead of moving. Costs
/// a few percent — the biquad is latency-bound on a serial chain, not
/// throughput-bound — measured by the benches alongside.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Coeffs {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

impl Default for Coeffs {
    fn default() -> Self {
        Self::identity()
    }
}

impl Coeffs {
    pub const fn identity() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
        }
    }

    /// In f64 like the matched sections: the resonance bank puts these at 20 Hz.
    fn norm(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Self {
        // a0 is only ever zero if the caller handed us a degenerate frequency; an
        // identity section is a far better failure than NaN spraying into the mix.
        if a0.abs() < f64::EPSILON || !a0.is_finite() {
            return Self::identity();
        }
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }

    pub fn peaking(freq: f32, q: f32, gain_db: f32, sr: f32) -> Self {
        let a = 10f64.powf(gain_db as f64 / 40.0);
        let w0 = TAU * freq as f64 / sr as f64;
        let cw = w0.cos();
        let alpha = w0.sin() / (2.0 * q as f64);
        Self::norm(
            1.0 + alpha * a,
            -2.0 * cw,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * cw,
            1.0 - alpha / a,
        )
    }

    pub fn bandpass(freq: f32, q: f32, sr: f32) -> Self {
        let w0 = TAU * freq as f64 / sr as f64;
        let cw = w0.cos();
        let alpha = w0.sin() / (2.0 * q as f64);
        Self::norm(alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cw, 1.0 - alpha)
    }

    pub fn lowpass(freq: f32, q: f32, sr: f32) -> Self {
        let w0 = TAU * freq as f64 / sr as f64;
        let alpha = w0.sin() / (2.0 * q as f64);
        // `1 - cos w0` as `2 sin^2(w0/2)`, which does not cancel at low corners.
        let one_minus_cw = 2.0 * (w0 * 0.5).sin().powi(2);
        Self::norm(
            one_minus_cw / 2.0,
            one_minus_cw,
            one_minus_cw / 2.0,
            1.0 + alpha,
            -2.0 * w0.cos(),
            1.0 - alpha,
        )
    }

    pub fn highpass(freq: f32, q: f32, sr: f32) -> Self {
        let w0 = TAU * freq as f64 / sr as f64;
        let alpha = w0.sin() / (2.0 * q as f64);
        let one_plus_cw = 2.0 * (w0 * 0.5).cos().powi(2);
        Self::norm(
            one_plus_cw / 2.0,
            -one_plus_cw,
            one_plus_cw / 2.0,
            1.0 + alpha,
            -2.0 * w0.cos(),
            1.0 - alpha,
        )
    }

    /// |H(e^jw)| at `f`.
    ///
    /// Never called per sample: tests use it, and the resonance bank samples it
    /// once at construction to learn how much its filters overlap.
    pub fn magnitude(&self, f: f32, sr: f32) -> f32 {
        let w = TAU * f as f64 / sr as f64;
        let (sw, cw) = w.sin_cos();
        let (s2w, c2w) = (2.0 * w).sin_cos();

        let num_re = self.b0 + self.b1 * cw + self.b2 * c2w;
        let num_im = -(self.b1 * sw + self.b2 * s2w);
        let den_re = 1.0 + self.a1 * cw + self.a2 * c2w;
        let den_im = -(self.a1 * sw + self.a2 * s2w);

        let den = den_re.hypot(den_im);
        if den == 0.0 {
            0.0
        } else {
            (num_re.hypot(num_im) / den) as f32
        }
    }

    /// Both poles strictly inside the unit circle.
    ///
    /// By radius rather than Jury's `|a1| < 1 + a2`: the same test, but Jury's
    /// subtracts numbers that agree to eleven digits for any low corner.
    fn stable(&self) -> bool {
        let (a1, a2) = (self.a1, self.a2);
        let discriminant = a1 * a1 - 4.0 * a2;
        let radius = if discriminant < 0.0 {
            a2.max(0.0).sqrt()
        } else {
            let root = discriminant.sqrt();
            (0.5 * (-a1 + root)).abs().max((0.5 * (-a1 - root)).abs())
        };
        radius < 1.0
    }

    fn finite(&self) -> bool {
        self.b0.is_finite()
            && self.b1.is_finite()
            && self.b2.is_finite()
            && self.a1.is_finite()
            && self.a2.is_finite()
    }
}

/// One second-order section, transposed direct form II.
///
/// TDF2 rather than DF1 because it holds only two state words and stays well
/// behaved when coefficients are swapped underneath it, which happens on every
/// control block as the smoothers move. The state is f64 for the same reason
/// the coefficients are: the accumulator carries the same cancellation.
#[derive(Clone, Copy, Default, Debug)]
pub struct Biquad {
    z1: f64,
    z2: f64,
}

impl Biquad {
    pub const fn new() -> Self {
        Self { z1: 0.0, z2: 0.0 }
    }

    #[inline(always)]
    pub fn process(&mut self, x: f32, c: &Coeffs) -> f32 {
        let x = x as f64;
        let y = c.b0 * x + self.z1;
        self.z1 = c.b1 * x - c.a1 * y + self.z2;
        self.z2 = c.b2 * x - c.a2 * y;
        y as f32
    }

    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }
}

// --- matched design ----------------------------------------------------------

/// 8 poles — 48 dB/oct — is four sections. An odd order spends one of them on
/// its single real pole, so the count never exceeds this either way.
pub const MAX_SECTIONS: usize = 4;

/// The Q at which a cut is Butterworth: maximally flat, -3 dB at the corner.
/// Above it a cut resonates; below it the corner rounds off early.
pub const FLAT_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;

const FLAT_Q_F64: f64 = std::f64::consts::FRAC_1_SQRT_2;
const TINY: f64 = 1.0e-300;

/// What [`design`] is being asked for. See [`Shape::poles`] for what each
/// member will accept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    LowPass,
    HighPass,
    LowShelf,
    HighShelf,
    Bell,
    Notch,
    BandPass,
}

impl Shape {
    /// Round a requested pole count to one this shape can actually build.
    pub fn poles(self, requested: usize) -> usize {
        match self {
            // The matched shelf is built from pole/zero pairs — no one-pole form.
            Shape::LowShelf | Shape::HighShelf => match requested {
                0..=2 => 2,
                3..=4 => 4,
                5..=6 => 6,
                _ => 8,
            },
            Shape::Bell | Shape::Notch | Shape::BandPass => 2,
            Shape::LowPass | Shape::HighPass => match requested {
                0..=1 => 1,
                2..=3 => 2,
                4..=5 => 4,
                6..=7 => 6,
                _ => 8,
            },
        }
    }

    /// Does Q shape this response at all? A one-pole cut and every shelf are
    /// fixed by their order alone.
    pub fn uses_q(self, poles: usize) -> bool {
        match self {
            Shape::LowShelf | Shape::HighShelf => false,
            Shape::LowPass | Shape::HighPass => poles > 1,
            _ => true,
        }
    }
}

impl From<crate::params::BandKind> for Shape {
    fn from(kind: crate::params::BandKind) -> Self {
        use crate::params::BandKind;
        match kind {
            // A low cut passes what is above it: the user's name, the filter's.
            BandKind::LowCut => Shape::HighPass,
            BandKind::HighCut => Shape::LowPass,
            BandKind::LowShelf => Shape::LowShelf,
            BandKind::HighShelf => Shape::HighShelf,
            BandKind::Bell => Shape::Bell,
            BandKind::Notch => Shape::Notch,
            BandKind::BandPass => Shape::BandPass,
        }
    }
}

/// Design the cascade one EQ band asks for.
///
/// The single place a band's settings become coefficients — the audio path and
/// the display both call it, which is what keeps the drawn curve honest.
pub fn band_cascade(
    kind: crate::params::BandKind,
    freq: f32,
    sr: f32,
    order: usize,
    q: f32,
    gain_db: f32,
    out: &mut [Coeffs; MAX_SECTIONS],
) -> usize {
    design(Shape::from(kind), freq, sr, order, q, gain_db, out)
}

/// A section mid-solve. Structurally identical to [`Coeffs`]; a separate type
/// so an unchecked section cannot reach the audio path.
#[derive(Clone, Copy)]
struct Sec {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

impl Sec {
    const IDENTITY: Self = Self {
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
        a1: 0.0,
        a2: 0.0,
    };

    /// `|H(e^jw)|` from a precomputed sine and cosine.
    fn magnitude_at(&self, cos_w: f64, sin_w: f64) -> f64 {
        let nr = (self.b0 + self.b2) * cos_w + self.b1;
        let ni = (self.b2 - self.b0) * sin_w;
        let dr = (1.0 + self.a2) * cos_w + self.a1;
        let di = (self.a2 - 1.0) * sin_w;
        nr.hypot(ni) / dr.hypot(di).max(TINY)
    }

    fn coeffs(&self) -> Coeffs {
        Coeffs {
            b0: self.b0,
            b1: self.b1,
            b2: self.b2,
            a1: self.a1,
            a2: self.a2,
        }
    }
}

/// Design one complete cascade into `out`, returning how many sections it used.
///
/// Zero means the request was degenerate and the band should be a straight
/// wire. `gain_db` is read by the shelves and the bell; `q` by everything but
/// the shelves and the one-pole cuts.
pub fn design(
    shape: Shape,
    freq: f32,
    sr: f32,
    poles: usize,
    q: f32,
    gain_db: f32,
    out: &mut [Coeffs; MAX_SECTIONS],
) -> usize {
    if !sr.is_finite() || sr <= 0.0 || !freq.is_finite() || !q.is_finite() || !gain_db.is_finite() {
        return 0;
    }
    let dw = 2.0 * std::f64::consts::PI * freq as f64 / sr as f64;
    // Strictly between DC and Nyquist, as every one of these designs requires.
    if !(dw > 1.0e-9 && dw < std::f64::consts::PI) {
        return 0;
    }
    let q = (q as f64).max(1.0e-6);
    let gain_db = gain_db as f64;
    let poles = shape.poles(poles);

    let mut sections = [Sec::IDENTITY; MAX_SECTIONS];
    let Some(count) = build(shape, dw, poles, q, gain_db, &mut sections) else {
        return 0;
    };

    for (slot, section) in out.iter_mut().zip(sections.iter()).take(count) {
        let finished = section.coeffs();
        // A design that fell apart somewhere; passing it on would be an oscillator.
        if !finished.finite() || !finished.stable() {
            return 0;
        }
        *slot = finished;
    }
    count
}

fn build(
    shape: Shape,
    dw: f64,
    poles: usize,
    q: f64,
    gain_db: f64,
    out: &mut [Sec; MAX_SECTIONS],
) -> Option<usize> {
    match shape {
        Shape::LowPass => {
            let aw = lowpass_analog_omega(poles, dw, q);
            lowpass_sections(poles, dw, aw, q, out)
        }
        Shape::HighPass => {
            let mut n = 0;
            if poles % 2 == 1 {
                let pole = (-compensated_analog_omega(dw)).exp();
                out[n] = Sec {
                    b0: 0.5 * (1.0 + pole),
                    b1: -0.5 * (1.0 + pole),
                    b2: 0.0,
                    a1: -pole,
                    a2: 0.0,
                };
                n += 1;
            }
            let (qs, count) = adjusted_qs(poles, q);
            for &section_q in qs.iter().take(count) {
                out[n] = highpass_section(dw, section_q);
                n += 1;
            }
            Some(n)
        }
        Shape::Bell => {
            out[0] = bell_section(dw, q, gain_db);
            Some(1)
        }
        Shape::Notch => {
            out[0] = notch_section(dw, q);
            Some(1)
        }
        Shape::BandPass => {
            out[0] = bandpass_section(dw, q);
            Some(1)
        }
        Shape::LowShelf | Shape::HighShelf => {
            Some(shelf_sections(shape, poles, dw, gain_db, out))
        }
    }
}

/// Vicanek's closed-form one-pole compensation: the analog corner whose matched
/// digital pole lands -3 dB at `dw`, rather than at a warped image of it.
fn compensated_analog_omega(dw: f64) -> f64 {
    let delta = 4.0 * (dw * 0.5).sin().powi(2);
    let root = (delta * (4.0 + delta)).sqrt();
    -(2.0 / (2.0 + delta + root)).ln()
}

/// Butterworth section Qs for `order` poles, ascending. Empty for order 1,
/// which has no complex pair to describe.
fn butterworth_qs(order: usize) -> ([f64; MAX_SECTIONS], usize) {
    let mut out = [0.0; MAX_SECTIONS];
    let count = (order / 2).min(MAX_SECTIONS);
    for (index, slot) in out.iter_mut().enumerate().take(count) {
        let k = (2 * index + 1) as f64;
        *slot = 1.0 / (2.0 * (k * std::f64::consts::PI / (2.0 * order as f64)).sin());
    }
    // The formula runs the Qs down; ascending keeps section ordering stable.
    out[..count].sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    (out, count)
}

/// Spread one overall Q across the sections of a cascade.
///
/// A Butterworth cascade's section Qs always multiply to `1/sqrt(2)`, so
/// scaling each by `(q / FLAT_Q)^(1/count)` makes the *whole* cascade read `q`
/// at its corner rather than compounding resonance once per biquad.
fn adjusted_qs(order: usize, q: f64) -> ([f64; MAX_SECTIONS], usize) {
    let (mut qs, count) = butterworth_qs(order);
    if count == 0 {
        return (qs, 0);
    }
    let scale = (q / FLAT_Q_F64).powf(1.0 / count as f64);
    for slot in qs.iter_mut().take(count) {
        *slot *= scale;
    }
    (qs, count)
}

/// The digital pole pair for an analog corner at `aw`, plus the two endpoint
/// sums the numerator solves need.
///
/// `dc` and `nyquist` are carried rather than recomputed because both are
/// residues of order `w^2` between terms of order 1, and the solves square and
/// subtract them again. Built from same-signed pieces, every digit survives.
struct Poles {
    a1: f64,
    a2: f64,
    /// `1 + a1 + a2`, exactly `(1 - r)^2 + bend`.
    dc: f64,
    /// `1 - a1 + a2`, exactly `(1 + r)^2 - bend`.
    nyquist: f64,
    /// Pole radius `r`.
    radius: f64,
    /// `4 r sin^2(theta/2)`, negative once the pair has gone real — which is
    /// what lets one expression cover both branches.
    bend: f64,
}

fn matched_poles(aw: f64, q: f64) -> Poles {
    let damping = 1.0 / (2.0 * q);
    let radius = (-damping * aw).exp();
    // 1 - r, without the cancellation of writing it as a subtraction.
    let one_minus_r = -(-damping * aw).exp_m1();

    let (a1, bend) = if damping < 1.0 {
        let angle = aw * (1.0 - damping * damping).sqrt();
        (
            -2.0 * radius * angle.cos(),
            4.0 * radius * (angle * 0.5).sin().powi(2),
        )
    } else {
        // Overdamped: the poles are real, and cosh is what cos becomes.
        let angle = aw * (damping * damping - 1.0).sqrt();
        (
            -2.0 * radius * angle.cosh(),
            -4.0 * radius * (angle * 0.5).sinh().powi(2),
        )
    };
    Poles {
        a1,
        a2: radius * radius,
        dc: one_minus_r * one_minus_r + bend,
        nyquist: (1.0 + radius) * (1.0 + radius) - bend,
        radius,
        bend,
    }
}

impl Poles {
    /// `AA1 - 16 r^2 phi0` as its two difference-of-squares factors,
    /// `(1 - a1 + a2) -/+ 4 r cos(w0/2)`. The sum is harmless; the difference
    /// is 16 against 16, so it is expanded into non-cancelling terms.
    fn nyquist_split(&self, dw: f64) -> (f64, f64) {
        let one_minus_r = 1.0 - self.radius;
        let low = one_minus_r * one_minus_r + 8.0 * self.radius * (dw * 0.25).sin().powi(2)
            - self.bend;
        (low, self.nyquist + 4.0 * self.radius * (dw * 0.5).cos())
    }

    /// `AA1 - AA0 - 16 r^2 cos w0` — the bell and band-pass `r2`. Three
    /// quantities near 16 leaving an `O(w0^2)` residue, expanded into terms
    /// that keep their own sign.
    fn spread(&self, dw: f64) -> f64 {
        let one_minus_r = 1.0 - self.radius;
        let r2 = self.radius * self.radius;
        8.0 * self.radius * one_minus_r * one_minus_r
            + 32.0 * r2 * (dw * 0.5).sin().powi(2)
            - 4.0 * self.bend * (1.0 + r2)
    }
}

/// The pieces every matched numerator solve is written in terms of, arranged so
/// nothing large is subtracted from anything large. Written the obvious way all
/// three solves are `O(w0^4)` residues between terms of order 1 and return noise
/// below a hundredth of a radian — 76 Hz at 48 kHz.
///
/// `A(e^jw0)` is deliberately absent: every solve needs it only minus something
/// that cancels its `AA0 phi0` term again, so each does that subtraction
/// symbolically rather than forming the full value and losing the digits.
struct Numerator {
    /// `sin^2(w0/2)`. `phi0` is `1 - phi1` and is never needed on its own.
    phi1: f64,
    /// `AA0`, exactly `(1 + a1 + a2)^2`.
    aa0: f64,
    /// `AA1 - 16 r^2 phi0`.
    endpoints: f64,
    /// `AA1 - AA0 - 16 r^2 cos w0`.
    spread: f64,
}

fn numerator_terms(dw: f64, poles: &Poles) -> Numerator {
    let (low, high) = poles.nyquist_split(dw);
    Numerator {
        phi1: (dw * 0.5).sin().powi(2),
        aa0: poles.dc * poles.dc,
        endpoints: low * high,
        spread: poles.spread(dw),
    }
}

/// One matched low-pass section: poles from `aw`, numerator solved so the
/// section reads exactly `q` at `dw`.
///
/// `None` when that numerator is not real, which happens only for an analog
/// corner past what the rate can carry — see [`max_valid_analog_omega`].
fn matched_lowpass_section(dw: f64, aw: f64, q: f64) -> Option<Sec> {
    let p = matched_poles(aw, q);
    let n = numerator_terms(dw, &p);

    // `(A(w0) q^2 - AA0 phi0) / phi1`, with the `AA0 phi0` term cancelled by
    // hand: `AA0 phi0 (q^2 - 1) / phi1  +  q^2 (AA1 - 16 r^2 phi0)`.
    let resonance = n.aa0 * (1.0 - n.phi1) * (q * q - 1.0) / n.phi1.max(TINY);
    let endpoints = q * q * n.endpoints;
    let b1_term = resonance + endpoints;

    let tolerance = 1.0e-12 * 1.0f64.max(resonance.abs()).max(endpoints.abs());
    if b1_term < -tolerance {
        return None;
    }
    let cutoff_root = b1_term.max(0.0).sqrt();
    Some(Sec {
        b0: 0.5 * (p.dc + cutoff_root),
        b1: 0.5 * (p.dc - cutoff_root),
        b2: 0.0,
        a1: p.a1,
        a2: p.a2,
    })
}

fn lowpass_sections(
    poles: usize,
    dw: f64,
    aw: f64,
    q: f64,
    out: &mut [Sec; MAX_SECTIONS],
) -> Option<usize> {
    let mut n = 0;
    if poles % 2 == 1 {
        let pole = (-compensated_analog_omega(dw)).exp();
        out[n] = Sec {
            b0: 1.0 - pole,
            b1: 0.0,
            b2: 0.0,
            a1: -pole,
            a2: 0.0,
        };
        n += 1;
    }
    let (qs, count) = adjusted_qs(poles, q);
    for &section_q in qs.iter().take(count) {
        out[n] = matched_lowpass_section(dw, aw, section_q)?;
        n += 1;
    }
    Some(n)
}

/// Largest analog corner for which every section of this cascade still solves.
///
/// Only reached when the interpolated calibration overshoots, which is rare and
/// confined to corners sitting on top of Nyquist.
fn max_valid_analog_omega(poles: usize, dw: f64, q: f64) -> f64 {
    let ok = |aw: f64| {
        let mut scratch = [Sec::IDENTITY; MAX_SECTIONS];
        lowpass_sections(poles, dw, aw, q, &mut scratch).is_some()
    };
    let ceiling = std::f64::consts::PI * (1.0 - 1.0e-8);
    if ok(ceiling) {
        return ceiling;
    }
    let (mut lower, mut upper) = (1.0e-6, ceiling);
    for _ in 0..42 {
        let middle = 0.5 * (lower + upper);
        if ok(middle) {
            lower = middle;
        } else {
            upper = middle;
        }
    }
    lower
}

/// The analog corner the low-pass cascade is built around.
///
/// The corner gain lands on `q` whatever this is — the numerator solve sees to
/// that — but the passband shape depends on it entirely, and near Nyquist a bad
/// choice costs thirty dB. See [`dewarp`].
fn lowpass_analog_omega(poles: usize, dw: f64, q: f64) -> f64 {
    let base = compensated_analog_omega(dw);
    if poles < 2 {
        return base;
    }
    let candidate = base * dewarp::ratio(poles, dw, q);
    let mut scratch = [Sec::IDENTITY; MAX_SECTIONS];
    if candidate.is_finite() && lowpass_sections(poles, dw, candidate, q, &mut scratch).is_some() {
        candidate
    } else {
        max_valid_analog_omega(poles, dw, q) * 0.999
    }
}

/// One matched high-pass section: zeros both at DC, so the design is the pole
/// pair plus a scale set at Nyquist. No calibration search — it is matched at
/// both endpoints by construction, and the analog corner is the digital one.
fn highpass_section(dw: f64, q: f64) -> Sec {
    let p = matched_poles(dw, q);
    let normalized = dw / std::f64::consts::PI;
    let analog = ((1.0 - normalized * normalized).powi(2) + normalized * normalized / (q * q))
        .max(1.0e-12)
        .sqrt();
    let b0 = p.nyquist.abs() / analog / 4.0;
    Sec {
        b0,
        b1: -2.0 * b0,
        b2: b0,
        a1: p.a1,
        a2: p.a2,
    }
}

/// One matched bell. Unity at DC and Nyquist, exactly `gain_db` at `dw`.
///
/// Deliberately has no low-frequency fallback to the RBJ bell. The two do not
/// share a Q — this puts poles at `sqrt(gain * q)` and zeros at
/// `q / sqrt(gain)`, RBJ's sit at `gain * q` and `q / gain` — so a guard there
/// swaps the filter for a different one and steps the response by a dB at the
/// crossover. The terms are rearranged instead, as in the low-pass solve.
fn bell_section(dw: f64, q: f64, gain_db: f64) -> Sec {
    if gain_db < 0.0 {
        // A cut is the reciprocal of the boost it undoes, so the two cancel.
        let boost = bell_section(dw, q, -gain_db);
        let scale = 1.0 / boost.b0;
        return Sec {
            b0: scale,
            b1: boost.a1 * scale,
            b2: boost.a2 * scale,
            a1: boost.b1 * scale,
            a2: boost.b2 * scale,
        };
    }

    let gain = 10f64.powf(gain_db / 20.0);
    let gain2 = gain * gain;
    // The requested Q is the geometric mean of the pole and zero Qs.
    let p = matched_poles(dw, (gain * q).max(1.0e-9).sqrt());
    let n = numerator_terms(dw, &p);

    // `(A(w0) g^2 - AA0) - phi1 g^2 spread`, with `g^2 phi0 - 1` written as
    // `(g^2 - 1) - g^2 phi1` so it stays exact at unity gain and any other.
    let bb2 = (n.aa0 * ((gain2 - 1.0) - gain2 * n.phi1)
        + gain2 * n.phi1 * (n.endpoints - n.spread))
        / (4.0 * n.phi1 * n.phi1).max(TINY);
    let bb1 = gain2 * n.spread + n.aa0 - 4.0 * dw.cos() * bb2;

    // `sqrt(AA0)` is `1 + a1 + a2`, already known exactly and positive.
    let nyquist_root = bb1.max(0.0).sqrt();
    let width = 0.5 * (p.dc + nyquist_root);
    let b0 = 0.5 * (width + (width * width + bb2).max(0.0).sqrt());
    Sec {
        b0,
        b1: 0.5 * (p.dc - nyquist_root),
        b2: -bb2 / (4.0 * b0),
        a1: p.a1,
        a2: p.a2,
    }
}

/// One matched notch: a true zero on the unit circle at `dw`, with the pole
/// radius chosen so the skirts leave DC and Nyquist where the analog notch does.
fn notch_section(dw: f64, q: f64) -> Sec {
    let zero_cosine = dw.cos();
    let dc_numerator = (2.0 - 2.0 * zero_cosine).max(1.0e-12);
    let nyquist_numerator = (2.0 + 2.0 * zero_cosine).max(1.0e-12);

    let ratio = std::f64::consts::PI / dw;
    let resonant = (1.0 - ratio * ratio).powi(2);
    let analog_nyquist =
        (1.0 - ratio * ratio).abs() / (resonant + (ratio / q).powi(2)).sqrt();

    let endpoint_ratio = analog_nyquist * dc_numerator / nyquist_numerator;
    let balance = ((1.0 - endpoint_ratio) / (1.0 + endpoint_ratio).max(1.0e-12)).abs();
    let matched_radius = (-dw / (2.0 * q)).exp();
    let endpoint_radius = if balance < 1.0e-9 {
        0.0
    } else {
        (1.0 - (1.0 - balance * balance).max(0.0).sqrt()) / balance
    };
    let radius = matched_radius.max(endpoint_radius + 1.0e-6).min(0.9999);
    let pole_cosine = ((1.0 + radius * radius) * (1.0 - endpoint_ratio)
        / (2.0 * radius * (1.0 + endpoint_ratio)).max(1.0e-12))
    .clamp(-1.0, 1.0);

    let a1 = -2.0 * radius * pole_cosine;
    let a2 = radius * radius;
    let scale = (1.0 + a1 + a2) / dc_numerator;
    Sec {
        b0: scale,
        b1: -2.0 * scale * zero_cosine,
        b2: scale,
        a1,
        a2,
    }
}

/// One matched band-pass: unity at its centre, zeros at DC and Nyquist.
fn bandpass_section(dw: f64, q: f64) -> Sec {
    let p = matched_poles(dw, q);
    let n = numerator_terms(dw, &p);

    // `A(w0) - phi1 spread`, expanded so its terms are never formed at full
    // size — the bell's cancellation, and the same cure.
    let bb2 = (n.aa0 * (1.0 - n.phi1) + n.phi1 * (n.endpoints - n.spread))
        / (4.0 * n.phi1 * n.phi1).max(TINY);
    let bb1 = n.spread - 4.0 * dw.cos() * bb2;

    let b1 = -0.5 * bb1.max(0.0).sqrt();
    let b0 = 0.5 * ((bb2 + b1 * b1).max(0.0).sqrt() - b1);
    Sec {
        b0,
        b1,
        b2: -b0 - b1,
        a1: p.a1,
        a2: p.a2,
    }
}

// --- shelves -----------------------------------------------------------------

/// Everything about a shelf cascade that does not move while its corner is
/// solved for.
///
/// Poles and zeros sit on two Butterworth arcs of the same order, one scaled up
/// by the gain and one down, so the transition has the order's steepness and the
/// height falls out of the ratio between them. Only where the arcs sit moves;
/// their angles depend on section index and order alone, so hoisting them out of
/// the search is most of what makes it affordable.
struct ShelfArcs {
    count: usize,
    /// `(sin, cos)` of each section's angle around the arc.
    angles: [(f64, f64); MAX_SECTIONS],
    /// How far apart the two arcs sit, which is the shelf's height.
    radius_ratio: f64,
    /// DC level, taken in equal shares across the sections.
    dc_gain: f64,
}

fn shelf_arcs(shape: Shape, order: usize, gain_db: f64) -> ShelfArcs {
    let count = (order / 2).clamp(1, MAX_SECTIONS);
    let requested_gain = 10f64.powf(gain_db / 20.0);
    let (shelf_gain, dc_gain) = match shape {
        // A low shelf is a high shelf of the reciprocal gain, lifted to unity.
        Shape::LowShelf => (1.0 / requested_gain, requested_gain.powf(1.0 / count as f64)),
        _ => (requested_gain, 1.0),
    };
    let mut angles = [(0.0, 0.0); MAX_SECTIONS];
    for (index, slot) in angles.iter_mut().enumerate().take(count) {
        let angle = std::f64::consts::FRAC_PI_2
            + (2 * index + 1) as f64 * std::f64::consts::PI / (2.0 * order as f64);
        *slot = angle.sin_cos();
    }
    ShelfArcs {
        count,
        angles,
        radius_ratio: shelf_gain.powf(1.0 / (2.0 * order as f64)),
        dc_gain,
    }
}

/// Place the arcs at one trial corner and write out the sections.
fn shelf_trial(arcs: &ShelfArcs, center: f64, out: &mut [Sec; MAX_SECTIONS]) -> usize {
    let pole_omega = center * arcs.radius_ratio;
    let zero_omega = center / arcs.radius_ratio;

    for (slot, &(sin_angle, cos_angle)) in out.iter_mut().zip(arcs.angles.iter()).take(arcs.count) {
        let pole_radius = (pole_omega * cos_angle).exp();
        let zero_radius = (zero_omega * cos_angle).exp();

        let b1 = -2.0 * zero_radius * (zero_omega * sin_angle).cos();
        let b2 = zero_radius * zero_radius;
        let a1 = -2.0 * pole_radius * (pole_omega * sin_angle).cos();
        let a2 = pole_radius * pole_radius;
        // Unity at DC per section, then the shelf's own DC level in equal shares.
        let scale = (1.0 + a1 + a2) / (1.0 + b1 + b2).max(1.0e-18) * arcs.dc_gain;
        *slot = Sec {
            b0: scale,
            b1: b1 * scale,
            b2: b2 * scale,
            a1,
            a2,
        };
    }
    arcs.count
}

/// Solve for the corner that puts half the shelf's gain at `dw`.
///
/// Centre gain is monotone in the corner, so this is a root find. Regula falsi
/// with Illinois damping: the response saturates well before the ends of the
/// bracket, where plain false position would crawl. A handful of evaluations
/// for a millionth of a dB, against thirty for bisection — and this runs on the
/// audio thread every block the shelf is moving.
fn shelf_sections(
    shape: Shape,
    order: usize,
    dw: f64,
    gain_db: f64,
    out: &mut [Sec; MAX_SECTIONS],
) -> usize {
    let count = (order / 2).clamp(1, MAX_SECTIONS);
    if gain_db.abs() < 1.0e-4 {
        // Poles and zeros coincide at unity gain; nothing to solve, nothing to do.
        for slot in out.iter_mut().take(count) {
            *slot = Sec::IDENTITY;
        }
        return count;
    }

    let arcs = shelf_arcs(shape, order, gain_db);
    let (sin_w, cos_w) = dw.sin_cos();
    let half_tangent = (dw * 0.5).tan();
    let target = gain_db * 0.5;

    // Search in octaves from the requested corner, not in the corner itself.
    let corner_for = |offset: f64| 2.0 * (half_tangent * offset.exp2()).atan();
    let mut scratch = [Sec::IDENTITY; MAX_SECTIONS];
    let mut error = |offset: f64| {
        let n = shelf_trial(&arcs, corner_for(offset), &mut scratch);
        let mut magnitude = 1.0;
        for section in scratch.iter().take(n) {
            magnitude *= section.magnitude_at(cos_w, sin_w);
        }
        20.0 * magnitude.max(TINY).log10() - target
    };

    // The response has flattened onto both shelf levels long before ten octaves
    // out, so the bracket always contains the crossing.
    let (mut lower, mut upper) = (-10.0f64, 10.0f64);
    let (mut at_lower, mut at_upper) = (error(lower), error(upper));
    let mut offset = 0.0;
    if at_lower.is_finite() && at_upper.is_finite() && (at_lower > 0.0) != (at_upper > 0.0) {
        for _ in 0..24 {
            offset = upper - at_upper * (upper - lower) / (at_upper - at_lower);
            if !offset.is_finite() {
                offset = 0.5 * (lower + upper);
            }
            let at_offset = error(offset);
            if at_offset.abs() < 1.0e-6 {
                break;
            }
            if (at_offset > 0.0) != (at_upper > 0.0) {
                lower = upper;
                at_lower = at_upper;
            } else {
                // Illinois: halve the stale endpoint so the bracket closes from
                // both sides rather than crawling in from one.
                at_lower *= 0.5;
            }
            upper = offset;
            at_upper = at_offset;
        }
    }
    shelf_trial(&arcs, corner_for(offset), out)
}

#[cfg(test)]
mod bench;
#[cfg(test)]
mod tests;
