//! Plugin parameters.
//!
//! The UI presents an EQ you add and remove bands from, but a VST3/CLAP host
//! needs a parameter list that never changes shape. Both are satisfied by a
//! fixed array of [`MAX_BANDS`] slots: a slot carries an `active` flag, and
//! "adding a band" is really claiming the first inactive slot. A band's slot
//! index is its identity — it is the `id` the web UI works with — so automation
//! lanes stay attached to the band the user drew.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use nih_plug::prelude::*;
use nih_plug_egui::EguiState;

/// Matches `MAX_BANDS` in `crate::gui::state`.
pub const MAX_BANDS: usize = 24;

/// dB past the threshold at which a dynamic band reaches its full range.
/// Mirrors `DYN_KNEE_DB` on the UI side.
pub const DYN_KNEE_DB: f32 = 6.0;

#[derive(Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandKind {
    #[id = "lowcut"]
    #[name = "Low Cut"]
    LowCut,
    #[id = "lowshelf"]
    #[name = "Low Shelf"]
    LowShelf,
    #[id = "bell"]
    Bell,
    #[id = "notch"]
    Notch,
    #[id = "bandpass"]
    #[name = "Band Pass"]
    BandPass,
    #[id = "highshelf"]
    #[name = "High Shelf"]
    HighShelf,
    #[id = "highcut"]
    #[name = "High Cut"]
    HighCut,
}

impl BandKind {
    /// Cut types are Butterworth cascades; everything else is a single section.
    pub fn is_cut(self) -> bool {
        matches!(self, BandKind::LowCut | BandKind::HighCut)
    }

    /// Only these types have a gain to move, so only these can be dynamic.
    pub fn uses_gain(self) -> bool {
        matches!(
            self,
            BandKind::LowShelf | BandKind::Bell | BandKind::HighShelf
        )
    }

    /// The name the web UI uses for this type.
    pub fn as_wire(self) -> &'static str {
        match self {
            BandKind::LowCut => "lowcut",
            BandKind::LowShelf => "lowshelf",
            BandKind::Bell => "bell",
            BandKind::Notch => "notch",
            BandKind::BandPass => "bandpass",
            BandKind::HighShelf => "highshelf",
            BandKind::HighCut => "highcut",
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        Some(match s {
            "lowcut" => BandKind::LowCut,
            "lowshelf" => BandKind::LowShelf,
            "bell" => BandKind::Bell,
            "notch" => BandKind::Notch,
            "bandpass" => BandKind::BandPass,
            "highshelf" => BandKind::HighShelf,
            "highcut" => BandKind::HighCut,
            _ => return None,
        })
    }
}

/// Which part of the signal a band acts on.
///
/// Left/Right are appended rather than slotted in next to Stereo on purpose.
/// Sessions store an enum by its `id` string so the order doesn't affect recall,
/// but a VST3 host's automation lane is a normalized float over the variant
/// count — reordering would move every existing lane.
#[derive(Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandChannel {
    #[id = "stereo"]
    Stereo,
    #[id = "mid"]
    Mid,
    #[id = "side"]
    Side,
    #[id = "left"]
    Left,
    #[id = "right"]
    Right,
}

impl BandChannel {
    pub fn as_wire(self) -> &'static str {
        match self {
            BandChannel::Stereo => "stereo",
            BandChannel::Mid => "mid",
            BandChannel::Side => "side",
            BandChannel::Left => "left",
            BandChannel::Right => "right",
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        Some(match s {
            "stereo" => BandChannel::Stereo,
            "mid" => BandChannel::Mid,
            "side" => BandChannel::Side,
            "left" => BandChannel::Left,
            "right" => BandChannel::Right,
            _ => return None,
        })
    }

    /// The domain this band has to be filtered in. `None` for stereo, which is
    /// the same filter on both buses and so gives the same answer either way.
    pub fn domain(self) -> Option<Domain> {
        match self {
            BandChannel::Stereo => None,
            BandChannel::Mid | BandChannel::Side => Some(Domain::MidSide),
            BandChannel::Left | BandChannel::Right => Some(Domain::LeftRight),
        }
    }

    /// Does this band act on the first bus of its domain — left, or mid?
    pub fn uses_first_bus(self) -> bool {
        !matches!(self, BandChannel::Side | BandChannel::Right)
    }

