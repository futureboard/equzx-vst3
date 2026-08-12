//! The scalar kernel backend — the reference implementation every SIMD
//! backend is tested against, and the fallback that always exists.

use realfft::num_complex::Complex32;

use super::dispatch::{Kernels, PowerMode};

/// dB per unit of log2 — `10·log10(x) = DB_PER_LOG2 · log2(x)`.
pub const DB_PER_LOG2: f32 = 3.010_3;

/// Nothing the detector measures is quieter than this.
pub const DB_FLOOR: f32 = -120.0;

/// Cubic minimax fit of `log2(m)` over `m ∈ [1, 2)`, evaluated as
/// `C0 + m·(C1 + m·(C2 + m·C3))`.
///
/// Max absolute error ≈ 0.005 in log2 — 0.015 dB, far below anything the
/// detector's thresholds can resolve — and strictly monotonic, so peak
/// ordering is preserved. Shared verbatim with the SIMD backends: parity in
/// the polynomial is what keeps detection decisions identical across them.
pub const LOG2_POLY: [f32; 4] = [-2.153_620_7, 3.047_884_2, -1.051_875, 0.157_611_35];

/// `log2(x)` by exponent extraction and the cubic above. `x` must be > 0;
/// callers floor at 1e-30 first.
#[inline(always)]
pub fn fast_log2(x: f32) -> f32 {
    let bits = x.to_bits();
    let exp = ((bits >> 23) & 0xff) as i32 - 127;
    let m = f32::from_bits((bits & 0x007f_ffff) | 0x3f80_0000);
    let poly = LOG2_POLY[0] + m * (LOG2_POLY[1] + m * (LOG2_POLY[2] + m * LOG2_POLY[3]));
    exp as f32 + poly
}

fn window(dst: &mut [f32], src: &[f32], window: &[f32]) {
    for ((d, s), w) in dst.iter_mut().zip(src).zip(window) {
        *d = s * w;
    }
}

fn power(l: &[Complex32], r: &[Complex32], mode: PowerMode, out: &mut [f32]) {
    match mode {
        PowerMode::Stereo => {
            for ((o, a), b) in out.iter_mut().zip(l).zip(r) {
                *o = 0.5 * (a.norm_sqr() + b.norm_sqr());
            }
        }
        PowerMode::Mid => {
            for ((o, a), b) in out.iter_mut().zip(l).zip(r) {
                let m = (a + b) * 0.5;
                *o = m.norm_sqr();
            }
        }
        PowerMode::Side => {
            for ((o, a), b) in out.iter_mut().zip(l).zip(r) {
                let s = (a - b) * 0.5;
                *o = s.norm_sqr();
            }
        }
        PowerMode::Left => {
            for (o, a) in out.iter_mut().zip(l) {
                *o = a.norm_sqr();
            }
        }
        PowerMode::Right => {
            for (o, b) in out.iter_mut().zip(r) {
                *o = b.norm_sqr();
            }
        }
    }
}

fn power_db(power: &[f32], offset_db: f32, out: &mut [f32]) {
    for (o, p) in out.iter_mut().zip(power) {
        *o = (DB_PER_LOG2 * fast_log2(p.max(1e-30)) + offset_db).max(DB_FLOOR);
    }
}

fn smooth(state: &mut [f32], fresh: &[f32], a_up: f32, a_dn: f32) {
    for (s, x) in state.iter_mut().zip(fresh) {
        let a = if *x > *s { a_up } else { a_dn };
        *s += (*x - *s) * a;
    }
}

fn subtract(a: &[f32], b: &[f32], out: &mut [f32]) {
    for ((o, x), y) in out.iter_mut().zip(a).zip(b) {
        *o = x - y;
    }
}

pub const SCALAR: Kernels = Kernels {
    name: "scalar",
    window,
    power,
    power_db,
    smooth,
    subtract,
};
