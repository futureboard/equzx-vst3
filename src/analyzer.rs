//! The spectrum analyser feeding the UI.
//!
//! The audio thread writes mono sums into a lock-free ring; the editor thread
//! pulls the most recent window out of it, runs an FFT, and reduces the result
//! to a log-spaced curve.
//!
//! Reducing on this side is the whole point. A raw 8192-point FFT is 4096 bins,
//! and pushing that through the webview's `evaluate_script` bridge sixty times a
//! second would cost megabytes per second of JSON. The display is logarithmic
//! anyway, so [`LOG_POINTS`] log-spaced values carry everything it can draw —
//! quantised to a byte each and base64'd, one frame of both curves is about
//! 1.4 kB.

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use base64::engine::general_purpose::STANDARD_NO_PAD;
use base64::Engine as _;
use realfft::{num_complex::Complex32, RealFftPlanner, RealToComplex};
use std::sync::Arc;

/// Analysis window. 8192 points is 5.9 Hz per bin at 48 kHz — enough resolution
/// that the bottom octave still has bins to interpolate between.
pub const FFT_SIZE: usize = 8192;

/// Ring capacity. Comfortably more than one window so a late editor frame still
/// finds a contiguous, untorn history behind the write head.
const RING_SIZE: usize = FFT_SIZE * 4;

/// Points in the curve sent to the UI, log-spaced across [`F_MIN`]..`f_max`.
pub const LOG_POINTS: usize = 512;

pub const F_MIN: f32 = 20.0;
/// Matches the display's upper limit in `EQDisplay.tsx`; clamped to Nyquist.
pub const F_MAX: f32 = 22_000.0;

/// dB window the byte quantisation spans. Mirrors the `minDecibels`/`maxDecibels`
/// the prototype's `AnalyserNode` used, so the drawn heights are unchanged.
pub const FLOOR_DB: f32 = -110.0;
pub const CEIL_DB: f32 = -5.0;

/// Inter-frame averaging, applied to magnitudes exactly as Web Audio's
/// `smoothingTimeConstant` does.
const SMOOTHING: f32 = 0.7;

/// A single-producer ring the audio thread can write to without blocking.
///
/// Samples are stored as `AtomicU32` bit patterns rather than a shared `Vec<f32>`
/// so a torn read between threads is merely stale data, not undefined behaviour.
/// Every access is `Relaxed`: the reader only needs *a* recent window, never a
/// precise one, and ordering guarantees would cost the audio thread something.
pub struct SampleRing {
    buf: Box<[AtomicU32]>,
    write: AtomicUsize,
}

impl Default for SampleRing {
    fn default() -> Self {
        Self::new()
    }
}

impl SampleRing {
    pub fn new() -> Self {
        Self {
            buf: (0..RING_SIZE).map(|_| AtomicU32::new(0)).collect(),
            write: AtomicUsize::new(0),
        }
    }

    /// Audio thread: append one sample. Wait-free.
    #[inline]
    pub fn push(&self, x: f32) {
        let w = self.write.load(Ordering::Relaxed);
        self.buf[w & (RING_SIZE - 1)].store(x.to_bits(), Ordering::Relaxed);
        self.write.store(w.wrapping_add(1), Ordering::Relaxed);
    }

    /// Editor thread: copy the most recent `out.len()` samples, oldest first.
    pub fn read_latest(&self, out: &mut [f32]) {
        let n = out.len().min(RING_SIZE);
        let w = self.write.load(Ordering::Relaxed);
        let start = w.wrapping_sub(n);
        for (i, slot) in out[..n].iter_mut().enumerate() {
            *slot = f32::from_bits(
                self.buf[start.wrapping_add(i) & (RING_SIZE - 1)].load(Ordering::Relaxed),
            );
        }
    }

    pub fn clear(&self) {
        for slot in self.buf.iter() {
            slot.store(0, Ordering::Relaxed);
        }
    }
}

/// The pair of taps the plugin exposes: signal in, signal out.
#[derive(Default)]
pub struct Taps {
    pub pre: SampleRing,
    pub post: SampleRing,
}

impl Taps {
    pub fn clear(&self) {
        self.pre.clear();
        self.post.clear();
    }
}

/// One analysed curve: window, FFT, smoothing and log reduction for a single tap.
struct Channel {
    windowed: Vec<f32>,
    spectrum: Vec<Complex32>,
    /// Smoothed magnitudes, carried between frames.
    mags: Vec<f32>,
    curve: Vec<f32>,
    bytes: Vec<u8>,
}

impl Channel {
    fn new(fft: &dyn RealToComplex<f32>) -> Self {
        Self {
            windowed: fft.make_input_vec(),
            spectrum: fft.make_output_vec(),
            mags: vec![0.0; FFT_SIZE / 2 + 1],
            curve: vec![FLOOR_DB; LOG_POINTS],
            bytes: vec![0; LOG_POINTS],
        }
    }
}

