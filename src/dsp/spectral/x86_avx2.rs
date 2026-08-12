//! AVX2 and AVX2+FMA kernel backends for x86-64.
//!
//! Every function here processes eight bins per iteration and hands the
//! remainder to the scalar backend, whose formulas it shares exactly — the
//! same [`LOG2_POLY`](super::scalar::LOG2_POLY), the same floors — so the
//! backends differ only by FMA rounding. These are worker-thread kernels;
//! nothing here runs on the audio callback.
//!
//! The unsafe surface is kept to this file. Each public entry is a safe fn
//! whose body calls one `#[target_feature]` function; safety rests on the
//! dispatch table only handing these out after
//! `is_x86_feature_detected!` said yes, which is the contract documented on
//! [`super::dispatch::select`].

#![cfg(target_arch = "x86_64")]

use core::arch::x86_64::*;

use realfft::num_complex::Complex32;

use super::dispatch::{Kernels, PowerMode};
use super::scalar::{self, DB_FLOOR, DB_PER_LOG2, LOG2_POLY};

// --- windowing ---------------------------------------------------------------

#[target_feature(enable = "avx2")]
unsafe fn window_avx2(dst: &mut [f32], src: &[f32], win: &[f32]) {
    let n = dst.len().min(src.len()).min(win.len());
    let mut i = 0;
    while i + 8 <= n {
        let s = _mm256_loadu_ps(src.as_ptr().add(i));
        let w = _mm256_loadu_ps(win.as_ptr().add(i));
        _mm256_storeu_ps(dst.as_mut_ptr().add(i), _mm256_mul_ps(s, w));
        i += 8;
    }
    for j in i..n {
        dst[j] = src[j] * win[j];
    }
}

// --- power spectra -----------------------------------------------------------

/// Powers of eight complex bins starting at `ptr`: square everything, sum
/// adjacent (re², im²) pairs, and undo `hadd`'s lane interleave.
#[inline(always)]
unsafe fn eight_powers(ptr: *const f32) -> __m256 {
    let a = _mm256_loadu_ps(ptr);
    let b = _mm256_loadu_ps(ptr.add(8));
    let h = _mm256_hadd_ps(_mm256_mul_ps(a, a), _mm256_mul_ps(b, b));
    // h = [p0 p1 p4 p5 | p2 p3 p6 p7] in 64-bit pairs — permute to order.
    _mm256_castpd_ps(_mm256_permute4x64_pd(_mm256_castps_pd(h), 0b11_01_10_00))
}

/// The same for `(l ± r) · 0.5` — the mid/side derivations.
#[inline(always)]
unsafe fn eight_powers_combined(l: *const f32, r: *const f32, sub: bool) -> __m256 {
    let half = _mm256_set1_ps(0.5);
    let combine = |x: __m256, y: __m256| {
        if sub {
            _mm256_mul_ps(_mm256_sub_ps(x, y), half)
        } else {
            _mm256_mul_ps(_mm256_add_ps(x, y), half)
        }
    };
    let a = combine(_mm256_loadu_ps(l), _mm256_loadu_ps(r));
    let b = combine(_mm256_loadu_ps(l.add(8)), _mm256_loadu_ps(r.add(8)));
    let h = _mm256_hadd_ps(_mm256_mul_ps(a, a), _mm256_mul_ps(b, b));
    _mm256_castpd_ps(_mm256_permute4x64_pd(_mm256_castps_pd(h), 0b11_01_10_00))
}

#[target_feature(enable = "avx2")]
unsafe fn power_avx2(l: &[Complex32], r: &[Complex32], mode: PowerMode, out: &mut [f32]) {
    let n = out.len().min(l.len()).min(r.len());
    // Complex32 is repr(C) { re, im }, so a spectrum is interleaved f32 pairs.
    let lp = l.as_ptr() as *const f32;
    let rp = r.as_ptr() as *const f32;
    let mut i = 0;
    while i + 8 <= n {
        let v = match mode {
            PowerMode::Stereo => _mm256_mul_ps(
                _mm256_add_ps(eight_powers(lp.add(i * 2)), eight_powers(rp.add(i * 2))),
                _mm256_set1_ps(0.5),
            ),
            PowerMode::Mid => eight_powers_combined(lp.add(i * 2), rp.add(i * 2), false),
            PowerMode::Side => eight_powers_combined(lp.add(i * 2), rp.add(i * 2), true),
            PowerMode::Left => eight_powers(lp.add(i * 2)),
            PowerMode::Right => eight_powers(rp.add(i * 2)),
        };
        _mm256_storeu_ps(out.as_mut_ptr().add(i), v);
        i += 8;
    }
    if i < n {
        (scalar::SCALAR.power)(&l[i..n], &r[i..n], mode, &mut out[i..n]);
    }
}

// --- dB conversion -----------------------------------------------------------

