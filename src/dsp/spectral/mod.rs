//! Spectral adaptive resonance — FFT detection, time-domain suppression.
//!
//! # The shape of the thing
//!
//! The adaptive bank in [`crate::dsp::resonance`] finds resonances with a
//! filter bank, which is cheap and latency-free but only ever as sharp as its
//! sixth-octave spacing. This module finds them with an FFT instead — and keeps
//! the audio path exactly as latency-free, because the FFT never touches the
//! audio. The signal is *copied* into a lock-free ring on its way past; a
//! background thread windows it, transforms it, scores and clusters its peaks,
//! and publishes a handful of resonance targets; and the audio thread reads the
//! latest snapshot of those targets and runs them as ordinary minimum-phase
//! peaking biquads. Audio never waits for analysis:
//!
//! ```text
//! audio ────────────────────────────► EQ ► adaptive filter pool ► out
//!   │                                            ▲
//!   └► analysis ring ► FFT worker ► targets ─────┘  (lock-free both ways)
//! ```
//!
//! # Latency, and what is not latency
//!
//! The pool adds **zero samples of processing latency**: every filter is a
//! causal biquad, nothing looks ahead, and nothing is buffered or delayed on
//! the audio path. What the FFT costs is *reaction time* — the detector needs
//! its window's worth of signal (about 43 ms at every supported rate) plus a
//! few hops of confidence before a target is trusted, so a resonance that
//! appears out of nowhere is heard unsuppressed for a few tens of
//! milliseconds while the audio continues undelayed. Those are different
//! quantities and this module keeps them apart on purpose.
//!
//! # False positives
//!
//! A loud bin is not a resonance. A target has to stand proud of a local
//! spectral baseline (prominence), be narrow enough to be a peak rather than a
//! formant or a tonal region (bandwidth), and keep doing it across analysis
//! frames (persistence) before its confidence rises enough to open the filter.
//! Bass fundamentals get an extra threshold below ~120 Hz unless a band's own
//! search region is aimed there, on the grounds that pointing a band at the
//! low end *is* configuration.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use realfft::{num_complex::Complex32, RealFftPlanner, RealToComplex};

use crate::dsp::biquad::{Biquad, Coeffs};
use crate::dsp::dynamics::step_toward;
use crate::params::{ResQuality, MAX_BANDS};

pub mod arm_neon;
pub mod dispatch;
pub mod scalar;
#[cfg(target_arch = "x86_64")]
pub mod x86_avx2;

pub use dispatch::{Kernels, PowerMode};

/// Capacity of the adaptive filter pool. The quality tiers cap how much of it
/// is used at once; the memory for all of it exists up front either way.
pub const MAX_TARGETS: usize = 24;

/// Worker-side tracks per channel — headroom over the pool so a new resonance
/// can build confidence while the pool is full of established ones.
const TRACKS: usize = 48;

/// Interleaved stereo ring: 16384 frames, which holds the largest analysis
/// window (8192 at 192 kHz) with the same again in slack.
const RING_LEN: usize = 1 << 15;

/// Analysis levels below this are silence, whatever they measure relative to
/// their neighbourhood.
const FLOOR_DB: f32 = -85.0;

/// Confidence below which a track is not even published as a candidate.
/// High enough that noise blips being intermittently re-matched — which the
/// slower, hold-friendly decay now keeps alive longer — never surface.
const PUBLISH_CONF: f32 = 0.2;

/// Blocks a freed slot keeps filtering near-identity so its delay line drains.
const IDLE_BLOCKS: u8 = 2;

/// FFT size for a sample rate: the power of two nearest ~23 Hz per bin.
///
/// That keeps the analysis window at ~43 ms across every supported rate —
/// resolution enough to separate a resonance from its neighbours down to the
/// low mids, reaction fast enough that the detector is not the slow part of
/// the attack. Deliberately *not* maximised: doubling the FFT would halve the
/// already-marginal usefulness below 100 Hz and double the reaction time
/// everywhere else.
pub fn fft_size_for(sr: f32) -> usize {
    let ideal = (sr / 23.4).max(256.0);
    let exp = ideal.log2().round() as u32;
    (1usize << exp).clamp(1024, 8192)
}

#[inline]
fn store_f32(a: &AtomicU32, v: f32) {
    a.store(v.to_bits(), Ordering::Relaxed);
}

#[inline]
fn load_f32(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(Ordering::Relaxed))
}

fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

// --- the analysis ring -------------------------------------------------------

/// Interleaved stereo ring the audio thread writes without blocking.
///
/// Interleaved rather than two rings so the worker's window is always cut at a
/// frame boundary — two independent write heads could hand it a left channel a
/// few samples ahead of its right, and the mid/side spectra are derived from
/// the pair, where that skew would read as phantom side energy.
///
/// Same storage discipline as [`crate::analyzer::SampleRing`]: atomic bit
/// patterns, everything relaxed, torn reads are stale data and nothing worse.
pub struct AnalysisRing {
    buf: Box<[AtomicU32]>,
    write: AtomicUsize,
}

impl Default for AnalysisRing {
    fn default() -> Self {
        Self {
            buf: (0..RING_LEN).map(|_| AtomicU32::new(0)).collect(),
            write: AtomicUsize::new(0),
        }
    }
}

impl AnalysisRing {
    /// Audio thread: append one stereo frame. Wait-free.
    #[inline]
    pub fn push(&self, l: f32, r: f32) {
        let w = self.write.load(Ordering::Relaxed);
        self.buf[w & (RING_LEN - 1)].store(l.to_bits(), Ordering::Relaxed);
        self.buf[(w + 1) & (RING_LEN - 1)].store(r.to_bits(), Ordering::Relaxed);
        self.write.store(w.wrapping_add(2), Ordering::Relaxed);
    }

    /// Total values written so far — the worker's cadence clock.
    pub fn position(&self) -> usize {
        self.write.load(Ordering::Relaxed)
    }

    /// Worker: copy the most recent `left.len()` frames, oldest first.
    pub fn read_latest(&self, left: &mut [f32], right: &mut [f32]) {
        let n = left.len().min(right.len()).min(RING_LEN / 2);
        // Aligned down to a frame boundary so `start` always lands on a left.
        let w = self.write.load(Ordering::Relaxed) & !1usize;
        let start = w.wrapping_sub(n * 2);
        for i in 0..n {
            let at = start.wrapping_add(i * 2);
            left[i] = f32::from_bits(self.buf[at & (RING_LEN - 1)].load(Ordering::Relaxed));
            right[i] =
                f32::from_bits(self.buf[(at + 1) & (RING_LEN - 1)].load(Ordering::Relaxed));
        }
    }

    pub fn clear(&self) {
        for slot in self.buf.iter() {
            slot.store(0, Ordering::Relaxed);
        }
    }
}

// --- the target handoff ------------------------------------------------------

/// One resonance the detector wants suppressed, as published to the audio
/// thread. `track` is a persistent id: the same physical resonance keeps the
/// same id across frames, which is what lets the pool keep one filter gliding
/// after it instead of retiring and re-opening filters on every FFT hop.
#[derive(Clone, Copy, Debug)]
pub struct WireTarget {
    /// Nonzero while the entry is real; 0 marks an empty slot.
    pub track: u32,
    pub freq: f32,
    pub q: f32,
    /// dB the peak stands above its effective threshold — the raw material the
    /// pool turns into a cut. Attenuation itself is decided on the audio
    /// thread, where Amount and Range are smoothed parameters.
    pub excess_db: f32,
    /// 0..1, built from narrowness, prominence and persistence.
    pub confidence: f32,
    /// Band slot that owns the target, or -1 for the global scan.
    pub owner: i16,
    /// [`crate::params::BandChannel`] as its variant index.
    pub channel: u8,
}

impl Default for WireTarget {
    fn default() -> Self {
        Self {
            track: 0,
            freq: 0.0,
            q: 8.0,
            excess_db: 0.0,
            confidence: 0.0,
            owner: -1,
            channel: 0,
        }
    }
}

/// A full set of targets, published atomically as one snapshot.
#[derive(Clone, Copy)]
pub struct TargetFrame {
    pub serial: u32,
    pub count: u32,
    pub targets: [WireTarget; MAX_TARGETS],
}

impl Default for TargetFrame {
    fn default() -> Self {
        Self {
            serial: 0,
            count: 0,
            targets: [WireTarget::default(); MAX_TARGETS],
        }
    }
}

/// Wait-free single-producer single-consumer triple buffer.
///
/// The three slots are always partitioned between the writer's back buffer,
/// the reader's front buffer, and the middle the two swap through, so neither
/// side ever touches a slot the other holds. Publishing and consuming are one
/// atomic swap each; nobody waits, nobody allocates, and the reader always
/// gets the *newest* published value rather than a queue of stale ones —
/// which is exactly right for "the current resonance targets".
pub struct TripleBuffer<T> {
    slots: [UnsafeCell<T>; 3],
    /// Index of the middle slot, with [`Self::FRESH`] set when it holds a
    /// publication the reader has not consumed.
    mid: AtomicU8,
}

// Safety: the writer only dereferences its back slot and the reader its front
// slot, and the swap protocol keeps those disjoint. `T: Copy` keeps reads
// free of drop hazards.
unsafe impl<T: Copy + Send> Sync for TripleBuffer<T> {}
unsafe impl<T: Copy + Send> Send for TripleBuffer<T> {}

impl<T: Copy + Default> Default for TripleBuffer<T> {
    fn default() -> Self {
        Self {
            slots: [
                UnsafeCell::new(T::default()),
                UnsafeCell::new(T::default()),
                UnsafeCell::new(T::default()),
            ],
            // Slot 1 starts as the middle; writers start on 0, readers on 2.
            mid: AtomicU8::new(1),
        }
    }
}

impl<T: Copy> TripleBuffer<T> {
    const FRESH: u8 = 0b100;

    /// Writer side. `back` is the writer's slot index, initially 0.
    pub fn publish(&self, back: &mut u8, value: T) {
        unsafe { *self.slots[*back as usize].get() = value };
        let prev = self.mid.swap(*back | Self::FRESH, Ordering::AcqRel);
        *back = prev & 0b11;
    }

    /// Reader side. `front` is the reader's slot index, initially 2. `None`
    /// when nothing new has been published since the last read.
    pub fn read(&self, front: &mut u8) -> Option<T> {
        if self.mid.load(Ordering::Relaxed) & Self::FRESH == 0 {
            return None;
        }
        let prev = self.mid.swap(*front, Ordering::AcqRel);
        *front = prev & 0b11;
        Some(unsafe { *self.slots[*front as usize].get() })
    }
}

// --- configuration the audio thread publishes to the worker ------------------

/// Plain snapshot of everything the detector needs to know.
#[derive(Clone, Copy)]
pub struct ConfigView {
    pub sample_rate: f32,
    /// The global Spectral mode is armed.
    pub global_on: bool,
    /// 0 ultra, 1 balanced, 2 high — see [`crate::params::ResQuality`].
    pub quality: u32,
    /// dB of prominence a peak needs before it counts.
    pub threshold_db: f32,
    /// 0..1 — narrows the baseline and weights narrowness into confidence.
    pub selectivity: f32,
    pub low_hz: f32,
    pub high_hz: f32,
    pub bands: [BandRegionView; MAX_BANDS],
}

impl Default for ConfigView {
    fn default() -> Self {
        Self {
            sample_rate: 48_000.0,
            global_on: false,
            quality: 0,
            threshold_db: 6.0,
            selectivity: 0.5,
            low_hz: 20.0,
            high_hz: 20_000.0,
            bands: [BandRegionView::default(); MAX_BANDS],
        }
    }
}

/// One band's spectral search region.
#[derive(Clone, Copy, Default)]
pub struct BandRegionView {
    pub active: bool,
    /// [`crate::params::BandChannel`] as its variant index.
    pub channel: u8,
    /// Centre of the search region — the band's own frequency.
    pub freq: f32,
    /// Octaves either side of it the detector may roam.
    pub width_oct: f32,
    /// dB taken off the threshold inside the region.
    pub sens_db: f32,
}

/// The same, as atomics the audio thread stores into without locking. Torn
/// reads across fields hand the worker a mix of two adjacent control blocks,
/// which is indistinguishable from having sampled a moment earlier or later.
pub struct SharedConfig {
    sample_rate: AtomicU32,
    flags: AtomicU32,
    threshold_db: AtomicU32,
    selectivity: AtomicU32,
    low_hz: AtomicU32,
    high_hz: AtomicU32,
    bands: [BandCfg; MAX_BANDS],
}

struct BandCfg {
    /// bit 0 active, bits 1..=3 channel.
    flags: AtomicU32,
    freq: AtomicU32,
    width_oct: AtomicU32,
    sens_db: AtomicU32,
}

