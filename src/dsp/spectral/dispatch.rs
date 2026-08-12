//! Runtime-dispatched kernels for the spectral detector's hot loops.
//!
//! The detector's per-pass work is a handful of embarrassingly parallel
//! per-bin operations — windowing, power spectra, dB conversion, smoothing,
//! prominence — plus an FFT that already carries its own SIMD (rustfft
//! runtime-dispatches SSE/AVX internally, so it is deliberately not
//! reimplemented here). This module gives those per-bin loops the same
//! treatment: one scalar implementation that always exists, and x86-64
//! AVX2 / AVX2+FMA implementations selected **at runtime** so the plugin
//! still loads and runs on CPUs without them.
//!
//! The kernels only ever run on the analysis worker thread. Nothing here is
//! called from the audio callback, so even the (cached) feature detection is
//! comfortably off the realtime path.
//!
//! Numerical parity is part of the contract: every backend uses the same
//! [`fast_log2`](super::scalar::fast_log2) polynomial, so scalar and SIMD
//! disagree only by FMA rounding — tested in `mod.rs` to fractions of a
//! millidecibel, which keeps detection decisions identical across machines.
//!
//! An ARM64/NEON backend slots in beside the x86 one — see
//! [`super::arm_neon`] for the intended shape. No target intrinsics leak
//! into the detector logic itself; it only ever sees this table.

use realfft::num_complex::Complex32;

/// Which derived channel a power spectrum should describe, in
/// [`crate::params::BandChannel`] variant order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PowerMode {
    Stereo,
    Mid,
    Side,
    Left,
    Right,
}

impl PowerMode {
    pub fn from_channel(ch: usize) -> Self {
        match ch {
            1 => PowerMode::Mid,
            2 => PowerMode::Side,
            3 => PowerMode::Left,
            4 => PowerMode::Right,
            _ => PowerMode::Stereo,
        }
    }
}

/// The kernel table one detector instance runs on. Plain function pointers —
/// chosen once, no per-call branching, trivially swappable in tests.
#[derive(Clone, Copy)]
pub struct Kernels {
    pub name: &'static str,
    /// `dst[i] = src[i] * window[i]`.
    pub window: fn(dst: &mut [f32], src: &[f32], window: &[f32]),
    /// Power spectrum of a derived channel from the L/R complex spectra.
    pub power: fn(l: &[Complex32], r: &[Complex32], mode: PowerMode, out: &mut [f32]),
    /// `out[i] = 10·log10(power[i]) + offset`, floored at −120 dB.
    pub power_db: fn(power: &[f32], offset_db: f32, out: &mut [f32]),
    /// Asymmetric one-pole per bin: `state += (fresh − state) · a`, `a`
    /// picked by direction.
    pub smooth: fn(state: &mut [f32], fresh: &[f32], a_up: f32, a_dn: f32),
    /// `out[i] = a[i] − b[i]` — prominence over the baseline.
    pub subtract: fn(a: &[f32], b: &[f32], out: &mut [f32]),
}

/// The backend this machine should run. Detected once per call site's first
/// use; detection itself is cheap and only ever happens off the audio thread.
pub fn select() -> Kernels {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            if std::arch::is_x86_feature_detected!("fma") {
                return super::x86_avx2::AVX2_FMA;
            }
            return super::x86_avx2::AVX2;
        }
    }
    super::scalar::SCALAR
}

/// Every backend this machine can run, strongest last — what the parity tests
/// and the benchmark iterate over.
pub fn available() -> Vec<Kernels> {
    let mut list = vec![super::scalar::SCALAR];
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            list.push(super::x86_avx2::AVX2);
            if std::arch::is_x86_feature_detected!("fma") {
                list.push(super::x86_avx2::AVX2_FMA);
            }
        }
    }
    list
}