    /// And on the second — right, or side?
    pub fn uses_second_bus(self) -> bool {
        !matches!(self, BandChannel::Mid | BandChannel::Left)
    }
}

/// Which pair of buses the signal is currently carried on.
///
/// Left/right and mid/side are two views of the same stereo signal, related by
/// an exactly invertible transform, so the EQ moves between them as it walks the
/// band list rather than committing to one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    LeftRight,
    MidSide,
}

/// Cut slopes, in dB/oct. Only even filter orders exist, hence multiples of 12.
#[derive(Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slope {
    #[id = "12"]
    #[name = "12 dB/oct"]
    S12,
    #[id = "24"]
    #[name = "24 dB/oct"]
    S24,
    #[id = "36"]
    #[name = "36 dB/oct"]
    S36,
    #[id = "48"]
    #[name = "48 dB/oct"]
    S48,
    #[id = "72"]
    #[name = "72 dB/oct"]
    S72,
    #[id = "96"]
    #[name = "96 dB/oct"]
    S96,
}

impl Slope {
    pub fn db_per_oct(self) -> u32 {
        match self {
            Slope::S12 => 12,
            Slope::S24 => 24,
            Slope::S36 => 36,
            Slope::S48 => 48,
            Slope::S72 => 72,
            Slope::S96 => 96,
        }
    }

    /// Filter order — the cascade holds half this many second-order sections.
    pub fn order(self) -> usize {
        (self.db_per_oct() / 6) as usize
    }

    pub fn from_db_per_oct(v: u32) -> Option<Self> {
        Some(match v {
            12 => Slope::S12,
            24 => Slope::S24,
            36 => Slope::S36,
            48 => Slope::S48,
            72 => Slope::S72,
            96 => Slope::S96,
            _ => return None,
        })
    }
}

/// What the global resonance stage runs as, once it is switched on.
///
/// Adaptive is the sixth-octave filter bank in [`crate::dsp::resonance`];
/// Spectral is the FFT detector and adaptive filter pool in
/// [`crate::dsp::spectral`]. Both are time-domain in the audio path — the
/// choice is about how resonances are *found*, not whether latency is added.
#[derive(Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResMode {
    #[id = "adaptive"]
    Adaptive,
    #[id = "spectral"]
    Spectral,
}

impl ResMode {
    pub fn as_wire(self) -> &'static str {
        match self {
            ResMode::Adaptive => "adaptive",
            ResMode::Spectral => "spectral",
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        Some(match s {
            "adaptive" => ResMode::Adaptive,
            "spectral" => ResMode::Spectral,
            _ => return None,
        })
    }
}

/// How many adaptive filters the spectral stage may run at once.
///
/// Named for what the user is choosing — the leanest tier is the one meant for
/// tracking and monitoring chains, hence "Ultra". None of the tiers change the
/// audio path's latency, which is zero in all of them.
#[derive(Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResQuality {
    #[id = "ultra"]
    Ultra,
    #[id = "balanced"]
    Balanced,
    #[id = "high"]
    High,
}

impl ResQuality {
    /// Simultaneous adaptive targets this tier allows.
    pub fn max_targets(self) -> usize {
        match self {
            ResQuality::Ultra => 8,
            ResQuality::Balanced => 16,
            ResQuality::High => 24,
        }
    }

    pub fn as_wire(self) -> &'static str {
        match self {
            ResQuality::Ultra => "ultra",
            ResQuality::Balanced => "balanced",
            ResQuality::High => "high",
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        Some(match s {
            "ultra" => ResQuality::Ultra,
            "balanced" => ResQuality::Balanced,
            "high" => ResQuality::High,
            _ => return None,
        })
    }
}

/// How a band's own resonance amount finds what it suppresses.
///
/// The default is Adaptive rather than Off on purpose: the amount itself
/// defaults to zero, so a fresh band is still inert, and every session saved
/// before the mode existed had exactly this behaviour behind its amount.
#[derive(Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandResMode {
    #[id = "off"]
    Off,
    #[id = "adaptive"]
    Adaptive,
    #[id = "spectral"]
    Spectral,
}

impl BandResMode {
    pub fn as_wire(self) -> &'static str {
        match self {
            BandResMode::Off => "off",
            BandResMode::Adaptive => "adaptive",
            BandResMode::Spectral => "spectral",
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        Some(match s {
            "off" => BandResMode::Off,
            "adaptive" => BandResMode::Adaptive,
            "spectral" => BandResMode::Spectral,
            _ => return None,
        })
    }
}