impl Default for SharedConfig {
    fn default() -> Self {
        let d = ConfigView::default();
        let cfg = Self {
            sample_rate: AtomicU32::new(0),
            flags: AtomicU32::new(0),
            threshold_db: AtomicU32::new(0),
            selectivity: AtomicU32::new(0),
            low_hz: AtomicU32::new(0),
            high_hz: AtomicU32::new(0),
            bands: std::array::from_fn(|_| BandCfg {
                flags: AtomicU32::new(0),
                freq: AtomicU32::new(0),
                width_oct: AtomicU32::new(0),
                sens_db: AtomicU32::new(0),
            }),
        };
        cfg.publish(&d);
        cfg
    }
}

impl SharedConfig {
    /// Audio thread: store the block's view. Plain relaxed stores.
    pub fn publish(&self, v: &ConfigView) {
        store_f32(&self.sample_rate, v.sample_rate);
        self.flags.store(
            (v.global_on as u32) | (v.quality.min(2) << 1),
            Ordering::Relaxed,
        );
        store_f32(&self.threshold_db, v.threshold_db);
        store_f32(&self.selectivity, v.selectivity);
        store_f32(&self.low_hz, v.low_hz);
        store_f32(&self.high_hz, v.high_hz);
        for (slot, band) in self.bands.iter().zip(v.bands.iter()) {
            slot.flags.store(
                (band.active as u32) | ((band.channel as u32 & 0b111) << 1),
                Ordering::Relaxed,
            );
            store_f32(&slot.freq, band.freq);
            store_f32(&slot.width_oct, band.width_oct);
            store_f32(&slot.sens_db, band.sens_db);
        }
    }

    /// Worker: read the current view.
    pub fn snapshot(&self) -> ConfigView {
        let flags = self.flags.load(Ordering::Relaxed);
        ConfigView {
            sample_rate: load_f32(&self.sample_rate),
            global_on: flags & 1 != 0,
            quality: (flags >> 1) & 0b11,
            threshold_db: load_f32(&self.threshold_db),
            selectivity: load_f32(&self.selectivity),
            low_hz: load_f32(&self.low_hz),
            high_hz: load_f32(&self.high_hz),
            bands: std::array::from_fn(|i| {
                let b = &self.bands[i];
                let flags = b.flags.load(Ordering::Relaxed);
                BandRegionView {
                    active: flags & 1 != 0,
                    channel: ((flags >> 1) & 0b111) as u8,
                    freq: load_f32(&b.freq),
                    width_oct: load_f32(&b.width_oct),
                    sens_db: load_f32(&b.sens_db),
                }
            }),
        }
    }
}

/// Everything the audio thread and the worker share. One per plugin instance.
#[derive(Default)]
pub struct SharedSpectral {
    pub ring: AnalysisRing,
    pub cfg: SharedConfig,
    pub frames: TripleBuffer<TargetFrame>,
    pub shutdown: AtomicBool,
}

// --- the detector (worker thread — may allocate, must never block audio) -----

/// Channel codes, matching [`crate::params::BandChannel`]'s variant order.
const CH_STEREO: usize = 0;
const CHANNELS: usize = 5;

/// One persistent resonance identity on the worker side.
///
/// Tracks live longer than any single FFT frame: a peak that goes missing
/// for a frame or three keeps its id, its smoothed frequency and its excess,
/// and only fades out through confidence decay — which is what lets the
/// audio-side pool hold one filter on one resonance instead of retriggering.
#[derive(Clone, Copy)]
struct Track {
    id: u32,
    log2f: f32,
    q: f32,
    excess: f32,
    conf: f32,
    /// Seconds this track has been continuously alive — earns the small
    /// persistence bonus that keeps arbitration from churning.
    age_s: f32,
    /// Seconds of consecutive matches — zeroed by a single miss. Noise blips
    /// re-lighting a stale track never build one, which is what keeps them
    /// off the wire.
    streak_s: f32,
    /// Seconds since a cluster last matched it.
    missing_s: f32,
    claimed: bool,
    /// Latest word on whether this peak sits in a harmonic comb.
    harmonic: bool,
}

impl Track {
    const DEAD: Self = Self {
        id: 0,
        log2f: 0.0,
        q: 8.0,
        excess: 0.0,
        conf: 0.0,
        age_s: 0.0,
        streak_s: 0.0,
        missing_s: 0.0,
        claimed: false,
        harmonic: false,
    };

    /// Arbitration score: how sure times how bad, with a small bonus for
    /// having been around — an established track must be beaten by a
    /// meaningfully better candidate, not merely tied.
    fn score(&self) -> f32 {
        self.conf * self.excess * (1.0 + 0.25 * (self.age_s / 0.5).min(1.0))
    }

    /// Worth telling the audio thread about? Established tracks stay on the
    /// wire through a brief dropout (their confidence is still high); a young
    /// candidate has to show up in consecutive frames first, so noise blips
    /// that merely graze an old track never publish.
    fn publishable(&self) -> bool {
        self.conf >= 0.5 || (self.conf >= PUBLISH_CONF && self.streak_s >= 0.032)
    }
}

/// Per-channel analysis state — smoothed spectrum and the tracks living on it.
struct ChannelState {
    smoothed_db: Vec<f32>,
    seeded: bool,
    tracks: [Track; TRACKS],
}

impl ChannelState {
    fn new(bins: usize) -> Self {
        Self {
            smoothed_db: vec![FLOOR_DB; bins],
            seeded: false,
            tracks: [Track::DEAD; TRACKS],
        }
    }
}

/// A raw spectral peak, before clustering.
#[derive(Clone, Copy)]
struct Peak {
    log2f: f32,
    excess: f32,
    lo_l2: f32,
    hi_l2: f32,
}

/// A cluster of peaks judged to be one physical resonance.
#[derive(Clone, Copy)]
struct Cluster {
    sum_w: f32,
    sum_wl2: f32,
    excess: f32,
    lo_l2: f32,
    hi_l2: f32,
    /// Member of a harmonic comb — a note's partial, not a resonance.
    harmonic: bool,
}

impl Cluster {
    fn log2f(&self) -> f32 {
        self.sum_wl2 / self.sum_w.max(1e-9)
    }

    /// Q from the cluster's width in octaves — `1 / (2^(w/2) − 2^(−w/2))`.
    fn q(&self) -> f32 {
        let w = (self.hi_l2 - self.lo_l2).max(0.02);
        let denom = 2f32.powf(w / 2.0) - 2f32.powf(-w / 2.0);
        (1.0 / denom).clamp(2.0, 36.0)
    }
}

/// Smoothing coefficients derived from the analysis-frame period.
///
/// Every rate in the detector is a *time* constant, converted to a per-frame
/// coefficient here — so changing the hop, or the sample rate changing the
/// hop's duration, changes nothing about how the tracker behaves in seconds.
#[derive(Clone, Copy)]
struct FrameRates {
    /// Seconds per analysis frame.
    dt: f32,
    /// Spectrum smoothing: fast up so a resonance registers on the frame it
    /// appears, slower down so it is not forgotten between bins' flickers.
    spec_up: f32,
    spec_dn: f32,
    /// Detection-confidence smoothing — the *identity* envelope, distinct on
    /// purpose from the audio-side gain ballistics (spec §7).
    conf_up: f32,
    conf_dn: f32,
    /// Multiplier per frame while a track goes unmatched (τ = 80 ms).
    conf_decay: f32,
    /// Worker-side target smoothing, before the pool smooths again.
    freq: f32,
    q: f32,
    excess_up: f32,
    excess_dn: f32,
}

impl FrameRates {
    fn for_hop(hop: usize, sr: f32) -> Self {
        let dt = hop as f32 / sr;
        let a = |tau_s: f32| 1.0 - (-dt / tau_s).exp();
        Self {
            dt,
            spec_up: a(0.010),
            spec_dn: a(0.030),
            conf_up: a(0.025),
            conf_dn: a(0.080),
            conf_decay: (-dt / 0.080).exp(),
            freq: a(0.015),
            q: a(0.025),
            excess_up: a(0.015),
            excess_dn: a(0.060),
        }
    }
}

pub struct Detector {
    sr: f32,
    size: usize,
    /// Samples between analysis passes — an eighth of the window, ~5.3 ms at
    /// every supported rate, so the tracker updates often enough for the
    /// audio-side interpolation to glide between small steps.
    pub hop: usize,
    fft: Arc<dyn RealToComplex<f32>>,
    kernels: Kernels,
    rates: FrameRates,
    window: Vec<f32>,
    /// dB to add to `10·log10(power)` so a full-scale sine reads 0 dBFS.
    norm_db: f32,

    in_l: Vec<f32>,
    in_r: Vec<f32>,
    win_scratch: Vec<f32>,
    spec_l: Vec<Complex32>,
    spec_r: Vec<Complex32>,

    /// Shared per-pass scratch, reused across channels.
    power: Vec<f32>,
    db: Vec<f32>,
    prefix: Vec<f32>,
    base: Vec<f32>,
    prom: Vec<f32>,
    thr: Vec<f32>,

    channels: [Option<Box<ChannelState>>; CHANNELS],
    peaks: Vec<Peak>,
    clusters: Vec<Cluster>,
    /// Selection scratch: arbitration score alongside the wire target, so
    /// the final cap can respect the persistence bonus.
    selected: Vec<(f32, WireTarget)>,

    next_id: u32,
    serial: u32,
    back: u8,
    /// Whether the last published frame carried any targets, so an idle
    /// detector publishes one empty frame rather than none or many.
    was_publishing: bool,
}

impl Detector {
    /// A detector on the best kernel backend this machine has.
    pub fn new(sr: f32) -> Self {
        Self::with_kernels(sr, dispatch::select())
    }

    /// A detector pinned to one backend — how the parity tests and the
    /// benchmark compare them.
    pub fn with_kernels(sr: f32, kernels: Kernels) -> Self {
        let mut d = Self {
            sr: 0.0,
            size: 0,
            hop: 0,
            fft: RealFftPlanner::new().plan_fft_forward(1024),
            kernels,
            rates: FrameRates::for_hop(256, 48_000.0),
            window: Vec::new(),
            norm_db: 0.0,
            in_l: Vec::new(),
            in_r: Vec::new(),
            win_scratch: Vec::new(),
            spec_l: Vec::new(),
            spec_r: Vec::new(),
            power: Vec::new(),
            db: Vec::new(),
            prefix: Vec::new(),
            base: Vec::new(),
            prom: Vec::new(),
            thr: Vec::new(),
            channels: [None, None, None, None, None],
            peaks: Vec::with_capacity(64),
            clusters: Vec::with_capacity(64),
            selected: Vec::with_capacity(MAX_TARGETS * 4),
            next_id: 1,
            serial: 0,
            back: 0,
            was_publishing: false,
        };
        d.rebuild(sr);
        d
    }

