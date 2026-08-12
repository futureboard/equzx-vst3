//! EQUZX — a 24-band dynamic mid/side parametric EQ, by Futureboard Digital
//! Technologies.
//!
//! The audio path lives in [`dsp`], the automatable surface in [`params`], and
//! the web UI bridge in [`editor`]. `process` does very little itself: it walks
//! the buffer in control blocks, hands each one to [`dsp::engine::EqEngine`],
//! and taps the signal either side of the EQ for the analyser.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use nih_plug::prelude::*;

pub mod analyzer;
pub mod dsp;
pub mod gui;
pub mod meters;
pub mod params;
pub mod version;

use crate::analyzer::Taps;
use crate::dsp::engine::{settings_for_block, EqEngine, CONTROL_BLOCK};
use crate::dsp::resonance::RES_BANDS;
use crate::dsp::spectral::{SpectralWorker, TargetView, MAX_TARGETS};
use crate::meters::Meters;
use crate::params::{EquzxParams, TransientState, MAX_BANDS};

pub struct Equzx {
    params: Arc<EquzxParams>,
    transient: Arc<TransientState>,
    taps: Arc<Taps>,
    meters: Arc<Meters>,
    /// Published for the editor, which needs the rate to map bins to frequencies.
    sample_rate: Arc<AtomicF32>,
    engine: EqEngine,
    /// The spectral analysis thread. Spawned on `initialize` — never from the
    /// audio callback — and joined when the plugin is dropped. It idles at a
    /// few wakeups a second while no spectral mode is armed.
    spectral_worker: Option<SpectralWorker>,
    /// Somewhere to read the resonance curve into on the way to the meters,
    /// owned here so `process` never allocates.
    resonance_curve: [f32; RES_BANDS],
    /// Same for the spectral target views.
    target_views: [TargetView; MAX_TARGETS],
}

impl Default for Equzx {
    fn default() -> Self {
        Self {
            params: Arc::new(EquzxParams::default()),
            transient: Arc::new(TransientState::default()),
            taps: Arc::new(Taps::default()),
            meters: Arc::new(Meters::default()),
            sample_rate: Arc::new(AtomicF32::new(48_000.0)),
            engine: EqEngine::new(48_000.0),
            spectral_worker: None,
            resonance_curve: [0.0; RES_BANDS],
            target_views: [TargetView::default(); MAX_TARGETS],
        }
    }
}

impl Plugin for Equzx {
    const NAME: &'static str = "EQUZX";
    const VENDOR: &'static str = "Futureboard Digital Technologies";
    const URL: &'static str = "https://futureboard.digital";
    const EMAIL: &'static str = "hello@futureboard.digital";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(2),
            main_output_channels: NonZeroU32::new(2),
            aux_input_ports: &[],
            aux_output_ports: &[],
            names: PortNames::const_default(),
        },
        // Mono is a real use for an EQ; the side bus simply never runs.
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(1),
            main_output_channels: NonZeroU32::new(1),
            ..AudioIOLayout::const_default()
        },
    ];

    const MIDI_INPUT: MidiConfig = MidiConfig::None;
    const MIDI_OUTPUT: MidiConfig = MidiConfig::None;
    const SAMPLE_ACCURATE_AUTOMATION: bool = true;

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn editor(&mut self, _executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        gui::create(gui::EditorContext {
            params: self.params.clone(),
            transient: self.transient.clone(),
            taps: self.taps.clone(),
            meters: self.meters.clone(),
            sample_rate: self.sample_rate.clone(),
        })
    }

    fn initialize(
        &mut self,
        _layout: &AudioIOLayout,
        config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        self.sample_rate
            .store(config.sample_rate, Ordering::Relaxed);
        self.engine.set_sample_rate(config.sample_rate);
        if self.spectral_worker.is_none() {
            self.spectral_worker = Some(SpectralWorker::spawn(self.engine.spectral_shared()));
        }
        true
    }

    fn reset(&mut self) {
        self.engine.reset();
        self.taps.clear();
        self.meters.clear();
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        _context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        let num_samples = buffer.samples();
        if num_samples == 0 {
            return ProcessStatus::Normal;
        }

        // A whole-state recall moves every band at once; flushing avoids a chorus
        // of filters ringing out on settings that no longer exist.
        if self.transient.flush.swap(false, Ordering::Relaxed) {
            self.engine.reset();
        }

        let sr = self.engine.sample_rate();
        let channels = buffer.channels();
        let slices = buffer.as_slice();

        let mut offset = 0;
        while offset < num_samples {
            let n = CONTROL_BLOCK.min(num_samples - offset);
            let settings = settings_for_block(&self.params, &self.transient, n, sr);

            let (first, rest) = slices.split_at_mut(1);
            let left = &mut first[0][offset..offset + n];
            let right = if channels >= 2 {
                Some(&mut rest[0][offset..offset + n])
            } else {
                None
            };

            // Tap the input before the EQ sees it. The analyser is mono, and the
            // mono sum of a stereo pair is exactly the mid bus.
            match &right {
                Some(r) => {
                    for i in 0..n {
                        self.taps.pre.push(0.5 * (left[i] + r[i]));
                    }
                }
                None => {
                    for &x in left.iter() {
                        self.taps.pre.push(x);
                    }
                }
            }

            self.engine.process_block(left, right, &settings);

            match slices.get(1) {
                Some(r) if channels >= 2 => {
                    for i in 0..n {
                        self.taps
                            .post
                            .push(0.5 * (slices[0][offset + i] + r[offset + i]));
                    }
                }
                _ => {
                    for i in 0..n {
                        self.taps.post.push(slices[0][offset + i]);
                    }
                }
            }

            offset += n;
        }

        for slot in 0..MAX_BANDS {
            self.meters.publish(slot, self.engine.meter(slot));
        }
        self.engine.resonance_reduction(&mut self.resonance_curve);
        self.meters
            .publish_resonance(&self.resonance_curve, self.engine.resonance_peak());
        self.engine.spectral_view(&mut self.target_views);
        self.meters.publish_targets(&self.target_views);

        ProcessStatus::Normal
    }
}