#[derive(Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DynMode {
    /// Engages once the band's level rises past the threshold.
    #[id = "above"]
    Above,
    /// Engages once it falls below it.
    #[id = "below"]
    Below,
}

impl DynMode {
    pub fn as_wire(self) -> &'static str {
        match self {
            DynMode::Above => "above",
            DynMode::Below => "below",
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        Some(match s {
            "above" => DynMode::Above,
            "below" => DynMode::Below,
            _ => return None,
        })
    }
}

/// One band slot. Ranges match `BandSnapshot::sanitized` in `crate::gui::state`,
/// so a preset written by the UI always round-trips through the parameters.
#[derive(Params)]
pub struct BandParams {
    /// Is this slot in use at all? Distinct from `enabled`, which is the user's
    /// per-band on/off switch — an inactive slot isn't drawn or listed.
    #[id = "act"]
    pub active: BoolParam,
    #[id = "en"]
    pub enabled: BoolParam,
    #[id = "type"]
    pub kind: EnumParam<BandKind>,
    #[id = "ch"]
    pub channel: EnumParam<BandChannel>,
    #[id = "f"]
    pub freq: FloatParam,
    #[id = "g"]
    pub gain: FloatParam,
    #[id = "q"]
    pub q: FloatParam,
    #[id = "sl"]
    pub slope: EnumParam<Slope>,

    #[id = "dyn"]
    pub dynamic: BoolParam,
    #[id = "dm"]
    pub dyn_mode: EnumParam<DynMode>,
    #[id = "dr"]
    pub dyn_range: FloatParam,
    #[id = "th"]
    pub threshold: FloatParam,
    #[id = "at"]
    pub attack: FloatParam,
    #[id = "rl"]
    pub release: FloatParam,

    /// Adaptive resonance suppression inside this band's own region, on top of
    /// whatever static curve the band is drawing. Zero is off.
    #[id = "res"]
    pub resonance: FloatParam,
    /// How that amount finds its resonances — the sixth-octave bank, or the
    /// spectral detector tracking a peak inside the band's search region.
    #[id = "rsm"]
    pub res_mode: EnumParam<BandResMode>,
    /// Ceiling on the cut this band may ask for, in dB.
    #[id = "rsr"]
    pub res_range: FloatParam,
    /// dB taken off the detection threshold inside this band's region —
    /// positive makes the band more eager than the global stage.
    #[id = "rss"]
    pub res_sens: FloatParam,
    /// Half-width of the spectral search region, in octaves either side of the
    /// band's frequency. The detected resonance may sit anywhere inside it.
    #[id = "rsw"]
    pub res_width: FloatParam,
    /// Ballistics for this band's resonance attenuation, distinct from the
    /// dynamics section's attack/release, which move the band's own gain.
    #[id = "rsa"]
    pub res_attack: FloatParam,
    #[id = "rsrl"]
    pub res_release: FloatParam,
}