    /// Name of the kernel backend in use, for diagnostics.
    pub fn backend(&self) -> &'static str {
        self.kernels.name
    }

    fn rebuild(&mut self, sr: f32) {
        let sr = if sr.is_finite() && sr > 8000.0 {
            sr
        } else {
            48_000.0
        };
        self.sr = sr;
        self.size = fft_size_for(sr);
        self.hop = self.size / 8;
        self.rates = FrameRates::for_hop(self.hop, sr);
        self.fft = RealFftPlanner::new().plan_fft_forward(self.size);

        // Hann. Compared with Blackman it trades a little side-lobe rejection
        // for a narrower main lobe, which is the right trade for a detector
        // whose whole job is telling adjacent peaks apart.
        self.window = (0..self.size)
            .map(|i| {
                let x = i as f32 / self.size as f32;
                0.5 - 0.5 * (2.0 * std::f32::consts::PI * x).cos()
            })
            .collect();
        let mean = self.window.iter().sum::<f32>() / self.size as f32;
        self.norm_db = 20.0 * (2.0 / (self.size as f32 * mean)).log10();

        let bins = self.size / 2 + 1;
        self.in_l = vec![0.0; self.size];
        self.in_r = vec![0.0; self.size];
        self.win_scratch = self.fft.make_input_vec();
        self.spec_l = self.fft.make_output_vec();
        self.spec_r = self.fft.make_output_vec();
        self.power = vec![0.0; bins];
        self.db = vec![0.0; bins];
        self.prefix = vec![0.0; bins + 1];
        self.base = vec![0.0; bins];
        self.prom = vec![0.0; bins];
        self.thr = vec![f32::INFINITY; bins];
        self.channels = [None, None, None, None, None];
    }

    /// One analysis pass: read the ring, transform, track, publish.
    pub fn analyze(&mut self, shared: &SharedSpectral) {
        let cfg = shared.cfg.snapshot();
        if (cfg.sample_rate - self.sr).abs() > 1.0 || fft_size_for(cfg.sample_rate) != self.size {
            self.rebuild(cfg.sample_rate);
        }

        let mut demand = [false; CHANNELS];
        if cfg.global_on {
            demand[CH_STEREO] = true;
        }
        for band in cfg.bands.iter().filter(|b| b.active) {
            demand[(band.channel as usize).min(CHANNELS - 1)] = true;
        }
        if !demand.iter().any(|d| *d) {
            self.go_idle(shared);
            return;
        }

        shared.ring.read_latest(&mut self.in_l, &mut self.in_r);
        self.transform();

        for (ch, wanted) in demand.iter().enumerate() {
            if *wanted {
                self.analyze_channel(ch, &cfg);
            } else if let Some(state) = self.channels[ch].as_mut() {
                // A channel nobody is watching decays rather than freezing, so
                // stale tracks cannot come back armed when it is re-demanded.
                let rates = self.rates;
                for t in state.tracks.iter_mut().filter(|t| t.id != 0) {
                    t.conf *= rates.conf_decay;
                    t.streak_s = 0.0;
                    t.missing_s += rates.dt;
                    if t.conf < 0.05 && t.missing_s > 0.15 {
                        t.id = 0;
                    }
                }
            }
        }

        self.select_and_publish(shared, &cfg);
    }

    /// Publish one empty frame on the transition into idleness, and decay the
    /// tracks so nothing is remembered from before.
    fn go_idle(&mut self, shared: &SharedSpectral) {
        for state in self.channels.iter_mut().flatten() {
            for t in state.tracks.iter_mut() {
                *t = Track::DEAD;
            }
            state.seeded = false;
        }
        if self.was_publishing {
            self.was_publishing = false;
            self.serial = self.serial.wrapping_add(1);
            shared.frames.publish(
                &mut self.back,
                TargetFrame {
                    serial: self.serial,
                    ..TargetFrame::default()
                },
            );
        }
    }

    /// Window and FFT both input channels.
    fn transform(&mut self) {
        (self.kernels.window)(&mut self.win_scratch, &self.in_l, &self.window);
        let _ = self.fft.process(&mut self.win_scratch, &mut self.spec_l);
        (self.kernels.window)(&mut self.win_scratch, &self.in_r, &self.window);
        let _ = self.fft.process(&mut self.win_scratch, &mut self.spec_r);
    }

    /// Detect and track on one channel: smooth, baseline, prominence, peaks,
    /// clusters, tracks.
    fn analyze_channel(&mut self, ch: usize, cfg: &ConfigView) {
        (self.kernels.power)(
            &self.spec_l,
            &self.spec_r,
            PowerMode::from_channel(ch),
            &mut self.power,
        );
        let bins = self.power.len();
        let bin_hz = self.sr / self.size as f32;

        if self.channels[ch].is_none() {
            self.channels[ch] = Some(Box::new(ChannelState::new(bins)));
        }

        // -- dB, then smooth across frames --------------------------------
        (self.kernels.power_db)(&self.power, self.norm_db, &mut self.db);
        {
            let state = self.channels[ch].as_mut().unwrap();
            if state.smoothed_db.len() != bins {
                **state = ChannelState::new(bins);
            }
            if !state.seeded {
                state.smoothed_db.copy_from_slice(&self.db);
                state.seeded = true;
            } else {
                // Faster up than down: a resonance should register on the
                // frame it appears, and take a couple of frames to be
                // forgotten — bin-level flicker is handled by confidence,
                // not by making the spectrum itself sluggish.
                (self.kernels.smooth)(
                    &mut state.smoothed_db,
                    &self.db,
                    self.rates.spec_up,
                    self.rates.spec_dn,
                );
            }
        }

        // -- local baseline over a log-frequency window --------------------
        // Prefix sums make each bin's neighbourhood mean O(1). Selectivity
        // narrows the neighbourhood: a wide baseline lets broad humps read as
        // excess, a tight one hugs the spectrum until only spikes stand out.
        {
            let state = self.channels[ch].as_ref().unwrap();
            self.prefix[0] = 0.0;
            for (i, s) in state.smoothed_db.iter().enumerate() {
                self.prefix[i + 1] = self.prefix[i] + s;
            }
        }
        let span_oct = 0.66 - 0.40 * cfg.selectivity.clamp(0.0, 1.0);
        let ratio = 2f32.powf(span_oct);
        let lo_bin = ((cfg.low_hz / bin_hz).floor().max(1.0) as usize).min(bins - 2);
        let hi_bin = ((cfg.high_hz.min(self.sr * 0.45) / bin_hz).ceil() as usize)
            .clamp(lo_bin + 1, bins - 2);
        for i in 1..bins - 1 {
            let a = ((i as f32 / ratio) as usize).min(i.saturating_sub(4)).max(1);
            let b = ((i as f32 * ratio).ceil() as usize).max(i + 4).min(bins - 1);
            self.base[i] = (self.prefix[b + 1] - self.prefix[a]) / (b + 1 - a) as f32;
        }
        self.base[0] = self.base[1];
        self.base[bins - 1] = self.base[bins - 2];
        {
            let state = self.channels[ch].as_ref().unwrap();
            (self.kernels.subtract)(&state.smoothed_db, &self.base, &mut self.prom);
        }
        self.prom[0] = 0.0;
        self.prom[bins - 1] = 0.0;

        // -- per-bin effective threshold -----------------------------------
        // The global scan pays a penalty below ~120 Hz — fundamentals live
        // there and "resonance" is mostly the note itself. A band whose search
        // region covers those bins pays no penalty: aiming a band at the low
        // end is configuration, and its own sensitivity applies instead.
        for t in self.thr.iter_mut() {
            *t = f32::INFINITY;
        }
        if cfg.global_on && ch == CH_STEREO {
            for i in lo_bin..=hi_bin {
                let f = i as f32 * bin_hz;
                let penalty = 6.0 * ((120.0 / f.max(1.0)).log2() / 2.5).clamp(0.0, 1.0);
                self.thr[i] = cfg.threshold_db + penalty;
            }
        }
        for band in cfg.bands.iter().filter(|b| b.active) {
            if band.channel as usize != ch || band.freq <= 0.0 {
                continue;
            }
            let lo = (band.freq * 2f32.powf(-band.width_oct)).max(cfg.low_hz);
            let hi = (band.freq * 2f32.powf(band.width_oct)).min(self.sr * 0.45);
            let a = ((lo / bin_hz).floor().max(lo_bin as f32) as usize).min(hi_bin);
            let b = ((hi / bin_hz).ceil() as usize).clamp(a, hi_bin);
            let t = cfg.threshold_db - band.sens_db;
            for i in a..=b {
                self.thr[i] = self.thr[i].min(t);
            }
        }

        // -- pick peaks ----------------------------------------------------
        self.peaks.clear();
        {
            let state = self.channels[ch].as_ref().unwrap();
            let mut i = lo_bin.max(2);
            while i <= hi_bin {
                let p = self.prom[i];
                let over = p - self.thr[i];
                if over > 0.0
                    && p >= self.prom[i - 1]
                    && p > self.prom[i + 1]
                    && state.smoothed_db[i] > FLOOR_DB
                {
                    // Parabolic interpolation for a sub-bin centre.
                    let denom = self.prom[i - 1] - 2.0 * p + self.prom[i + 1];
                    let delta = if denom.abs() > 1e-6 {
                        (0.5 * (self.prom[i - 1] - self.prom[i + 1]) / denom).clamp(-0.5, 0.5)
                    } else {
                        0.0
                    };
                    let f = (i as f32 + delta) * bin_hz;

                    // Walk to the half-prominence points for a width, no more
                    // than an octave out either side.
                    let half = p - 3.0;
                    let max_walk = ((i as f32 * 0.5) as usize).max(2);
                    let mut a = i;
                    while a > i.saturating_sub(max_walk).max(1) && self.prom[a - 1] > half {
                        a -= 1;
                    }
                    let mut b = i;
                    while b + 1 < bins - 1 && b < i + max_walk && self.prom[b + 1] > half {
                        b += 1;
                    }

                    self.peaks.push(Peak {
                        log2f: f.max(1.0).log2(),
                        excess: over,
                        lo_l2: (a as f32 * bin_hz).max(1.0).log2(),
                        hi_l2: (b as f32 * bin_hz).max(1.0).log2(),
                    });
                    // Skip past this peak's right flank so its shoulder bins
                    // cannot register as peaks of their own.
                    i = b + 1;
                } else {
                    i += 1;
                }
            }
        }

        // -- cluster -------------------------------------------------------
        // Bins belonging to one physical resonance become one target. Peaks
        // arrive in ascending frequency; a peak that starts inside the last
        // cluster's extent — or within a sixth of an octave of it — joins it.
        self.clusters.clear();
        for peak in self.peaks.iter() {
            let joined = match self.clusters.last_mut() {
                Some(c) if peak.lo_l2 <= c.hi_l2 + 1.0 / 6.0 => {
                    c.sum_w += peak.excess;
                    c.sum_wl2 += peak.excess * peak.log2f;
                    c.excess = c.excess.max(peak.excess);
                    c.hi_l2 = c.hi_l2.max(peak.hi_l2);
                    true
                }
                _ => false,
            };
            if !joined {
                self.clusters.push(Cluster {
                    sum_w: peak.excess,
                    sum_wl2: peak.excess * peak.log2f,
                    excess: peak.excess,
                    lo_l2: peak.lo_l2,
                    hi_l2: peak.hi_l2,
                    harmonic: false,
                });
            }
        }

        // -- harmonic guard ------------------------------------------------
        // Three or more clusters on an integer series are a note, not a rack
        // of resonances. Its members stay visible to band regions — pointing
        // a band at a partial is configuration — but the global scan will not
        // publish them: "the mix has harmonics" is not a finding.
        flag_harmonic_combs(&mut self.clusters);

        // -- update tracks -------------------------------------------------
        let sel = cfg.selectivity.clamp(0.0, 1.0);
        let rates = self.rates;
        let state = self.channels[ch].as_mut().unwrap();
        for t in state.tracks.iter_mut() {
            t.claimed = false;
        }

        // Match clusters to tracks, strongest cluster first so the resonance
        // that matters most gets the best continuation. Distance is measured
        // in octaves — log-frequency — because 3.0 → 3.1 kHz is the same
        // resonance drifting while 100 → 120 Hz is a different note.
        self.clusters
            .sort_unstable_by(|a, b| b.excess.partial_cmp(&a.excess).unwrap_or(std::cmp::Ordering::Equal));
        for cluster in self.clusters.iter() {
            let cl2 = cluster.log2f();
            let mut best: Option<usize> = None;
            let mut best_cost = f32::INFINITY;
            for (i, t) in state.tracks.iter().enumerate() {
                if t.id == 0 || t.claimed {
                    continue;
                }
                let d = (t.log2f - cl2).abs();
                // Beyond a sixth of an octave or so it is a different
                // resonance — generous against drift and vibrato, tight
                // enough that stray blips rarely graze an existing track.
                if d > 0.18 {
                    continue;
                }
                // Deterministic cost: distance dominates; a small Q-mismatch
                // term breaks ties between two nearby tracks in favour of the
                // one whose shape this cluster continues.
                let cost = d + 0.02 * ((cluster.q().ln() - t.q.max(0.1).ln()).abs());
                if cost < best_cost {
                    best_cost = cost;
                    best = Some(i);
                }
            }
            let slot = best.or_else(|| {
                state
                    .tracks
                    .iter()
                    .position(|t| t.id == 0)
                    .or_else(|| {
                        // Full: replace the weakest unclaimed track.
                        state
                            .tracks
                            .iter()
                            .enumerate()
                            .filter(|(_, t)| !t.claimed)
                            .min_by(|a, b| {
                                a.1.score()
                                    .partial_cmp(&b.1.score())
                                    .unwrap_or(std::cmp::Ordering::Equal)
                            })
                            .map(|(i, _)| i)
                    })
            });
            let Some(slot) = slot else { continue };

            let t = &mut state.tracks[slot];
            if t.id == 0 || best.is_none() {
                *t = Track {
                    id: self.next_id,
                    log2f: cl2,
                    q: cluster.q(),
                    excess: cluster.excess,
                    conf: 0.0,
                    age_s: 0.0,
                    streak_s: rates.dt,
                    missing_s: 0.0,
                    claimed: true,
                    harmonic: cluster.harmonic,
                };
                self.next_id = self.next_id.wrapping_add(1).max(1);
            } else {
                t.log2f += (cl2 - t.log2f) * rates.freq;
                t.q += (cluster.q() - t.q) * rates.q;
                let a = if cluster.excess > t.excess {
                    rates.excess_up
                } else {
                    rates.excess_dn
                };
                t.excess += (cluster.excess - t.excess) * a;
                t.age_s += rates.dt;
                t.streak_s += rates.dt;
                t.missing_s = 0.0;
                t.claimed = true;
                t.harmonic = cluster.harmonic;
            }

            // Confidence: prominence says how much it matters, narrowness says
            // how much it looks like a resonance rather than tone, and the
            // one-pole *is* the persistence — a peak has to keep showing up
            // across frames to get anywhere near 1. This is the *detection*
            // envelope; audible gain has its own ballistics in the pool.
            let t = &mut state.tracks[slot];
            let strength = smoothstep(t.excess / 6.0);
            let narrow = smoothstep((t.q - 3.0) / 8.0);
            let want = strength * (1.0 - sel * 0.6 * (1.0 - narrow));
            let a = if want > t.conf {
                rates.conf_up
            } else {
                rates.conf_dn
            };
            t.conf += (want - t.conf) * a;
        }

        // Tracks nothing matched decay, controlled rather than cliff-edged —
        // a track survives well past a lost frame or three, which is what
        // feeds the pool's hold behaviour instead of retriggering it.
        for t in state.tracks.iter_mut() {
            if t.id != 0 && !t.claimed {
                t.conf *= rates.conf_decay;
                t.streak_s = 0.0;
                t.missing_s += rates.dt;
                if t.conf < 0.05 && t.missing_s > 0.15 {
                    t.id = 0;
                }
            }
        }
    }

    /// Arbitrate targets between band regions and the global scan, cap by the
    /// quality tier, and publish one snapshot.
    fn select_and_publish(&mut self, shared: &SharedSpectral, cfg: &ConfigView) {
        self.selected.clear();

        // Per-pass claim marks, so overlapping regions cannot mint two targets
        // out of one track.
        for state in self.channels.iter_mut().flatten() {
            for t in state.tracks.iter_mut() {
                t.claimed = false;
            }
        }

        // Bands claim first, nearest-band-wins by slot order; each band takes
        // at most a handful so one wide region cannot hog the pool.
        for (slot, band) in cfg.bands.iter().enumerate() {
            if !band.active || band.freq <= 0.0 {
                continue;
            }
            let ch = (band.channel as usize).min(CHANNELS - 1);
            let Some(state) = self.channels[ch].as_mut() else {
                continue;
            };
            let centre = band.freq.log2();
            let mut taken = 0;
            // Strongest first within the region.
            let mut order: [usize; TRACKS] = std::array::from_fn(|i| i);
            let tracks = &mut state.tracks;
            order.sort_unstable_by(|a, b| {
                tracks[*b]
                    .score()
                    .partial_cmp(&tracks[*a].score())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            for i in order {
                if taken >= 4 {
                    break;
                }
                let t = &mut tracks[i];
                if t.id == 0 || t.claimed || !t.publishable() {
                    continue;
                }
                if (t.log2f - centre).abs() > band.width_oct {
                    continue;
                }
                t.claimed = true;
                taken += 1;
                self.selected.push((
                    t.score(),
                    WireTarget {
                        track: t.id,
                        freq: 2f32.powf(t.log2f),
                        q: t.q,
                        excess_db: t.excess,
                        confidence: t.conf,
                        owner: slot as i16,
                        channel: band.channel,
                    },
                ));
            }
        }

        // The global scan takes whatever is left on the stereo bus — except
        // harmonic-comb members, which it leaves to explicit band regions.
        if cfg.global_on {
            if let Some(state) = self.channels[CH_STEREO].as_mut() {
                for t in state.tracks.iter_mut() {
                    if t.id == 0 || t.claimed || !t.publishable() || t.harmonic {
                        continue;
                    }
                    let f = 2f32.powf(t.log2f);
                    if f < cfg.low_hz || f > cfg.high_hz {
                        continue;
                    }
                    t.claimed = true;
                    self.selected.push((
                        t.score(),
                        WireTarget {
                            track: t.id,
                            freq: f,
                            q: t.q,
                            excess_db: t.excess,
                            confidence: t.conf,
                            owner: -1,
                            channel: CH_STEREO as u8,
                        },
                    ));
                }
            }
        }

        // Established tracks carry their persistence bonus into the cap, so
        // ranking wiggle between two similar resonances cannot churn which of
        // them holds the last slot.
        self.selected.sort_unstable_by(|a, b| {
            b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)
        });
        let cap = match cfg.quality {
            0 => 8,
            1 => 16,
            _ => 24,
        };
        self.selected.truncate(cap.min(MAX_TARGETS));

        let mut frame = TargetFrame::default();
        self.serial = self.serial.wrapping_add(1);
        frame.serial = self.serial;
        frame.count = self.selected.len() as u32;
        for (dst, (_, t)) in frame.targets.iter_mut().zip(self.selected.iter()) {
            *dst = *t;
        }
        self.was_publishing = frame.count > 0;
        shared.frames.publish(&mut self.back, frame);
    }
}