/// `log2` of eight positive floats: exponent from the bit pattern, the shared
/// cubic on the mantissa. `fma` statically selects the instruction; both
/// monomorphizations are compiled under their own target features.
#[inline(always)]
unsafe fn log2_lanes<const FMA: bool>(x: __m256) -> __m256 {
    let bits = _mm256_castps_si256(x);
    let exp = _mm256_sub_epi32(_mm256_srli_epi32::<23>(bits), _mm256_set1_epi32(127));
    let mant = _mm256_castsi256_ps(_mm256_or_si256(
        _mm256_and_si256(bits, _mm256_set1_epi32(0x007f_ffff)),
        _mm256_set1_epi32(0x3f80_0000),
    ));
    let c0 = _mm256_set1_ps(LOG2_POLY[0]);
    let c1 = _mm256_set1_ps(LOG2_POLY[1]);
    let c2 = _mm256_set1_ps(LOG2_POLY[2]);
    let c3 = _mm256_set1_ps(LOG2_POLY[3]);
    let poly = if FMA {
        _mm256_fmadd_ps(
            _mm256_fmadd_ps(_mm256_fmadd_ps(c3, mant, c2), mant, c1),
            mant,
            c0,
        )
    } else {
        _mm256_add_ps(
            _mm256_mul_ps(
                _mm256_add_ps(
                    _mm256_mul_ps(_mm256_add_ps(_mm256_mul_ps(c3, mant), c2), mant),
                    c1,
                ),
                mant,
            ),
            c0,
        )
    };
    _mm256_add_ps(_mm256_cvtepi32_ps(exp), poly)
}

#[inline(always)]
unsafe fn power_db_lanes<const FMA: bool>(power: &[f32], offset_db: f32, out: &mut [f32]) {
    let n = out.len().min(power.len());
    let scale = _mm256_set1_ps(DB_PER_LOG2);
    let offset = _mm256_set1_ps(offset_db);
    let floor_in = _mm256_set1_ps(1e-30);
    let floor_db = _mm256_set1_ps(DB_FLOOR);
    let mut i = 0;
    while i + 8 <= n {
        let p = _mm256_max_ps(_mm256_loadu_ps(power.as_ptr().add(i)), floor_in);
        let l2 = log2_lanes::<FMA>(p);
        let db = if FMA {
            _mm256_fmadd_ps(l2, scale, offset)
        } else {
            _mm256_add_ps(_mm256_mul_ps(l2, scale), offset)
        };
        _mm256_storeu_ps(out.as_mut_ptr().add(i), _mm256_max_ps(db, floor_db));
        i += 8;
    }
    if i < n {
        (scalar::SCALAR.power_db)(&power[i..n], offset_db, &mut out[i..n]);
    }
}

#[target_feature(enable = "avx2")]
unsafe fn power_db_avx2(power: &[f32], offset_db: f32, out: &mut [f32]) {
    power_db_lanes::<false>(power, offset_db, out);
}

#[target_feature(enable = "avx2,fma")]
unsafe fn power_db_avx2_fma(power: &[f32], offset_db: f32, out: &mut [f32]) {
    power_db_lanes::<true>(power, offset_db, out);
}

// --- per-bin smoothing and prominence ---------------------------------------

#[target_feature(enable = "avx2")]
unsafe fn smooth_avx2(state: &mut [f32], fresh: &[f32], a_up: f32, a_dn: f32) {
    let n = state.len().min(fresh.len());
    let up = _mm256_set1_ps(a_up);
    let dn = _mm256_set1_ps(a_dn);
    let mut i = 0;
    while i + 8 <= n {
        let s = _mm256_loadu_ps(state.as_ptr().add(i));
        let x = _mm256_loadu_ps(fresh.as_ptr().add(i));
        let rising = _mm256_cmp_ps::<_CMP_GT_OQ>(x, s);
        let a = _mm256_blendv_ps(dn, up, rising);
        let next = _mm256_add_ps(s, _mm256_mul_ps(_mm256_sub_ps(x, s), a));
        _mm256_storeu_ps(state.as_mut_ptr().add(i), next);
        i += 8;
    }
    if i < n {
        (scalar::SCALAR.smooth)(&mut state[i..n], &fresh[i..n], a_up, a_dn);
    }
}

#[target_feature(enable = "avx2")]
unsafe fn subtract_avx2(a: &[f32], b: &[f32], out: &mut [f32]) {
    let n = out.len().min(a.len()).min(b.len());
    let mut i = 0;
    while i + 8 <= n {
        let x = _mm256_loadu_ps(a.as_ptr().add(i));
        let y = _mm256_loadu_ps(b.as_ptr().add(i));
        _mm256_storeu_ps(out.as_mut_ptr().add(i), _mm256_sub_ps(x, y));
        i += 8;
    }
    if i < n {
        (scalar::SCALAR.subtract)(&a[i..n], &b[i..n], &mut out[i..n]);
    }
}

// --- safe wrappers and the tables -------------------------------------------
//
// Callers reach these through the dispatch table, which only selects them
// after runtime detection — the invariant the `unsafe` blocks stand on.

fn window(dst: &mut [f32], src: &[f32], win: &[f32]) {
    unsafe { window_avx2(dst, src, win) }
}

fn power(l: &[Complex32], r: &[Complex32], mode: PowerMode, out: &mut [f32]) {
    unsafe { power_avx2(l, r, mode, out) }
}

fn power_db(power: &[f32], offset_db: f32, out: &mut [f32]) {
    unsafe { power_db_avx2(power, offset_db, out) }
}

fn power_db_fma(power: &[f32], offset_db: f32, out: &mut [f32]) {
    unsafe { power_db_avx2_fma(power, offset_db, out) }
}

fn smooth(state: &mut [f32], fresh: &[f32], a_up: f32, a_dn: f32) {
    unsafe { smooth_avx2(state, fresh, a_up, a_dn) }
}

fn subtract(a: &[f32], b: &[f32], out: &mut [f32]) {
    unsafe { subtract_avx2(a, b, out) }
}

pub const AVX2: Kernels = Kernels {
    name: "avx2",
    window,
    power,
    power_db,
    smooth,
    subtract,
};

pub const AVX2_FMA: Kernels = Kernels {
    name: "avx2+fma",
    window,
    power,
    power_db: power_db_fma,
    smooth,
    subtract,
};