pub struct Analyzer {
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    /// Sum of the window, for the amplitude correction that keeps a full-scale
    /// sine reading 0 dBFS rather than whatever the window happens to cost.
    window_gain: f32,
    scratch: Vec<f32>,
    pre: Channel,
    post: Channel,
    sample_rate: f32,
    /// Log-spaced output frequencies, and the bin they map to. Rebuilt on a
    /// sample-rate change.
    bin_pos: Vec<f32>,
}

impl Analyzer {
    pub fn new(sample_rate: f32) -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);

        // Blackman, the same window Web Audio's AnalyserNode applies.
        let window: Vec<f32> = (0..FFT_SIZE)
            .map(|i| {
                let x = i as f32 / FFT_SIZE as f32;
                let two_pi_x = 2.0 * std::f32::consts::PI * x;
                0.42 - 0.5 * two_pi_x.cos() + 0.08 * (2.0 * two_pi_x).cos()
            })
            .collect();
        let window_gain = window.iter().sum::<f32>() / FFT_SIZE as f32;

        let pre = Channel::new(fft.as_ref());
        let post = Channel::new(fft.as_ref());

        let mut analyzer = Self {
            window,
            window_gain,
            scratch: vec![0.0; FFT_SIZE],
            pre,
            post,
            fft,
            sample_rate,
            bin_pos: Vec::new(),
        };
        analyzer.rebuild_mapping();
        analyzer
    }

    pub fn set_sample_rate(&mut self, sr: f32) {
        if (sr - self.sample_rate).abs() > f32::EPSILON {
            self.sample_rate = sr;
            self.rebuild_mapping();
        }
    }

    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    /// Upper display frequency, never past Nyquist.
    pub fn f_max(&self) -> f32 {
        F_MAX.min(self.sample_rate / 2.0 - 1.0)
    }

    /// Fractional bin index for each output point.
    fn rebuild_mapping(&mut self) {
        let bin_hz = self.sample_rate / FFT_SIZE as f32;
        let f_max = self.f_max();
        let ratio = f_max / F_MIN;
        self.bin_pos = (0..LOG_POINTS)
            .map(|j| {
                let f = F_MIN * ratio.powf(j as f32 / (LOG_POINTS - 1) as f32);
                f / bin_hz
            })
            .collect();
    }

    /// Analyse both taps and return the two base64 curves, pre first.
    pub fn analyze(&mut self, taps: &Taps) -> (String, String) {
        let pre = self.analyze_one(&taps.pre, true);
        let post = self.analyze_one(&taps.post, false);
        (pre, post)
    }

    fn analyze_one(&mut self, ring: &SampleRing, is_pre: bool) -> String {
        ring.read_latest(&mut self.scratch);

        let channel = if is_pre {
            &mut self.pre
        } else {
            &mut self.post
        };
        for (dst, (src, w)) in channel
            .windowed
            .iter_mut()
            .zip(self.scratch.iter().zip(self.window.iter()))
        {
            *dst = src * w;
        }

        // The planner's scratch requirement is satisfied internally by realfft
        // when passing an empty scratch slice is not an option, so use the
        // process call that allocates nothing beyond the vectors we already own.
        if self
            .fft
            .process(&mut channel.windowed, &mut channel.spectrum)
            .is_err()
        {
            return String::new();
        }

        // Referenced so that a full-scale sine reads 0 dBFS: a tone of amplitude A
        // puts A/2 into its bin, and the window costs another factor of its own
        // mean — both are divided back out here. Magnitudes are then smoothed
        // between frames the way Web Audio's `smoothingTimeConstant` does.
        let norm = 2.0 / (FFT_SIZE as f32 * self.window_gain);
        for (mag, bin) in channel.mags.iter_mut().zip(channel.spectrum.iter()) {
            let current = bin.norm() * norm;
            *mag = SMOOTHING * *mag + (1.0 - SMOOTHING) * current;
        }

        log_reduce(&channel.mags, &self.bin_pos, &mut channel.curve);

        for (byte, db) in channel.bytes.iter_mut().zip(channel.curve.iter()) {
            let t = ((db - FLOOR_DB) / (CEIL_DB - FLOOR_DB)).clamp(0.0, 1.0);
            *byte = (t * 255.0 + 0.5) as u8;
        }
        STANDARD_NO_PAD.encode(&channel.bytes)
    }
}