/// Mark clusters that sit on an integer-multiple series of one of the lowest
/// clusters. Three members within two percent of their ideal spot is a note.
///
/// Deliberately conservative in both directions: a lone resonance can never be
/// flagged (it has no series), and inharmonic sets — room modes, rattles,
/// feedback — do not land on integer ratios of a shared fundamental.
fn flag_harmonic_combs(clusters: &mut [Cluster]) {
    let n = clusters.len();
    if n < 3 {
        return;
    }
    // Clusters arrive in ascending frequency. The lowest few are the
    // fundamental candidates — each tried as the fundamental itself and as
    // its second or third harmonic, in case the fundamental never peaked.
    for base in 0..n.min(3) {
        let fb = 2f32.powf(clusters[base].log2f());
        for div in 1..=3u32 {
            let candidate = fb / div as f32;
            if candidate < 40.0 {
                continue;
            }

            // Pass one, loose: gather plausible members and refine the
            // fundamental from them. A low fundamental's measured centre can
            // sit a few percent off — bin resolution down there is coarse —
            // and every ratio inherits that error, so the raw candidate is
            // only good enough to find the members that vote. Only low-order
            // members vote (k ≤ 6): one loud peak at a high k could otherwise
            // drag the fit onto itself and manufacture a comb out of junk.
            let mut num = 0.0f32;
            let mut den = 0.0f32;
            for c in clusters.iter() {
                let f = 2f32.powf(c.log2f());
                let ratio = f / candidate;
                let k = ratio.round();
                if (1.0..=6.0).contains(&k) && (ratio - k).abs() <= 0.06 * k {
                    num += c.excess * (f / k);
                    den += c.excess;
                }
            }
            if den <= 0.0 {
                continue;
            }
            let f0 = num / den;

            // Pass two, strict: a member is a cluster whose extent a harmonic
            // line crosses — which also catches adjacent partials the
            // clusterer merged into one wide cluster. A real note has dense
            // low partials of comparable weight, so the comb needs three
            // members within earshot of the strongest one's excess, at least
            // two of them in the first four harmonics, before anything is
            // flagged — one towering peak plus a couple of sub-dB blips that
            // happen to line up is not a note.
            let mex = clusters
                .iter()
                .filter(|c| comb_order(c, f0).is_some())
                .fold(0.0f32, |acc, c| acc.max(c.excess));
            let significant = (0.15 * mex).max(3.0);
            let mut hits = 0usize;
            let mut low_hits = 0usize;
            for c in clusters.iter() {
                if c.excess < significant {
                    continue;
                }
                if let Some(k) = comb_order(c, f0) {
                    hits += 1;
                    if k <= 4 {
                        low_hits += 1;
                    }
                }
            }
            if hits >= 3 && low_hits >= 2 {
                for c in clusters.iter_mut() {
                    if comb_order(c, f0).is_some() {
                        c.harmonic = true;
                    }
                }
            }
        }
    }
}

/// The lowest harmonic of `f0` that falls inside this cluster's extent (plus
/// a little slack), or `None`. Orders past twelve do not count — that far up
/// a comb's teeth are closer together than a resonance is wide, and anything
/// there is better judged on its own merits. Clusters wider than a few
/// harmonic spacings are excused too: a genuinely broad resonance should not
/// be flagged just because a comb exists somewhere else in the spectrum.
fn comb_order(c: &Cluster, f0: f32) -> Option<u32> {
    let lo = 2f32.powf(c.lo_l2);
    let hi = 2f32.powf(c.hi_l2);
    if hi - lo > 2.5 * f0 {
        return None;
    }
    let slack = 0.025 * 2f32.powf(c.log2f());
    let k_lo = ((lo - slack) / f0).ceil().max(1.0);
    let k_hi = ((hi + slack) / f0).floor().min(12.0);
    (k_lo <= k_hi).then_some(k_lo as u32)
}

// --- the worker thread -------------------------------------------------------

/// Owns the analysis thread; dropping it shuts the thread down and joins it.
pub struct SpectralWorker {
    shared: Arc<SharedSpectral>,
    handle: Option<JoinHandle<()>>,
}

impl SpectralWorker {
    pub fn spawn(shared: Arc<SharedSpectral>) -> Self {
        let for_thread = shared.clone();
        let handle = std::thread::Builder::new()
            .name("equzx-spectral".into())
            .spawn(move || worker_loop(&for_thread))
            .ok();
        Self { shared, handle }
    }
}

impl Drop for SpectralWorker {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn worker_loop(shared: &SharedSpectral) {
    let mut detector = Detector::new(shared.cfg.snapshot().sample_rate);
    let mut last_pos = shared.ring.position();
    loop {
        if shared.shutdown.load(Ordering::Relaxed) {
            return;
        }
        let cfg = shared.cfg.snapshot();
        let demanded = cfg.global_on || cfg.bands.iter().any(|b| b.active);
        if !demanded {
            detector.go_idle(shared);
            last_pos = shared.ring.position();
            std::thread::sleep(std::time::Duration::from_millis(25));
            continue;
        }
        let pos = shared.ring.position();
        // Interleaved, so a hop of samples is two hops of ring values.
        if pos.wrapping_sub(last_pos) >= detector.hop * 2 {
            last_pos = pos;
            detector.analyze(shared);
        }
        std::thread::sleep(std::time::Duration::from_millis(3));
    }
}

// --- the adaptive filter pool (audio thread — no locks, no allocation) -------

/// What the UI reads about one pool slot.
#[derive(Clone, Copy, Default, Debug)]
pub struct TargetView {
    /// 0 marks an empty slot.
    pub freq: f32,
    pub cut_db: f32,
    pub q: f32,
    pub confidence: f32,
}

impl TargetView {
    pub fn is_some(&self) -> bool {
        self.freq > 0.0
    }

    /// Actively attenuating, as opposed to a detected candidate.
    pub fn is_active(&self) -> bool {
        self.cut_db > 0.05
    }
}

/// Per-quality behaviour of the pool — the tuning that makes Live/Ultra
/// conservative and High surgical.
///
/// These are the values the smoothness of the whole system stands on, so
/// they are data, not code: the engine derives them from the quality tier,
/// and tests substitute their own.
#[derive(Clone, Copy)]
pub struct PoolTuning {
    /// Confidence a target needs before its filter engages at all.
    pub on_conf: f32,
    /// Confidence below which an engaged filter enters Hold — well under
    /// `on_conf`, which is the hysteresis that stops chatter at one line.
    pub off_conf: f32,
    /// Confidence at which a held filter resumes without re-proving itself.
    pub rearm_conf: f32,
    /// How long a vanished resonance keeps its cut frozen before releasing.
    pub hold_s: f32,
    /// How long a freed slot rests before it may be claimed again.
    pub cooldown_s: f32,
    /// Floors under the user's ballistics for automatically found targets.
    pub min_attack_ms: f32,
    pub min_release_ms: f32,
    /// Cap on the global scan's automatic cuts, in dB. Band-owned targets
    /// are explicit configuration and keep the band's own Range.
    pub max_cut_db: f32,
    /// The Q corridor filters are allowed to live in.
    pub q_min: f32,
    pub q_max: f32,
    /// How fast a filter may chase a moving resonance, octaves per second.
    pub slew_oct_s: f32,
}

impl PoolTuning {
    /// The tier's tuning. Ultra is the live-performance profile: shallow,
    /// wide-ish, slow-moving cuts that are never worth noticing; High trusts
    /// the detector with depth and speed.
    pub fn for_quality(q: ResQuality) -> Self {
        match q {
            ResQuality::Ultra => Self {
                on_conf: 0.70,
                off_conf: 0.40,
                rearm_conf: 0.55,
                hold_s: 0.060,
                cooldown_s: 0.030,
                min_attack_ms: 15.0,
                min_release_ms: 150.0,
                max_cut_db: 6.0,
                q_min: 1.5,
                q_max: 10.0,
                slew_oct_s: 1.5,
            },
            ResQuality::Balanced => Self {
                on_conf: 0.65,
                off_conf: 0.38,
                rearm_conf: 0.50,
                hold_s: 0.050,
                cooldown_s: 0.020,
                min_attack_ms: 10.0,
                min_release_ms: 100.0,
                max_cut_db: 12.0,
                q_min: 1.0,
                q_max: 16.0,
                slew_oct_s: 3.0,
            },
            ResQuality::High => Self {
                on_conf: 0.60,
                off_conf: 0.35,
                rearm_conf: 0.45,
                hold_s: 0.040,
                cooldown_s: 0.015,
                min_attack_ms: 5.0,
                min_release_ms: 60.0,
                max_cut_db: 24.0,
                q_min: 0.7,
                q_max: 24.0,
                slew_oct_s: 6.0,
            },
        }
    }