impl ClapPlugin for Equzx {
    const CLAP_ID: &'static str = "digital.futureboard.equzx";
    const CLAP_DESCRIPTION: Option<&'static str> =
        Some("24-band dynamic mid/side parametric EQ with a spectrum analyser");
    const CLAP_MANUAL_URL: Option<&'static str> = Some(Self::URL);
    const CLAP_SUPPORT_URL: Option<&'static str> = Some(Self::URL);
    const CLAP_FEATURES: &'static [ClapFeature] = &[
        ClapFeature::AudioEffect,
        ClapFeature::Stereo,
        ClapFeature::Mono,
        ClapFeature::Equalizer,
        ClapFeature::Mastering,
    ];
}

impl Vst3Plugin for Equzx {
    /// Sixteen bytes, fixed forever: changing it orphans every session that
    /// already loaded the plugin.
    const VST3_CLASS_ID: [u8; 16] = *b"EQUZXFutureboard";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[
        Vst3SubCategory::Fx,
        Vst3SubCategory::Eq,
        Vst3SubCategory::Stereo,
    ];
}

nih_export_clap!(Equzx);
nih_export_vst3!(Equzx);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::{Analyzer, FLOOR_DB, F_MIN, LOG_POINTS};
    use std::f32::consts::PI;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 512;

    /// Run a stereo sine through the plugin the way a host would.
    fn run_sine(plugin: &mut Equzx, freq: f32, blocks: usize) {
        let mut left = vec![0.0f32; BLOCK];
        let mut right = vec![0.0f32; BLOCK];

        for block in 0..blocks {
            for i in 0..BLOCK {
                let t = (block * BLOCK + i) as f32 / SR;
                let x = (2.0 * PI * freq * t).sin();
                left[i] = x;
                right[i] = x;
            }

            let mut slices = [left.as_mut_slice(), right.as_mut_slice()];
            let mut buffer = Buffer::default();
            // Safety: the slices outlive the buffer, which is dropped at the end
            // of this iteration, and nothing else aliases them meanwhile.
            unsafe {
                buffer.set_slices(BLOCK, |output| {
                    *output = slices
                        .iter_mut()
                        .map(|slice| std::mem::transmute::<&mut [f32], &mut [f32]>(*slice))
                        .collect();
                });
            }

            let mut aux = AuxiliaryBuffers {
                inputs: &mut [],
                outputs: &mut [],
            };
            let mut context = DummyContext;
            plugin.process(&mut buffer, &mut aux, &mut context);
        }
    }

    /// The minimum a `ProcessContext` has to be for `process` to run.
    struct DummyContext;

    impl ProcessContext<Equzx> for DummyContext {
        fn plugin_api(&self) -> PluginApi {
            PluginApi::Clap
        }
        fn execute_background(&self, _task: ()) {}
        fn execute_gui(&self, _task: ()) {}
        fn transport(&self) -> &Transport {
            // `process` never looks at the transport, and nih-plug keeps the
            // constructor private, so this is only ever a shape to satisfy the
            // trait — hence the panic rather than a fabricated value.
            unimplemented!("the EQ does not read the transport")
        }
        fn next_event(&mut self) -> Option<NoteEvent<()>> {
            None
        }
        fn send_event(&mut self, _event: NoteEvent<()>) {}
        fn set_latency_samples(&self, _samples: u32) {}
        fn set_current_voice_capacity(&self, _capacity: u32) {}
    }

    /// Index of the log-spaced point nearest a frequency.
    fn point_for(freq: f32) -> usize {
        let f_max = crate::analyzer::F_MAX.min(SR / 2.0 - 1.0);
        let t = (freq / F_MIN).ln() / (f_max / F_MIN).ln();
        (t * (LOG_POINTS - 1) as f32).round() as usize
    }

    #[test]
    fn a_default_plugin_passes_audio_through_untouched() {
        let mut plugin = Equzx::default();
        plugin.engine.set_sample_rate(SR);

        let mut left = vec![0.0f32; BLOCK];
        let mut right = vec![0.0f32; BLOCK];
        for i in 0..BLOCK {
            let x = (2.0 * PI * 1000.0 * i as f32 / SR).sin();
            left[i] = x;
            right[i] = x * 0.5;
        }
        let expected_left = left.clone();
        let expected_right = right.clone();

        let mut slices = [left.as_mut_slice(), right.as_mut_slice()];
        let mut buffer = Buffer::default();
        unsafe {
            buffer.set_slices(BLOCK, |output| {
                *output = slices.iter_mut().map(|s| &mut **s).collect();
            });
        }
        let mut aux = AuxiliaryBuffers {
            inputs: &mut [],
            outputs: &mut [],
        };
        plugin.process(&mut buffer, &mut aux, &mut DummyContext);

        for i in 0..BLOCK {
            assert!(
                (left[i] - expected_left[i]).abs() < 1e-5,
                "L changed at {i}"
            );
            assert!(
                (right[i] - expected_right[i]).abs() < 1e-5,
                "R changed at {i}"
            );
        }
    }

    #[test]
    fn the_analyser_sees_what_the_host_played() {
        let mut plugin = Equzx::default();
        plugin.engine.set_sample_rate(SR);
        run_sine(&mut plugin, 1000.0, 40);

        let mut analyzer = Analyzer::new(SR);
        // Frames are smoothed against each other, so let the curve settle.
        for _ in 0..40 {
            analyzer.analyze(&plugin.taps);
        }

        let (pre_curve, post_curve) = analyzer.curves();
        let peak = point_for(1000.0);

        assert!(
            pre_curve[peak] > -12.0,
            "pre read {} dB at 1 kHz",
            pre_curve[peak]
        );
        // Nothing is EQing, so the two taps must agree.
        assert!(
            (pre_curve[peak] - post_curve[peak]).abs() < 1.0,
            "pre {} vs post {}",
            pre_curve[peak],
            post_curve[peak]
        );
        // And there is nothing two octaves down.
        assert!(pre_curve[point_for(250.0)] < -50.0);
    }

    #[test]
    fn a_silent_buffer_leaves_the_meters_parked() {
        let mut plugin = Equzx::default();
        plugin.engine.set_sample_rate(SR);
        run_sine(&mut plugin, 1000.0, 4);

        let mut level = vec![0.0; MAX_BANDS];
        let mut delta = vec![0.0; MAX_BANDS];
        plugin.meters.read_into(&mut level, &mut delta);
        // No band is active, so nothing is metering.
        assert!(level.iter().all(|v| *v == -100.0));
        assert!(delta.iter().all(|v| *v == 0.0));
    }

    #[test]
    fn reset_clears_the_analyser_history() {
        let mut plugin = Equzx::default();
        plugin.engine.set_sample_rate(SR);
        run_sine(&mut plugin, 1000.0, 20);
        plugin.reset();

        let mut analyzer = Analyzer::new(SR);
        for _ in 0..40 {
            analyzer.analyze(&plugin.taps);
        }
        assert!(
            analyzer.curves().0.iter().all(|db| *db <= FLOOR_DB),
            "history survived a reset"
        );
    }
}
