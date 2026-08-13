//! Audio-thread cost, measured rather than assumed. [`super::design`] runs once
//! per 32-sample block for every band that moved — 0.17 ms at 192 kHz, shared
//! with the resonance stage and the analyser.
//!
//! `cargo test --release -- --ignored --nocapture bench`

use super::*;
use std::time::Instant;

fn time<F: FnMut()>(label: &str, iterations: usize, mut body: F) -> f64 {
    // Warm up, so the first design's page faults are not in the number.
    for _ in 0..iterations / 10 {
        body();
    }
    let start = Instant::now();
    for _ in 0..iterations {
        body();
    }
    let per = start.elapsed().as_secs_f64() / iterations as f64;
    println!("  {label:<34} {:>8.3} us", per * 1.0e6);
    per
}

#[test]
#[ignore = "timing, not correctness"]
fn a_design_fits_inside_a_control_block() {
    let mut out = [Coeffs::identity(); MAX_SECTIONS];
    // Walk the table rather than sitting on one cache line, as automation does.
    let mut step = 0usize;
    let mut next = || {
        step = step.wrapping_add(1);
        let freq = 20.0 * (22_000.0f32 / 20.0).powf((step % 997) as f32 / 997.0);
        let q = 0.1 * 100f32.powf((step % 401) as f32 / 401.0);
        (freq, q)
    };

    println!("per design, 48 kHz:");
    let mut worst = 0.0f64;
    for (label, shape, poles) in [
        ("low cut, 12 dB/oct", Shape::HighPass, 2usize),
        ("low cut, 48 dB/oct", Shape::HighPass, 8),
        ("high cut, 48 dB/oct", Shape::LowPass, 8),
        ("high cut, 6 dB/oct", Shape::LowPass, 1),
        ("bell", Shape::Bell, 2),
        ("notch", Shape::Notch, 2),
        ("band pass", Shape::BandPass, 2),
        ("low shelf, 12 dB/oct", Shape::LowShelf, 2),
        ("high shelf, 48 dB/oct", Shape::HighShelf, 8),
    ] {
        let per = time(label, 20_000, || {
            let (freq, q) = next();
            std::hint::black_box(design(shape, freq, 48_000.0, poles, q, 6.0, &mut out));
        });
        worst = worst.max(per);
    }

    // Every band moving at once, on the tightest block the plugin supports.
    let block = 32.0 / 192_000.0;
    let all_bands = worst * 24.0;
    println!(
        "\n  worst design {:.3} us; 24 of them {:.3} us against a {:.1} us block at 192 kHz \
         ({:.1}% of it)",
        worst * 1.0e6,
        all_bands * 1.0e6,
        block * 1.0e6,
        100.0 * all_bands / block
    );
    assert!(
        all_bands < block * 0.5,
        "redesigning every band would take {:.1}% of a control block",
        100.0 * all_bands / block
    );
}

#[test]
#[ignore = "timing, not correctness"]
fn filtering_a_block_stays_well_inside_real_time() {
    // The other half of the budget: running samples through the coefficients,
    // which is what f64 sections cost — see [`Coeffs`].
    let coeffs = {
        let mut out = [Coeffs::identity(); MAX_SECTIONS];
        let n = design(Shape::HighPass, 40.0, 48_000.0, 8, FLAT_Q, 0.0, &mut out);
        assert_eq!(n, MAX_SECTIONS);
        out
    };

    // The fullest chain the plugin can be asked for.
    const BANDS: usize = 24;
    const CHANNELS: usize = 2;
    let mut states = vec![[Biquad::new(); MAX_SECTIONS]; BANDS * CHANNELS];
    let mut buffer = [0.0f32; 32];
    for (i, x) in buffer.iter_mut().enumerate() {
        *x = (i as f32 * 0.37).sin() * 0.25;
    }

    let blocks = 20_000;
    let start = Instant::now();
    for _ in 0..blocks {
        for bank in states.iter_mut() {
            for section in bank.iter_mut() {
                for x in buffer.iter_mut() {
                    *x = section.process(*x, &coeffs[0]);
                }
            }
        }
        std::hint::black_box(&buffer);
    }
    let elapsed = start.elapsed().as_secs_f64();

    let samples = (blocks * buffer.len()) as f64;
    let per_sample = elapsed / samples;
    let sections = (BANDS * CHANNELS * MAX_SECTIONS) as f64;
    println!(
        "  {sections:.0} sections/sample: {:.2} ns per sample, \
         {:.1}% of real time at 192 kHz",
        per_sample * 1.0e9,
        100.0 * per_sample * 192_000.0
    );
    println!("  {:.2} ns per section-sample", per_sample * 1.0e9 / sections);
    assert!(
        per_sample * 192_000.0 < 0.5,
        "the full chain would take {:.1}% of real time at 192 kHz",
        100.0 * per_sample * 192_000.0
    );
}