    /// Everything wide open — for tests that want to observe raw pool
    /// mechanics without the musical safety rails.
    pub fn permissive() -> Self {
        Self {
            on_conf: 0.5,
            off_conf: 0.3,
            rearm_conf: 0.4,
            hold_s: 0.005,
            cooldown_s: 0.0,
            min_attack_ms: 0.0,
            min_release_ms: 0.0,
            max_cut_db: 36.0,
            q_min: 0.5,
            q_max: 40.0,
            slew_oct_s: 1000.0,
        }
    }
}

/// Global-side controls for the pool, resolved once per control block.
#[derive(Clone, Copy)]
pub struct PoolControls {
    pub global_on: bool,
    pub amount: f32,
    pub range_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    /// Pool budget from the quality tier.
    pub max_slots: usize,
    pub tuning: PoolTuning,
    pub bands: [BandCtl; MAX_BANDS],
}

impl Default for PoolControls {
    fn default() -> Self {
        Self {
            global_on: false,
            amount: 0.5,
            range_db: 36.0,
            attack_ms: 5.0,
            release_ms: 40.0,
            max_slots: 8,
            tuning: PoolTuning::for_quality(ResQuality::Ultra),
            bands: [BandCtl::default(); MAX_BANDS],
        }
    }
}

/// One band's say over the targets it owns.
#[derive(Clone, Copy, Default)]
pub struct BandCtl {
    pub active: bool,
    pub amount: f32,
    pub range_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
}

/// Where a slot's filter stands in its life.
///
/// ```text
/// Free ── claim (candidate) ──► Standby ── conf ≥ on ──► Active
///   ▲                             │  ▲                      │ conf ≤ off
///   │                             │  └── conf ≥ rearm ── Hold (timer)
///   │                             │                         │ expires
///   └── drained + cooldown ◄── Release ◄────────────────────┘
/// ```
///
/// Hysteresis lives in the thresholds (`on` well above `off`), hold in the
/// timer, and click-safety in the fact that every transition only ever moves
/// a *gain target* the ballistics then chase — coefficients never jump.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SlotState {
    /// Claimed by a track the detector is still weighing. No cut; the
    /// frequency glides after the target so an engage starts in tune.
    Standby,
    Active,
    /// The resonance went quiet or doubtful; the cut is frozen, not
    /// released, until the timer runs out — one lost FFT frame is not a
    /// reason to pump.
    Hold,
    Release,
}

struct PoolSlot {
    /// 0 = free.
    track: u32,
    owner: i16,
    channel: u8,
    /// Latest word from the detector.
    excess_db: f32,
    confidence: f32,
    in_frame: bool,

    state: SlotState,
    /// Seconds of Hold left.
    hold_remaining: f32,
    /// The cut the Hold state froze.
    held_cut: f32,
    /// Seconds a freed slot still has to rest before reuse.
    cooldown: f32,

    /// Smoothed filter state. Frequency lives in log2 so a moving resonance
    /// is chased along the axis the ear hears, not the one the FFT bins use.
    freq_l2: f32,
    q: f32,
    cut_db: f32,
    tgt_freq_l2: f32,
    tgt_q: f32,

    coeffs: Coeffs,
    key_freq: f32,
    key_q: f32,
    key_cut: f32,
    a: Biquad,
    b: Biquad,
    idle: u8,
}

impl Default for PoolSlot {
    fn default() -> Self {
        Self {
            track: 0,
            owner: -1,
            channel: 0,
            excess_db: 0.0,
            confidence: 0.0,
            in_frame: false,
            state: SlotState::Standby,
            hold_remaining: 0.0,
            held_cut: 0.0,
            cooldown: 0.0,
            freq_l2: 10.0,
            q: 8.0,
            cut_db: 0.0,
            tgt_freq_l2: 10.0,
            tgt_q: 8.0,
            coeffs: Coeffs::identity(),
            key_freq: 0.0,
            key_q: 0.0,
            key_cut: 0.0,
            a: Biquad::new(),
            b: Biquad::new(),
            idle: IDLE_BLOCKS,
        }
    }
}

/// The preallocated adaptive filter bank the audio thread runs.
///
/// Slots are reused, never allocated: a target that disappears releases its
/// slot's cut to zero, drains the delay line through near-identity
/// coefficients, and only then frees the slot for the next resonance.
pub struct AdaptivePool {
    sr: f32,
    slots: [PoolSlot; MAX_TARGETS],
    front: u8,
    latest: TargetFrame,
}

impl AdaptivePool {
    pub fn new(sr: f32) -> Self {
        Self {
            sr,
            slots: std::array::from_fn(|_| PoolSlot::default()),
            front: 2,
            latest: TargetFrame::default(),
        }
    }

    pub fn set_sample_rate(&mut self, sr: f32) {
        self.sr = sr;
        self.reset();
    }

    pub fn reset(&mut self) {
        for slot in self.slots.iter_mut() {
            *slot = PoolSlot::default();
        }
        self.latest = TargetFrame::default();
    }

    /// Anything still filtering — including slots draining out.
    pub fn busy(&self) -> bool {
        self.slots
            .iter()
            .any(|s| s.track != 0 || s.idle < IDLE_BLOCKS)
    }

    pub fn peak_cut(&self) -> f32 {
        self.slots.iter().fold(0.0f32, |acc, s| acc.max(s.cut_db))
    }

    /// Snapshot for the UI. `out` should hold [`MAX_TARGETS`] entries.
    pub fn view(&self, out: &mut [TargetView]) {
        for (view, slot) in out.iter_mut().zip(self.slots.iter()) {
            *view = if slot.track != 0 {
                TargetView {
                    freq: 2f32.powf(slot.freq_l2),
                    cut_db: slot.cut_db,
                    q: slot.q,
                    confidence: slot.confidence,
                }
            } else {
                TargetView::default()
            };
        }
    }

    /// Take the newest detector frame, if any, and advance every slot's
    /// smoothed state one control block.
    pub fn update(&mut self, frames: &TripleBuffer<TargetFrame>, ctl: &PoolControls, dt: f32) {
        if let Some(frame) = frames.read(&mut self.front) {
            self.latest = frame;
            self.apply_frame(ctl);
        }
        let tun = &ctl.tuning;

        for slot in self.slots.iter_mut() {
            if slot.track == 0 {
                slot.cooldown = (slot.cooldown - dt).max(0.0);
                continue;
            }

            // Whose word governs this slot — the owning band's, or the global
            // stage's. An owner that has gone away (band removed, mode
            // changed) releases rather than freezing mid-cut.
            let (live, amount, range, attack, release) = if slot.owner >= 0 {
                let b = &ctl.bands[(slot.owner as usize).min(MAX_BANDS - 1)];
                // A band-owned target is explicit configuration: its Range
                // stands, its ballistics get only a small click-safety floor.
                (
                    b.active,
                    b.amount,
                    b.range_db,
                    b.attack_ms.max(2.0),
                    b.release_ms.max(20.0),
                )
            } else {
                // The global scan's automatic cuts wear the quality tier's
                // safety rails: capped depth, floored ballistics.
                (
                    ctl.global_on,
                    ctl.amount,
                    ctl.range_db.min(tun.max_cut_db),
                    ctl.attack_ms.max(tun.min_attack_ms),
                    ctl.release_ms.max(tun.min_release_ms),
                )
            };

            // The confidence the state machine judges. A target the detector
            // stopped publishing, or whose owner went away, reads as zero —
            // which routes it through Hold and Release like any other fade.
            let conf = if live && slot.in_frame {
                slot.confidence
            } else {
                0.0
            };

            // Hysteresis and hold — the anti-chatter core. Note what is NOT
            // here: confidence never scales the gain. Between the thresholds
            // the cut simply keeps doing what it was doing, which is what
            // removes the frame-rate wobble the old smooth gate had.
            match slot.state {
                SlotState::Standby => {
                    if conf >= tun.on_conf {
                        slot.state = SlotState::Active;
                    }
                }
                SlotState::Active => {
                    if conf < tun.off_conf {
                        slot.state = SlotState::Hold;
                        slot.hold_remaining = tun.hold_s;
                        slot.held_cut = slot.cut_db;
                    }
                }
                SlotState::Hold => {
                    if conf >= tun.rearm_conf {
                        slot.state = SlotState::Active;
                    } else {
                        slot.hold_remaining -= dt;
                        if slot.hold_remaining <= 0.0 {
                            slot.state = SlotState::Release;
                        }
                    }
                }
                SlotState::Release => {
                    if conf >= tun.on_conf {
                        slot.state = SlotState::Active;
                    }
                }
            }

            let want = match slot.state {
                SlotState::Active => {
                    let cut = (slot.excess_db * amount.clamp(0.0, 1.0)).min(range.max(0.0));
                    slot.held_cut = cut;
                    cut
                }
                SlotState::Hold => slot.held_cut,
                SlotState::Standby | SlotState::Release => 0.0,
            };

            let tau = if want > slot.cut_db { attack } else { release };
            slot.cut_db = step_toward(slot.cut_db, want, tau, dt);

            // The filter glides after the detector in log-frequency, one-pole
            // smoothed and then slew-limited: proportional pursuit for small
            // drifts, a hard octaves-per-second ceiling for big ones, so a
            // retargeted filter walks rather than teleports.
            let desired = step_toward(slot.freq_l2, slot.tgt_freq_l2, 40.0, dt);
            let max_step = tun.slew_oct_s * dt;
            slot.freq_l2 += (desired - slot.freq_l2).clamp(-max_step, max_step);
            slot.q = step_toward(slot.q, slot.tgt_q.clamp(tun.q_min, tun.q_max), 60.0, dt);

            if slot.cut_db < 0.02 {
                if slot.idle < IDLE_BLOCKS {
                    slot.idle += 1;
                    if slot.idle == IDLE_BLOCKS {
                        // The delay line has drained through near-identity
                        // coefficients; park the filter completely.
                        slot.a.reset();
                        slot.b.reset();
                        slot.coeffs = Coeffs::identity();
                        slot.key_cut = 0.0;
                        slot.key_freq = slot.freq_l2;
                        slot.key_q = slot.q;
                    }
                }
                // Free only once fully drained AND genuinely gone — and even
                // then the slot rests for the cooldown before reuse, so a
                // borderline target cannot free/claim/free every few frames.
                if slot.idle >= IDLE_BLOCKS
                    && !slot.in_frame
                    && matches!(slot.state, SlotState::Standby | SlotState::Release)
                {
                    slot.track = 0;
                    slot.cooldown = tun.cooldown_s;
                    continue;
                }
            } else {
                slot.idle = 0;
            }

            // Rebuild coefficients only when something audible moved, and
            // never while parked at zero — a Standby slot tracking its target
            // across the spectrum costs no trigonometry.
            if slot.idle < IDLE_BLOCKS
                && ((slot.cut_db - slot.key_cut).abs() > 0.005
                    || (slot.freq_l2 - slot.key_freq).abs() > 0.0007
                    || (slot.q - slot.key_q).abs() > 0.01)
            {
                slot.key_cut = slot.cut_db;
                slot.key_freq = slot.freq_l2;
                slot.key_q = slot.q;
                let freq = 2f32.powf(slot.freq_l2).clamp(20.0, self.sr * 0.45);
                slot.coeffs = Coeffs::peaking(
                    freq,
                    slot.q.clamp(tun.q_min, tun.q_max),
                    -slot.cut_db,
                    self.sr,
                );
            }
        }
    }