impl Default for BandParams {
    fn default() -> Self {
        Self {
            active: BoolParam::new("Active", false),
            enabled: BoolParam::new("Enabled", true),
            kind: EnumParam::new("Type", BandKind::Bell),
            channel: EnumParam::new("Channel", BandChannel::Stereo),

            freq: FloatParam::new(
                "Frequency",
                1000.0,
                FloatRange::Skewed {
                    min: 20.0,
                    max: 22_000.0,
                    factor: FloatRange::skew_factor(-2.0),
                },
            )
            .with_smoother(SmoothingStyle::Logarithmic(20.0))
            .with_unit(" Hz")
            .with_value_to_string(formatters::v2s_f32_hz_then_khz(1))
            .with_string_to_value(formatters::s2v_f32_hz_then_khz()),

            gain: FloatParam::new(
                "Gain",
                0.0,
                FloatRange::Linear {
                    min: -30.0,
                    max: 30.0,
                },
            )
            .with_smoother(SmoothingStyle::Linear(20.0))
            .with_unit(" dB")
            .with_value_to_string(formatters::v2s_f32_rounded(2)),

            q: FloatParam::new(
                "Q",
                1.0,
                FloatRange::Skewed {
                    min: 0.025,
                    max: 40.0,
                    factor: FloatRange::skew_factor(-1.0),
                },
            )
            .with_smoother(SmoothingStyle::Logarithmic(20.0))
            .with_value_to_string(formatters::v2s_f32_rounded(3)),

            slope: EnumParam::new("Slope", Slope::S24),

            dynamic: BoolParam::new("Dynamic", false),
            dyn_mode: EnumParam::new("Dyn Mode", DynMode::Above),
            dyn_range: FloatParam::new(
                "Dyn Range",
                -6.0,
                FloatRange::Linear {
                    min: -30.0,
                    max: 30.0,
                },
            )
            .with_smoother(SmoothingStyle::Linear(20.0))
            .with_unit(" dB")
            .with_value_to_string(formatters::v2s_f32_rounded(2)),
            threshold: FloatParam::new(
                "Threshold",
                -24.0,
                FloatRange::Linear {
                    min: -70.0,
                    max: 0.0,
                },
            )
            .with_smoother(SmoothingStyle::Linear(20.0))
            .with_unit(" dB")
            .with_value_to_string(formatters::v2s_f32_rounded(1)),
            attack: FloatParam::new(
                "Attack",
                20.0,
                FloatRange::Skewed {
                    min: 1.0,
                    max: 300.0,
                    factor: FloatRange::skew_factor(-1.5),
                },
            )
            .with_unit(" ms")
            .with_value_to_string(formatters::v2s_f32_rounded(1)),
            release: FloatParam::new(
                "Release",
                200.0,
                FloatRange::Skewed {
                    min: 10.0,
                    max: 2000.0,
                    factor: FloatRange::skew_factor(-1.5),
                },
            )
            .with_unit(" ms")
            .with_value_to_string(formatters::v2s_f32_rounded(0)),

            // Off by default: a band the user drew is a band they want, not a
            // starting point for something else to chew on.
            resonance: FloatParam::new("Resonance", 0.0, FloatRange::Linear { min: 0.0, max: 1.0 })
                .with_smoother(SmoothingStyle::Linear(30.0))
                .with_unit("%")
                .with_value_to_string(formatters::v2s_f32_percentage(0))
                .with_string_to_value(formatters::s2v_f32_percentage()),

            res_mode: EnumParam::new("Res Mode", BandResMode::Adaptive),

            // Defaults to the bank's own hard ceiling, so a session from before
            // the control existed keeps its exact behaviour.
            res_range: FloatParam::new(
                "Res Range",
                36.0,
                FloatRange::Linear { min: 0.0, max: 36.0 },
            )
            .with_smoother(SmoothingStyle::Linear(30.0))
            .with_unit(" dB")
            .with_value_to_string(formatters::v2s_f32_rounded(1)),

            res_sens: FloatParam::new(
                "Res Sensitivity",
                0.0,
                FloatRange::Linear {
                    min: -12.0,
                    max: 12.0,
                },
            )
            .with_smoother(SmoothingStyle::Linear(30.0))
            .with_unit(" dB")
            .with_value_to_string(formatters::v2s_f32_rounded(1)),

            res_width: FloatParam::new(
                "Res Width",
                1.0,
                FloatRange::Skewed {
                    min: 0.25,
                    max: 2.0,
                    factor: FloatRange::skew_factor(-0.5),
                },
            )
            .with_smoother(SmoothingStyle::Linear(30.0))
            .with_unit(" oct")
            .with_value_to_string(formatters::v2s_f32_rounded(2)),

            res_attack: FloatParam::new(
                "Res Attack",
                5.0,
                FloatRange::Skewed {
                    min: 0.5,
                    max: 100.0,
                    factor: FloatRange::skew_factor(-1.5),
                },
            )
            .with_unit(" ms")
            .with_value_to_string(formatters::v2s_f32_rounded(1)),

            res_release: FloatParam::new(
                "Res Release",
                40.0,
                FloatRange::Skewed {
                    min: 5.0,
                    max: 1000.0,
                    factor: FloatRange::skew_factor(-1.5),
                },
            )
            .with_unit(" ms")
            .with_value_to_string(formatters::v2s_f32_rounded(0)),
        }
    }
}