/// Reduce linear-spaced magnitudes onto log-spaced points, in dB.
///
/// The two ends of the spectrum need opposite treatment. Down at 20 Hz several
/// output points fall inside one bin, so the value is interpolated — reading the
/// same bin repeatedly would draw a staircase. Up at 20 kHz dozens of bins fall
/// inside one point, so the loudest is kept, which is what makes a narrow peak
/// survive to the display instead of being averaged away.
fn log_reduce(mags: &[f32], bin_pos: &[f32], out: &mut [f32]) {
    let last_bin = mags.len() - 1;
    let db = |m: f32| 20.0 * m.max(1e-12).log10();

    for j in 0..out.len() {
        let pos = bin_pos[j];
        // Half-way to each neighbour marks this point's share of the spectrum.
        let lo = if j == 0 {
            pos
        } else {
            (pos + bin_pos[j - 1]) * 0.5
        };
        let hi = if j + 1 == out.len() {
            pos
        } else {
            (pos + bin_pos[j + 1]) * 0.5
        };

        out[j] = if hi - lo < 1.0 {
            let i = pos.floor().clamp(0.0, last_bin as f32) as usize;
            let next = (i + 1).min(last_bin);
            let t = (pos - i as f32).clamp(0.0, 1.0);
            db(mags[i] * (1.0 - t) + mags[next] * t)
        } else {
            let a = (lo.floor().max(0.0) as usize).min(last_bin);
            let b = ((hi.ceil() as usize).max(a + 1)).min(last_bin + 1);
            let mut peak = 0.0f32;
            for &m in &mags[a..b] {
                if m > peak {
                    peak = m;
                }
            }
            db(peak)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    fn fill(ring: &SampleRing, freq: f32, sr: f32, amp: f32) {
        for i in 0..FFT_SIZE * 2 {
            ring.push((2.0 * PI * freq * i as f32 / sr).sin() * amp);
        }
    }

    fn decode(b64: &str) -> Vec<f32> {
        STANDARD_NO_PAD
            .decode(b64)
            .unwrap()
            .into_iter()
            .map(|b| FLOOR_DB + (b as f32 / 255.0) * (CEIL_DB - FLOOR_DB))
            .collect()
    }

    /// Index of the output point nearest a frequency.
    fn point_for(freq: f32, sr: f32) -> usize {
        let f_max = F_MAX.min(sr / 2.0 - 1.0);
        let t = (freq / F_MIN).ln() / (f_max / F_MIN).ln();
        (t * (LOG_POINTS - 1) as f32).round() as usize
    }

    #[test]
    fn the_ring_returns_the_most_recent_window() {
        let ring = SampleRing::new();
        for i in 0..RING_SIZE + 500 {
            ring.push(i as f32);
        }
        let mut out = [0.0f32; 8];
        ring.read_latest(&mut out);
        let base = (RING_SIZE + 500 - 8) as f32;
        for (i, v) in out.iter().enumerate() {
            assert_eq!(*v, base + i as f32);
        }
    }

    #[test]
    fn a_tone_shows_up_at_its_own_frequency() {
        let sr = 48_000.0;
        let taps = Taps::default();
        fill(&taps.pre, 1000.0, sr, 1.0);
        let mut analyzer = Analyzer::new(sr);

        // Smoothing means the first frames only get part of the way there.
        let mut pre = String::new();
        for _ in 0..40 {
            pre = analyzer.analyze(&taps).0;
        }
        let curve = decode(&pre);

        let peak_idx = curve
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap()
            .0;
        let expected = point_for(1000.0, sr);
        assert!(
            (peak_idx as i32 - expected as i32).abs() <= 3,
            "peak landed at point {peak_idx}, expected near {expected}"
        );
        // A full-scale sine should read close to 0 dBFS, i.e. clamp at the ceiling.
        assert!(
            curve[peak_idx] > CEIL_DB - 1.0,
            "peak was {} dB",
            curve[peak_idx]
        );
        // Two octaves below, there is nothing.
        assert!(curve[point_for(250.0, sr)] < -60.0);
    }

    #[test]
    fn a_quieter_tone_reads_lower() {
        let sr = 48_000.0;
        let loud = Taps::default();
        let quiet = Taps::default();
        fill(&loud.pre, 1000.0, sr, 1.0);
        fill(&quiet.pre, 1000.0, sr, 0.01); // -40 dB

        let mut a = Analyzer::new(sr);
        let mut b = Analyzer::new(sr);
        let (mut loud_curve, mut quiet_curve) = (String::new(), String::new());
        for _ in 0..40 {
            loud_curve = a.analyze(&loud).0;
            quiet_curve = b.analyze(&quiet).0;
        }

        let idx = point_for(1000.0, sr);
        let l = decode(&loud_curve)[idx];
        let q = decode(&quiet_curve)[idx];
        assert!(q < l - 30.0, "loud {l} dB vs quiet {q} dB");
    }

    #[test]
    fn silence_sits_on_the_floor() {
        let taps = Taps::default();
        let mut analyzer = Analyzer::new(48_000.0);
        let curve = decode(&analyzer.analyze(&taps).0);
        assert!(curve.iter().all(|&db| db <= FLOOR_DB + 0.5));
    }

    #[test]
    fn the_curve_is_the_advertised_size() {
        let taps = Taps::default();
        let mut analyzer = Analyzer::new(44_100.0);
        let (pre, post) = analyzer.analyze(&taps);
        assert_eq!(decode(&pre).len(), LOG_POINTS);
        assert_eq!(decode(&post).len(), LOG_POINTS);
    }

    #[test]
    fn the_top_of_the_curve_never_exceeds_nyquist() {
        let mut analyzer = Analyzer::new(44_100.0);
        assert!(analyzer.f_max() < 22_050.0);
        analyzer.set_sample_rate(96_000.0);
        assert_eq!(analyzer.f_max(), F_MAX);
    }
}