    /// Match the latest frame's targets to slots — by track id first, then by
    /// claiming free, rested slots. Slot ownership is stable on purpose: a
    /// track keeps its slot for life whatever the ranking does, a slot
    /// mid-cut is never stolen, and a replacement is always two independent
    /// fades — the old slot releasing, the new one attacking — never a
    /// coefficient reset.
    fn apply_frame(&mut self, ctl: &PoolControls) {
        for slot in self.slots.iter_mut() {
            slot.in_frame = false;
        }
        let count = (self.latest.count as usize)
            .min(MAX_TARGETS)
            .min(ctl.max_slots);
        let tun = &ctl.tuning;

        // Copy out to dodge the borrow of `self.latest` while slots mutate.
        let targets = self.latest.targets;
        for t in targets[..count].iter().filter(|t| t.track != 0) {
            if let Some(slot) = self.slots.iter_mut().find(|s| s.track == t.track) {
                slot.in_frame = true;
                slot.owner = t.owner;
                slot.channel = t.channel;
                slot.excess_db = t.excess_db;
                slot.confidence = t.confidence;
                slot.tgt_freq_l2 = t.freq.max(1.0).log2();
                slot.tgt_q = t.q.clamp(tun.q_min, tun.q_max);
                continue;
            }
            // New tracks claim only slots that are free AND past their
            // cooldown — the pool holds one physical slot per publishable
            // target, so under the honest lifecycle there is always room.
            let Some(slot) = self
                .slots
                .iter_mut()
                .find(|s| s.track == 0 && s.cooldown <= 0.0)
            else {
                continue;
            };
            let l2 = t.freq.max(1.0).log2();
            let q = t.q.clamp(tun.q_min, tun.q_max);
            *slot = PoolSlot {
                track: t.track,
                owner: t.owner,
                channel: t.channel,
                excess_db: t.excess_db,
                confidence: t.confidence,
                in_frame: true,
                state: SlotState::Standby,
                freq_l2: l2,
                q,
                cut_db: 0.0,
                tgt_freq_l2: l2,
                tgt_q: q,
                idle: IDLE_BLOCKS,
                ..PoolSlot::default()
            };
        }
    }