/// Adaptive resonance suppression — see [`crate::dsp::resonance`] for what the
/// stage does with these.
///
/// Ranges are the plain units the UI works in; the engine converts the
/// percentages to the 0..1 ratios the bank wants.
#[derive(Params)]
pub struct ResonanceParams {
    #[id = "rson"]
    pub enabled: BoolParam,
    /// Which engine the switch arms — see [`ResMode`]. Kept apart from
    /// `enabled` so the automation lane hosts already wrote for the switch
    /// keeps meaning on/off.
    #[id = "rsmod"]
    pub mode: EnumParam<ResMode>,
    /// Adaptive filter budget for the spectral engine — see [`ResQuality`].
    #[id = "rsqua"]
    pub quality: EnumParam<ResQuality>,
    /// Ceiling on any single cut the stage makes, in dB.
    #[id = "rsrng"]
    pub range: FloatParam,
    #[id = "rsdep"]
    pub depth: FloatParam,
    #[id = "rssh"]
    pub sharpness: FloatParam,
    #[id = "rsthr"]
    pub threshold: FloatParam,
    #[id = "rsatk"]
    pub attack: FloatParam,
    #[id = "rsrel"]
    pub release: FloatParam,
    #[id = "rslo"]
    pub low: FloatParam,
    #[id = "rshi"]
    pub high: FloatParam,
    #[id = "rsmix"]
    pub mix: FloatParam,
    #[id = "rsdlt"]
    pub delta: BoolParam,
}

impl Default for ResonanceParams {
    fn default() -> Self {
        Self {
            // Off by default: a session that predates the stage has to sound
            // exactly as it did, and a suppressor nobody asked for is the last
            // thing anyone wants finding resonances in their mix.
            enabled: BoolParam::new("Resonance", false),

            mode: EnumParam::new("Resonance Mode", ResMode::Adaptive),
            quality: EnumParam::new("Resonance Quality", ResQuality::Ultra),

            // The bank's historical hard ceiling, so pre-existing sessions
            // keep their exact behaviour with the control at its default.
            range: FloatParam::new(
                "Resonance Range",
                36.0,
                FloatRange::Linear { min: 0.0, max: 36.0 },
            )
            .with_smoother(SmoothingStyle::Linear(30.0))
            .with_unit(" dB")
            .with_value_to_string(formatters::v2s_f32_rounded(1)),

            depth: FloatParam::new("Resonance Depth", 0.5, FloatRange::Linear { min: 0.0, max: 1.0 })
                .with_smoother(SmoothingStyle::Linear(30.0))
                .with_unit("%")
                .with_value_to_string(formatters::v2s_f32_percentage(0))
                .with_string_to_value(formatters::s2v_f32_percentage()),

            sharpness: FloatParam::new(
                "Resonance Sharpness",
                0.5,
                FloatRange::Linear { min: 0.0, max: 1.0 },
            )
            .with_smoother(SmoothingStyle::Linear(30.0))
            .with_unit("%")
            .with_value_to_string(formatters::v2s_f32_percentage(0))
            .with_string_to_value(formatters::s2v_f32_percentage()),

            // Six dB clear of the local average. Lower starts shaving the
            // partials of anything tonal, which is a choice rather than a default.
            threshold: FloatParam::new(
                "Resonance Threshold",
                6.0,
                FloatRange::Linear {
                    min: -12.0,
                    max: 24.0,
                },
            )
            .with_smoother(SmoothingStyle::Linear(30.0))
            .with_unit(" dB")
            .with_value_to_string(formatters::v2s_f32_rounded(1)),

            attack: FloatParam::new(
                "Resonance Attack",
                5.0,
                FloatRange::Skewed {
                    min: 0.5,
                    max: 100.0,
                    factor: FloatRange::skew_factor(-1.5),
                },
            )
            .with_unit(" ms")
            .with_value_to_string(formatters::v2s_f32_rounded(1)),

            release: FloatParam::new(
                "Resonance Release",
                40.0,
                FloatRange::Skewed {
                    min: 5.0,
                    max: 1000.0,
                    factor: FloatRange::skew_factor(-1.5),
                },
            )
            .with_unit(" ms")
            .with_value_to_string(formatters::v2s_f32_rounded(0)),

            low: FloatParam::new(
                "Resonance Low",
                20.0,
                FloatRange::Skewed {
                    min: 20.0,
                    max: 2000.0,
                    factor: FloatRange::skew_factor(-2.0),
                },
            )
            .with_smoother(SmoothingStyle::Logarithmic(50.0))
            .with_unit(" Hz")
            .with_value_to_string(formatters::v2s_f32_hz_then_khz(1))
            .with_string_to_value(formatters::s2v_f32_hz_then_khz()),

            high: FloatParam::new(
                "Resonance High",
                20_000.0,
                FloatRange::Skewed {
                    min: 500.0,
                    max: 20_000.0,
                    factor: FloatRange::skew_factor(-2.0),
                },
            )
            .with_smoother(SmoothingStyle::Logarithmic(50.0))
            .with_unit(" Hz")
            .with_value_to_string(formatters::v2s_f32_hz_then_khz(1))
            .with_string_to_value(formatters::s2v_f32_hz_then_khz()),

            mix: FloatParam::new("Resonance Mix", 1.0, FloatRange::Linear { min: 0.0, max: 1.0 })
                .with_smoother(SmoothingStyle::Linear(30.0))
                .with_unit("%")
                .with_value_to_string(formatters::v2s_f32_percentage(0))
                .with_string_to_value(formatters::s2v_f32_percentage()),

            // Monitoring, not sound design — but it is a parameter so the host
            // can bind it to a key and so it survives a reopened editor.
            delta: BoolParam::new("Resonance Delta", false),
        }
    }
}