    /// Run the filters over one control block, in place. `right` is `None` on
    /// mono, where mid means the whole signal and side has nothing to act on —
    /// the same reading the EQ bands give those words.
    pub fn process(&mut self, left: &mut [f32], right: Option<&mut [f32]>, n: usize) {
        let mut right = right;
        let stereo = right.is_some();

        let mut any_ms = false;
        for slot in self.slots.iter_mut() {
            if slot.idle >= IDLE_BLOCKS {
                continue;
            }
            match slot.channel {
                1 | 2 if stereo => any_ms = true,
                _ => {
                    let c = slot.coeffs;
                    match slot.channel {
                        // Stereo: one filter, both channels — linked on
                        // purpose, so the image cannot wander.
                        0 => {
                            for x in left[..n].iter_mut() {
                                *x = slot.a.process(*x, &c);
                            }
                            if let Some(r) = right.as_deref_mut() {
                                for x in r[..n].iter_mut() {
                                    *x = slot.b.process(*x, &c);
                                }
                            }
                        }
                        3 => {
                            for x in left[..n].iter_mut() {
                                *x = slot.a.process(*x, &c);
                            }
                        }
                        4 => {
                            if let Some(r) = right.as_deref_mut() {
                                for x in r[..n].iter_mut() {
                                    *x = slot.b.process(*x, &c);
                                }
                            }
                        }
                        // Mid on mono is the signal itself; side is nothing.
                        1 => {
                            for x in left[..n].iter_mut() {
                                *x = slot.a.process(*x, &c);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        // Mid/side targets need the pair in that domain; one encode, all the
        // M/S slots, one decode.
        if any_ms {
            if let Some(r) = right {
                for i in 0..n {
                    let (l, rr) = (left[i], r[i]);
                    left[i] = 0.5 * (l + rr);
                    r[i] = 0.5 * (l - rr);
                }
                for slot in self.slots.iter_mut() {
                    if slot.idle >= IDLE_BLOCKS {
                        continue;
                    }
                    let c = slot.coeffs;
                    match slot.channel {
                        1 => {
                            for x in left[..n].iter_mut() {
                                *x = slot.a.process(*x, &c);
                            }
                        }
                        2 => {
                            for x in r[..n].iter_mut() {
                                *x = slot.b.process(*x, &c);
                            }
                        }
                        _ => {}
                    }
                }
                for i in 0..n {
                    let (m, s) = (left[i], r[i]);
                    left[i] = m + s;
                    r[i] = m - s;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    const SR: f32 = 48_000.0;

    #[test]
    fn fft_sizes_hold_the_bin_width_across_rates() {
        assert_eq!(fft_size_for(44_100.0), 2048);
        assert_eq!(fft_size_for(48_000.0), 2048);
        assert_eq!(fft_size_for(88_200.0), 4096);
        assert_eq!(fft_size_for(96_000.0), 4096);
        assert_eq!(fft_size_for(176_400.0), 8192);
        assert_eq!(fft_size_for(192_000.0), 8192);
        // The window stays ~43 ms everywhere, which is the reaction-time
        // budget the detector was designed around.
        for sr in [44_100.0, 96_000.0, 192_000.0f32] {
            let ms = fft_size_for(sr) as f32 / sr * 1000.0;
            assert!((35.0..55.0).contains(&ms), "{sr}: {ms} ms window");
        }
    }

    #[test]
    fn the_triple_buffer_hands_over_the_newest_value() {
        let tb = TripleBuffer::<u64>::default();
        let (mut back, mut front) = (0u8, 2u8);
        assert!(tb.read(&mut front).is_none());

        tb.publish(&mut back, 41);
        tb.publish(&mut back, 42);
        assert_eq!(tb.read(&mut front), Some(42));
        // Consumed: nothing new until the next publish.
        assert!(tb.read(&mut front).is_none());
        tb.publish(&mut back, 43);
        assert_eq!(tb.read(&mut front), Some(43));
    }

    #[test]
    fn the_ring_keeps_stereo_frames_paired() {
        let ring = AnalysisRing::default();
        for i in 0..RING_LEN {
            ring.push(i as f32, -(i as f32));
        }
        let mut l = [0.0f32; 16];
        let mut r = [0.0f32; 16];
        ring.read_latest(&mut l, &mut r);
        for i in 0..16 {
            assert_eq!(l[i], -r[i], "frame {i} split its pair");
        }
        assert_eq!(l[15], (RING_LEN - 1) as f32);
    }

    fn spectral_cfg() -> ConfigView {
        ConfigView {
            sample_rate: SR,
            global_on: true,
            quality: 2,
            threshold_db: 6.0,
            selectivity: 0.5,
            low_hz: 20.0,
            high_hz: 20_000.0,
            ..ConfigView::default()
        }
    }

    /// A sine evaluated with an f64 argument. The f32 version quantises its
    /// phase argument once it grows past a few thousand radians, and the
    /// resulting phase-modulation spurs are real spectral peaks the detector
    /// would honestly find.
    fn sine64(freq: f32, i: usize) -> f32 {
        (2.0 * std::f64::consts::PI * freq as f64 * i as f64 / SR as f64).sin() as f32
    }

    /// Deterministic noise, matching the resonance bank's test helper.
    struct Noise(u32);

    impl Noise {
        fn next(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (self.0 >> 8) as f32 / (1 << 23) as f32 - 1.0
        }
    }

    /// Fill the ring and run enough passes for confidence to settle.
    fn run_detector(
        shared: &SharedSpectral,
        detector: &mut Detector,
        passes: usize,
        mut gen: impl FnMut(usize) -> f32,
    ) {
        let mut at = 0;
        for _ in 0..passes {
            for _ in 0..detector.hop {
                let x = gen(at);
                shared.ring.push(x, x);
                at += 1;
            }
            detector.analyze(shared);
        }
    }

    /// Drain the reader side down to the newest frame. `front` must be the
    /// same reader index across every call on one buffer — the triple buffer's
    /// slot partition depends on it.
    fn latest_frame(shared: &SharedSpectral, front: &mut u8) -> TargetFrame {
        let mut last = TargetFrame::default();
        while let Some(f) = shared.frames.read(front) {
            last = f;
        }
        last
    }

    /// The headline requirement: one physical resonance — a sine, which leaks
    /// across several FFT bins through the window — must come out as ONE
    /// target, at the right frequency.
    #[test]
    fn a_tone_in_noise_becomes_exactly_one_target() {
        let shared = SharedSpectral::default();
        shared.cfg.publish(&spectral_cfg());
        let mut detector = Detector::new(SR);
        let mut front = 2u8;

        let mut noise = Noise(7);
        run_detector(&shared, &mut detector, 40, move |i| {
            sine64(3800.0, i) * 0.5 + noise.next() * 0.02
        });

        let frame = latest_frame(&shared, &mut front);
        assert_eq!(
            frame.count, 1,
            "expected one clustered target, got {}",
            frame.count
        );
        let t = frame.targets[0];
        assert!(
            (t.freq - 3800.0).abs() < 100.0,
            "target landed at {} Hz",
            t.freq
        );
        assert!(t.confidence > 0.5, "confidence stalled at {}", t.confidence);
        assert!(t.excess_db > 6.0, "excess read {}", t.excess_db);
        assert!(t.q >= 2.0 && t.q <= 36.0, "q was {}", t.q);
    }

    /// Two resonances far apart are two targets, not one smear.
    #[test]
    fn two_distant_tones_become_two_targets() {
        let shared = SharedSpectral::default();
        shared.cfg.publish(&spectral_cfg());
        let mut detector = Detector::new(SR);
        let mut front = 2u8;

        let mut noise = Noise(99);
        run_detector(&shared, &mut detector, 40, move |i| {
            sine64(700.0, i) * 0.4 + sine64(5200.0, i) * 0.4 + noise.next() * 0.02
        });

        let frame = latest_frame(&shared, &mut front);
        assert_eq!(frame.count, 2, "got {} targets", frame.count);
        let mut freqs: Vec<f32> = frame.targets[..2].iter().map(|t| t.freq).collect();
        freqs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert!((freqs[0] - 700.0).abs() < 40.0, "low target at {}", freqs[0]);
        assert!(
            (freqs[1] - 5200.0).abs() < 150.0,
            "high target at {}",
            freqs[1]
        );
    }

    /// False-positive control: broadband noise has no local prominence, so the
    /// detector must publish nothing.
    #[test]
    fn broadband_noise_produces_no_targets() {
        let shared = SharedSpectral::default();
        shared.cfg.publish(&spectral_cfg());
        let mut detector = Detector::new(SR);
        let mut front = 2u8;

        let mut noise = Noise(12345);
        run_detector(&shared, &mut detector, 40, move |_| noise.next() * 0.4);

        let frame = latest_frame(&shared, &mut front);
        assert_eq!(frame.count, 0, "noise minted {} targets", frame.count);
    }

    /// A harmonic series is tonal material, not a rack of resonances: the
    /// fundamental sits below the low-end penalty and the partials all stand
    /// on each other's shoulders. The detector may flag a couple of the
    /// strongest, but it must not carpet-bomb the series.
    #[test]
    fn a_harmonic_series_is_not_carpet_bombed() {
        let shared = SharedSpectral::default();
        let mut cfg = spectral_cfg();
        cfg.selectivity = 0.7;
        shared.cfg.publish(&cfg);
        let mut detector = Detector::new(SR);
        let mut front = 2u8;

        run_detector(&shared, &mut detector, 40, move |i| {
            let mut x = 0.0;
            for h in 1..=10 {
                x += sine64(110.0 * h as f32, i) * (1.0 / h as f32);
            }
            x * 0.3
        });

        let frame = latest_frame(&shared, &mut front);
        assert!(
            frame.count <= 4,
            "a harmonic series produced {} targets: {:?}",
            frame.count,
            &frame.targets[..frame.count as usize]
        );
    }

    /// A band region owns the targets inside it; the global scan gets the rest.
    #[test]
    fn band_regions_claim_their_targets_first() {
        let shared = SharedSpectral::default();
        let mut cfg = spectral_cfg();
        cfg.bands[3] = BandRegionView {
            active: true,
            channel: 0,
            freq: 3200.0,
            width_oct: 1.0,
            sens_db: 0.0,
        };
        shared.cfg.publish(&cfg);
        let mut detector = Detector::new(SR);
        let mut front = 2u8;

        // The tone sits at 3.84 kHz — inside the band's ±1 octave region but
        // well off its 3.2 kHz centre, which is the whole point: the band
        // tracks the resonance where it is, not where the band sits.
        run_detector(&shared, &mut detector, 40, move |i| {
            sine64(3840.0, i) * 0.5
        });

        let frame = latest_frame(&shared, &mut front);
        assert_eq!(
            frame.count,
            1,
            "targets: {:?}",
            &frame.targets[..frame.count as usize]
        );
        let t = frame.targets[0];
        assert_eq!(t.owner, 3, "the band did not claim its own region");
        assert!((t.freq - 3840.0).abs() < 100.0, "tracked to {} Hz", t.freq);
    }

    /// The same track keeps its id from frame to frame — the property the
    /// pool's slot reuse and glide smoothing stand on.
    #[test]
    fn a_steady_resonance_keeps_its_track_id() {
        let shared = SharedSpectral::default();
        shared.cfg.publish(&spectral_cfg());
        let mut detector = Detector::new(SR);
        let mut front = 2u8;

        let mut ids = Vec::new();
        let mut at = 0usize;
        for _ in 0..30 {
            for _ in 0..detector.hop {
                let x = sine64(1500.0, at) * 0.5;
                shared.ring.push(x, x);
                at += 1;
            }
            detector.analyze(&shared);
            let frame = latest_frame(&shared, &mut front);
            if frame.count > 0 {
                ids.push(frame.targets[0].track);
            }
        }
        assert!(ids.len() > 10);
        let first = ids[ids.len() / 2];
        assert!(
            ids[ids.len() / 2..].iter().all(|id| *id == first),
            "track id churned: {:?}",
            &ids[ids.len() / 2..]
        );
    }

    /// Pool + detector end to end: the published target must turn into real
    /// attenuation at the target frequency, smoothly, and release when the
    /// detector goes quiet.
    #[test]
    fn the_pool_attenuates_what_the_detector_publishes() {
        let shared = SharedSpectral::default();
        let mut pool = AdaptivePool::new(SR);
        let mut ctl = PoolControls {
            global_on: true,
            amount: 1.0,
            range_db: 12.0,
            attack_ms: 2.0,
            release_ms: 10.0,
            max_slots: 8,
            // Raw mechanics under test — the live safety rails have their
            // own coverage.
            tuning: PoolTuning::permissive(),
            ..PoolControls::default()
        };

        // Hand-published frame, as the worker would.
        let mut back = 0u8;
        let mut frame = TargetFrame {
            serial: 1,
            count: 1,
            ..TargetFrame::default()
        };
        frame.targets[0] = WireTarget {
            track: 7,
            freq: 1000.0,
            q: 8.0,
            excess_db: 9.0,
            confidence: 1.0,
            owner: -1,
            channel: 0,
        };
        shared.frames.publish(&mut back, frame);

        let dt = 32.0 / SR;
        let mut phase = 0.0f32;
        let mut out_rms = 0.0f64;
        let mut in_rms = 0.0f64;
        let mut counted = 0usize;
        for block in 0..600 {
            let mut l = [0.0f32; 32];
            let mut r = [0.0f32; 32];
            for i in 0..32 {
                phase += 2.0 * PI * 1000.0 / SR;
                l[i] = phase.sin();
                r[i] = l[i];
            }
            let input = l;
            pool.update(&shared.frames, &ctl, dt);
            pool.process(&mut l, Some(&mut r), 32);
            if block >= 300 {
                for i in 0..32 {
                    in_rms += (input[i] * input[i]) as f64;
                    out_rms += (l[i] * l[i]) as f64;
                    counted += 1;
                }
            }
        }
        let db = |s: f64| 10.0 * (s / counted as f64).max(1e-12).log10();
        let drop = db(in_rms) - db(out_rms);
        assert!(
            (drop - 9.0).abs() < 1.5,
            "expected ~9 dB of cut, measured {drop:.2}"
        );
        assert!(pool.peak_cut() > 8.0);

        // The detector retracts the target: the pool must release to silence
        // and free the slot.
        let empty = TargetFrame {
            serial: 2,
            count: 0,
            ..TargetFrame::default()
        };
        shared.frames.publish(&mut back, empty);
        for _ in 0..600 {
            let mut l = [0.0f32; 32];
            let mut r = [0.0f32; 32];
            pool.update(&shared.frames, &ctl, dt);
            pool.process(&mut l, Some(&mut r), 32);
        }
        assert!(pool.peak_cut() < 0.05, "cut stuck at {}", pool.peak_cut());
        assert!(!pool.busy(), "slots never drained");

        // And with the global stage off, a fresh target does nothing.
        ctl.global_on = false;
        let mut frame = TargetFrame {
            serial: 3,
            count: 1,
            ..TargetFrame::default()
        };
        frame.targets[0].track = 9;
        frame.targets[0].freq = 500.0;
        frame.targets[0].excess_db = 12.0;
        frame.targets[0].confidence = 1.0;
        shared.frames.publish(&mut back, frame);
        for _ in 0..100 {
            let mut l = [0.0f32; 32];
            pool.update(&shared.frames, &ctl, dt);
            pool.process(&mut l, None, 32);
        }
        assert!(pool.peak_cut() < 0.01);
    }

    /// A side-channel target must leave a mono (correlated) signal alone.
    #[test]
    fn a_side_target_does_not_touch_mono_material() {
        let shared = SharedSpectral::default();
        let mut pool = AdaptivePool::new(SR);
        let mut ctl = PoolControls::default();
        ctl.bands[0] = BandCtl {
            active: true,
            amount: 1.0,
            range_db: 24.0,
            attack_ms: 1.0,
            release_ms: 10.0,
        };

        let mut back = 0u8;
        let mut frame = TargetFrame {
            serial: 1,
            count: 1,
            ..TargetFrame::default()
        };
        frame.targets[0] = WireTarget {
            track: 3,
            freq: 1000.0,
            q: 8.0,
            excess_db: 12.0,
            confidence: 1.0,
            owner: 0,
            channel: 2, // side
        };
        shared.frames.publish(&mut back, frame);

        let dt = 32.0 / SR;
        let mut phase = 0.0f32;
        let mut peak = 0.0f32;
        for block in 0..400 {
            let mut l = [0.0f32; 32];
            let mut r = [0.0f32; 32];
            for i in 0..32 {
                phase += 2.0 * PI * 1000.0 / SR;
                l[i] = phase.sin();
                r[i] = l[i];
            }
            pool.update(&shared.frames, &ctl, dt);
            pool.process(&mut l, Some(&mut r), 32);
            if block > 200 {
                for i in 0..32 {
                    peak = peak.max(l[i].abs());
                    assert!((l[i] - r[i]).abs() < 1e-6, "the pair decorrelated");
                }
            }
        }
        assert!((peak - 1.0).abs() < 0.01, "mono material was cut to {peak}");
    }

    /// Retuning the target must glide the filter, not jump it: no sample-to-
    /// sample step in the output beyond what the tone itself does.
    #[test]
    fn a_moving_target_glides_without_clicks() {
        let shared = SharedSpectral::default();
        let mut pool = AdaptivePool::new(SR);
        let ctl = PoolControls {
            global_on: true,
            amount: 1.0,
            range_db: 12.0,
            attack_ms: 5.0,
            release_ms: 40.0,
            max_slots: 8,
            ..PoolControls::default()
        };

        let mut back = 0u8;
        let target = |freq: f32, serial: u32| {
            let mut f = TargetFrame {
                serial,
                count: 1,
                ..TargetFrame::default()
            };
            f.targets[0] = WireTarget {
                track: 1,
                freq,
                q: 8.0,
                excess_db: 10.0,
                confidence: 1.0,
                owner: -1,
                channel: 0,
            };
            f
        };
        shared.frames.publish(&mut back, target(1000.0, 1));

        let dt = 32.0 / SR;
        let mut phase = 0.0f32;
        let mut last = 0.0f32;
        let mut max_step = 0.0f32;
        for block in 0..1200 {
            // Move the target by a third of an octave mid-run.
            if block == 600 {
                shared.frames.publish(&mut back, target(1260.0, 2));
            }
            let mut l = [0.0f32; 32];
            for i in 0..32 {
                phase += 2.0 * PI * 1000.0 / SR;
                l[i] = phase.sin() * 0.5;
            }
            pool.update(&shared.frames, &ctl, dt);
            pool.process(&mut l, None, 32);
            for x in l {
                if block > 2 {
                    max_step = max_step.max((x - last).abs());
                }
                last = x;
            }
        }
        // A 1 kHz half-scale sine moves at most 2π·1000/48000·0.5 ≈ 0.065 a
        // sample; anything much past that is a discontinuity.
        assert!(max_step < 0.1, "output stepped by {max_step}");
    }

    // --- tracking regressions (spec: smooth tracking) ---------------------

    /// A narrow resonance sweeping 2 kHz → 4 kHz over four seconds must be
    /// ONE persistent track following it — not a chain of tracks being born
    /// and dying along the way.
    #[test]
    fn a_moving_resonance_stays_one_track() {
        let shared = SharedSpectral::default();
        shared.cfg.publish(&spectral_cfg());
        let mut detector = Detector::new(SR);
        let mut front = 2u8;

        let seconds = 4.0f64;
        let total = (seconds * SR as f64) as usize;
        let mut phase = 0.0f64;
        let mut at = 0usize;
        let mut ids = Vec::new();
        let mut freqs = Vec::new();
        while at < total {
            for _ in 0..detector.hop {
                let f = 2000.0 * 2f64.powf(at as f64 / SR as f64 / seconds);
                phase += 2.0 * std::f64::consts::PI * f / SR as f64;
                let x = phase.sin() as f32 * 0.5;
                shared.ring.push(x, x);
                at += 1;
            }
            detector.analyze(&shared);
            let frame = latest_frame(&shared, &mut front);
            // Skip the settling half second.
            if at > (SR * 0.5) as usize && frame.count >= 1 {
                ids.push(frame.targets[0].track);
                freqs.push(frame.targets[0].freq);
            }
        }

        assert!(ids.len() > 400, "the sweep was barely tracked");
        let first = ids[0];
        let churned = ids.iter().filter(|id| **id != first).count();
        assert_eq!(
            churned, 0,
            "the sweep churned identities: {} of {} frames on other tracks",
            churned,
            ids.len()
        );
        // And the track actually followed it up the spectrum.
        assert!(freqs.first().unwrap() < &2600.0);
        assert!(
            freqs.last().unwrap() > &3700.0,
            "tracking stalled at {} Hz",
            freqs.last().unwrap()
        );
        let regressions = freqs
            .windows(2)
            .filter(|w| w[1] < w[0] - 60.0)
            .count();
        assert!(
            regressions == 0,
            "the tracked frequency jumped backwards {regressions} times"
        );
    }

    /// A resonance that vanishes for 20 ms and returns keeps its identity and
    /// enough confidence that the pool would never have released it.
    #[test]
    fn a_brief_dropout_survives_with_the_same_track() {
        let shared = SharedSpectral::default();
        shared.cfg.publish(&spectral_cfg());
        let mut detector = Detector::new(SR);
        let mut front = 2u8;

        let mut at = 0usize;
        let mut feed = |detector: &mut Detector, samples: usize, on: bool| {
            let mut done = 0;
            while done < samples {
                for _ in 0..detector.hop {
                    let x = if on { sine64(1500.0, at) * 0.5 } else { 0.0 };
                    shared.ring.push(x, x);
                    at += 1;
                }
                done += detector.hop;
                detector.analyze(&shared);
            }
        };

        feed(&mut detector, (SR * 0.4) as usize, true);
        let before = latest_frame(&shared, &mut front);
        assert_eq!(before.count, 1);
        let id = before.targets[0].track;
        assert!(before.targets[0].confidence > 0.9);

        // 20 ms of silence — three or four analysis frames.
        feed(&mut detector, (SR * 0.02) as usize, false);
        let during = latest_frame(&shared, &mut front);
        assert_eq!(during.count, 1, "the dropout unpublished the track");
        assert_eq!(during.targets[0].track, id, "the dropout minted a new id");
        assert!(
            during.targets[0].confidence > 0.4,
            "confidence collapsed to {} across 20 ms",
            during.targets[0].confidence
        );

        feed(&mut detector, (SR * 0.2) as usize, true);
        let after = latest_frame(&shared, &mut front);
        assert_eq!(after.count, 1);
        assert_eq!(after.targets[0].track, id, "the return retriggered a new track");
        assert!(after.targets[0].confidence > 0.9);
    }

    /// Two resonances a minor third apart must settle into a stable set of
    /// identities — one filter must not alternate between them every frame.
    #[test]
    fn close_peaks_do_not_swap_every_frame() {
        let shared = SharedSpectral::default();
        shared.cfg.publish(&spectral_cfg());
        let mut detector = Detector::new(SR);
        let mut front = 2u8;

        let mut at = 0usize;
        let mut seen: Vec<(u32, f32)> = Vec::new();
        let passes = (SR * 1.5) as usize / detector.hop;
        for pass in 0..passes {
            for _ in 0..detector.hop {
                let x = sine64(3000.0, at) * 0.4 + sine64(3300.0, at) * 0.4;
                shared.ring.push(x, x);
                at += 1;
            }
            detector.analyze(&shared);
            if pass > passes / 2 {
                let frame = latest_frame(&shared, &mut front);
                for t in frame.targets[..frame.count as usize].iter() {
                    seen.push((t.track, t.freq));
                }
            }
        }

        let mut ids: Vec<u32> = seen.iter().map(|(id, _)| *id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert!(
            (1..=2).contains(&ids.len()),
            "two close peaks produced {} identities over the steady half",
            ids.len()
        );
        // Each identity stays put: its reported frequency never wanders far.
        for id in ids {
            let fs: Vec<f32> = seen
                .iter()
                .filter(|(i, _)| *i == id)
                .map(|(_, f)| *f)
                .collect();
            let lo = fs.iter().cloned().fold(f32::INFINITY, f32::min);
            let hi = fs.iter().cloned().fold(0.0f32, f32::max);
            assert!(
                (hi / lo).log2() < 0.1,
                "track {id} wandered {lo:.0}..{hi:.0} Hz"
            );
        }
    }

    /// The pool's Schmitt gate and hold, exercised directly: confidence
    /// wobbling between the thresholds neither engages nor releases anything,
    /// and a short collapse rides through on hold.
    #[test]
    fn hysteresis_and_hold_prevent_chatter() {
        let shared = SharedSpectral::default();
        let mut pool = AdaptivePool::new(SR);
        let tuning = PoolTuning {
            on_conf: 0.70,
            off_conf: 0.40,
            rearm_conf: 0.55,
            hold_s: 0.050,
            cooldown_s: 0.020,
            min_attack_ms: 2.0,
            min_release_ms: 20.0,
            max_cut_db: 12.0,
            q_min: 1.0,
            q_max: 16.0,
            slew_oct_s: 8.0,
        };
        let ctl = PoolControls {
            global_on: true,
            amount: 1.0,
            range_db: 12.0,
            attack_ms: 2.0,
            release_ms: 20.0,
            max_slots: 8,
            tuning,
            ..PoolControls::default()
        };

        let mut back = 0u8;
        let mut serial = 0u32;
        let mut publish = |conf: f32| {
            serial += 1;
            let mut frame = TargetFrame {
                serial,
                count: 1,
                ..TargetFrame::default()
            };
            frame.targets[0] = WireTarget {
                track: 5,
                freq: 1000.0,
                q: 8.0,
                excess_db: 8.0,
                confidence: conf,
                owner: -1,
                channel: 0,
            };
            shared.frames.publish(&mut back, frame);
        };
        let dt = 32.0 / SR;
        let mut run = |pool: &mut AdaptivePool, blocks: usize| {
            for _ in 0..blocks {
                let mut l = [0.0f32; 32];
                pool.update(&shared.frames, &ctl, dt);
                pool.process(&mut l, None, 32);
            }
        };

        // Below the on-threshold — even just below — nothing engages.
        publish(0.65);
        run(&mut pool, 300);
        assert!(
            pool.peak_cut() < 0.01,
            "engaged below the on-threshold: {}",
            pool.peak_cut()
        );

        // Strong confidence engages.
        publish(0.9);
        run(&mut pool, 300);
        let engaged = pool.peak_cut();
        assert!((engaged - 8.0).abs() < 0.5, "cut settled at {engaged}");

        // Falling into the hysteresis window changes nothing.
        publish(0.5);
        run(&mut pool, 300);
        assert!(
            (pool.peak_cut() - engaged).abs() < 0.1,
            "the cut moved inside the hysteresis window: {}",
            pool.peak_cut()
        );

        // A collapse shorter than the hold keeps the cut frozen...
        publish(0.1);
        run(&mut pool, 45); // 30 ms < 50 ms hold
        assert!(
            pool.peak_cut() > engaged - 0.5,
            "hold failed: cut fell to {}",
            pool.peak_cut()
        );
        // ...and recovery above the rearm threshold resumes seamlessly.
        publish(0.6);
        run(&mut pool, 300);
        assert!((pool.peak_cut() - engaged).abs() < 0.5);

        // A real disappearance — the worker stops publishing the track —
        // releases and eventually frees the slot.
        serial += 1;
        shared.frames.publish(
            &mut back,
            TargetFrame {
                serial,
                count: 0,
                ..TargetFrame::default()
            },
        );
        run(&mut pool, 1500);
        assert!(pool.peak_cut() < 0.05, "never released: {}", pool.peak_cut());
        assert!(!pool.busy(), "the slot never freed");
    }

    // --- SIMD backends ----------------------------------------------------

    /// Every backend this machine can run must agree with scalar to within
    /// rounding — same windows, same powers, same decibels, same prominence.
    #[test]
    fn simd_backends_match_scalar() {
        let n = 1037; // deliberately not a multiple of 8, to cover remainders
        let mut noise = Noise(0xBEEF);
        let src: Vec<f32> = (0..n * 2).map(|_| noise.next()).collect();
        let win: Vec<f32> = (0..n * 2).map(|_| noise.next().abs()).collect();
        let spec_l: Vec<realfft::num_complex::Complex32> = (0..n)
            .map(|_| realfft::num_complex::Complex32::new(noise.next() * 3.0, noise.next() * 3.0))
            .collect();
        let spec_r: Vec<realfft::num_complex::Complex32> = (0..n)
            .map(|_| realfft::num_complex::Complex32::new(noise.next() * 3.0, noise.next() * 3.0))
            .collect();
        let power: Vec<f32> = (0..n).map(|_| noise.next().abs() * 1e-3 + 1e-9).collect();
        let fresh: Vec<f32> = (0..n).map(|_| noise.next() * 40.0 - 60.0).collect();

        let scalar = scalar::SCALAR;
        for backend in dispatch::available() {
            if backend.name == scalar.name {
                continue;
            }

            let mut a = vec![0.0f32; n * 2];
            let mut b = vec![0.0f32; n * 2];
            (scalar.window)(&mut a, &src, &win);
            (backend.window)(&mut b, &src, &win);
            for i in 0..n * 2 {
                assert!(
                    (a[i] - b[i]).abs() <= 1e-6,
                    "{}: window diverged at {i}",
                    backend.name
                );
            }

            for mode in [
                PowerMode::Stereo,
                PowerMode::Mid,
                PowerMode::Side,
                PowerMode::Left,
                PowerMode::Right,
            ] {
                let mut a = vec![0.0f32; n];
                let mut b = vec![0.0f32; n];
                (scalar.power)(&spec_l, &spec_r, mode, &mut a);
                (backend.power)(&spec_l, &spec_r, mode, &mut b);
                for i in 0..n {
                    let tol = 1e-5 * a[i].abs().max(1e-6);
                    assert!(
                        (a[i] - b[i]).abs() <= tol,
                        "{}: power {mode:?} diverged at {i}: {} vs {}",
                        backend.name,
                        a[i],
                        b[i]
                    );
                }
            }

            let mut a = vec![0.0f32; n];
            let mut b = vec![0.0f32; n];
            (scalar.power_db)(&power, -12.3, &mut a);
            (backend.power_db)(&power, -12.3, &mut b);
            for i in 0..n {
                assert!(
                    (a[i] - b[i]).abs() <= 0.01,
                    "{}: dB diverged at {i}: {} vs {}",
                    backend.name,
                    a[i],
                    b[i]
                );
            }

            let mut a = fresh.clone();
            let mut b = fresh.clone();
            (scalar.smooth)(&mut a, &power, 0.3, 0.1);
            (backend.smooth)(&mut b, &power, 0.3, 0.1);
            for i in 0..n {
                assert!(
                    (a[i] - b[i]).abs() <= 1e-5,
                    "{}: smooth diverged at {i}",
                    backend.name
                );
            }

            let mut a = vec![0.0f32; n];
            let mut b = vec![0.0f32; n];
            (scalar.subtract)(&fresh, &power, &mut a);
            (backend.subtract)(&fresh, &power, &mut b);
            for i in 0..n {
                assert!(
                    (a[i] - b[i]).abs() <= 1e-6,
                    "{}: subtract diverged at {i}",
                    backend.name
                );
            }
        }
    }

    /// The decisions, not just the arithmetic: a full detector run on each
    /// backend must publish the same targets at the same frequencies.
    #[test]
    fn backends_make_the_same_detections() {
        let mut reference: Option<Vec<(f32, f32)>> = None;
        for backend in dispatch::available() {
            let shared = SharedSpectral::default();
            shared.cfg.publish(&spectral_cfg());
            let mut detector = Detector::with_kernels(SR, backend);
            let mut front = 2u8;

            let mut noise = Noise(7);
            run_detector(&shared, &mut detector, 80, move |i| {
                sine64(700.0, i) * 0.4 + sine64(5200.0, i) * 0.4 + noise.next() * 0.02
            });
            let frame = latest_frame(&shared, &mut front);
            let mut got: Vec<(f32, f32)> = frame.targets[..frame.count as usize]
                .iter()
                .map(|t| (t.freq, t.confidence))
                .collect();
            got.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

            match &reference {
                None => reference = Some(got),
                Some(want) => {
                    assert_eq!(got.len(), want.len(), "{}: target count", backend.name);
                    for (g, w) in got.iter().zip(want.iter()) {
                        assert!(
                            (g.0 - w.0).abs() < 2.0,
                            "{}: frequency {} vs {}",
                            backend.name,
                            g.0,
                            w.0
                        );
                        assert!(
                            (g.1 - w.1).abs() < 0.05,
                            "{}: confidence {} vs {}",
                            backend.name,
                            g.1,
                            w.1
                        );
                    }
                }
            }
        }
    }

    /// Per-backend analysis-pass timing, worst case included — realtime work
    /// cares about spikes, not averages.
    /// `cargo test --release --lib detector_backend_bench -- --ignored --nocapture`
    #[test]
    #[ignore = "prints timings; run explicitly in release"]
    fn detector_backend_bench() {
        for backend in dispatch::available() {
            let shared = SharedSpectral::default();
            shared.cfg.publish(&spectral_cfg());
            let mut detector = Detector::with_kernels(SR, backend);

            // A busy spectrum: tones and noise, ring pre-filled.
            let mut noise = Noise(99);
            for i in 0..detector.hop * 16 {
                let x = sine64(700.0, i) * 0.3
                    + sine64(3130.0, i) * 0.2
                    + sine64(9200.0, i) * 0.15
                    + noise.next() * 0.05;
                shared.ring.push(x, x);
            }

            let passes = 3000;
            let mut times = Vec::with_capacity(passes);
            for _ in 0..passes {
                let start = std::time::Instant::now();
                detector.analyze(&shared);
                times.push(start.elapsed().as_secs_f64() * 1e6);
            }
            times.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let avg: f64 = times.iter().sum::<f64>() / passes as f64;
            let p = |q: f64| times[((passes as f64 * q) as usize).min(passes - 1)];
            println!(
                "{:>9}: avg {avg:7.1} µs · p95 {:7.1} · p99 {:7.1} · max {:7.1}",
                backend.name,
                p(0.95),
                p(0.99),
                times[passes - 1]
            );
        }

        // And the kernels on their own, to show where the SIMD actually
        // lands — the full pass is dominated by the FFT (SIMD inside
        // rustfft already) and the sequential stages.
        let bins = 2049usize;
        let mut noise = Noise(3);
        let power: Vec<f32> = (0..bins).map(|_| noise.next().abs() * 1e-3 + 1e-9).collect();
        let spec: Vec<realfft::num_complex::Complex32> = (0..bins)
            .map(|_| realfft::num_complex::Complex32::new(noise.next(), noise.next()))
            .collect();
        let src: Vec<f32> = (0..4096).map(|_| noise.next()).collect();
        let win: Vec<f32> = (0..4096).map(|_| noise.next().abs()).collect();
        let mut out = vec![0.0f32; 4096];
        let iters = 20_000;
        for backend in dispatch::available() {
            let t0 = std::time::Instant::now();
            for _ in 0..iters {
                (backend.window)(&mut out, &src, &win);
            }
            let t_win = t0.elapsed().as_secs_f64() / iters as f64 * 1e6;
            let t0 = std::time::Instant::now();
            for _ in 0..iters {
                (backend.power)(&spec, &spec, PowerMode::Stereo, &mut out[..bins]);
            }
            let t_pow = t0.elapsed().as_secs_f64() / iters as f64 * 1e6;
            let t0 = std::time::Instant::now();
            for _ in 0..iters {
                (backend.power_db)(&power, -6.0, &mut out[..bins]);
            }
            let t_db = t0.elapsed().as_secs_f64() / iters as f64 * 1e6;
            println!(
                "{:>9} kernels: window {t_win:5.2} µs · power {t_pow:5.2} µs · dB {t_db:5.2} µs",
                backend.name
            );
        }
    }
}