#[derive(Params)]
pub struct EquzxParams {
    #[id = "bypass"]
    pub bypass: BoolParam,
    #[id = "outgain"]
    pub output_gain: FloatParam,

    #[nested(array, group = "Band")]
    pub bands: [BandParams; MAX_BANDS],

    /// Appended after the bands so every parameter that existed before it keeps
    /// the automation index a host already wrote into its sessions.
    #[nested(group = "Resonance")]
    pub resonance: ResonanceParams,

    /// View state the DAW should remember but never automate: analyser mode,
    /// dB range, panel height, the parked A/B slot. Opaque JSON owned by the UI.
    #[persist = "ui"]
    pub ui_state: Arc<RwLock<String>>,

    /// Window size, so a reopened editor comes back the size it was left.
    ///
    /// Held by the egui adapter rather than written into `ui_state` with the
    /// rest: the editor has to know how large to open *before* any of its own
    /// state has been read.
    #[persist = "editor"]
    pub editor_state: Arc<EguiState>,
}

impl Default for EquzxParams {
    fn default() -> Self {
        Self {
            // Declared as a real bypass so hosts wire it to their own bypass button.
            bypass: BoolParam::new("Bypass", false)
                .with_value_to_string(formatters::v2s_bool_bypass())
                .with_string_to_value(formatters::s2v_bool_bypass())
                .make_bypass(),
            output_gain: FloatParam::new(
                "Output Gain",
                0.0,
                FloatRange::Linear {
                    min: -24.0,
                    max: 12.0,
                },
            )
            .with_smoother(SmoothingStyle::Linear(20.0))
            .with_unit(" dB")
            .with_value_to_string(formatters::v2s_f32_rounded(2)),

            bands: std::array::from_fn(|_| BandParams::default()),
            resonance: ResonanceParams::default(),
            ui_state: Arc::new(RwLock::new(String::new())),
            editor_state: EguiState::from_size(
                crate::gui::DEFAULT_WIDTH,
                crate::gui::DEFAULT_HEIGHT,
            ),
        }
    }
}

/// State the UI drives that isn't automatable and shouldn't be saved with the
/// session: which band is currently soloed.
///
/// Solo is a monitoring action, not part of the sound the user is designing, so
/// it lives outside the parameter list. `-1` means nothing is soloed.
pub struct TransientState {
    solo: std::sync::atomic::AtomicI32,
    /// Set by the editor when it wants the DSP to drop filter state — e.g. after
    /// loading a preset that moves every band at once.
    pub flush: AtomicBool,
}

impl Default for TransientState {
    fn default() -> Self {
        Self {
            solo: std::sync::atomic::AtomicI32::new(-1),
            flush: AtomicBool::new(false),
        }
    }
}

impl TransientState {
    pub fn solo(&self) -> Option<usize> {
        let v = self.solo.load(Ordering::Relaxed);
        if v < 0 || v as usize >= MAX_BANDS {
            None
        } else {
            Some(v as usize)
        }
    }

    pub fn set_solo(&self, slot: Option<usize>) {
        self.solo
            .store(slot.map(|s| s as i32).unwrap_or(-1), Ordering::Relaxed);
    }
}
