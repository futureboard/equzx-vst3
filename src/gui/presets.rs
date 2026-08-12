//! The preset store.
//!
//! Presets are plain files in a folder the user can open — saving one *is*
//! exporting it, and importing is dropping a file in.
//!
//! The format is the Futureboard Preset File: `.pst`, binary, little-endian,
//! opening with the magic `FBPF` and a format version. The layout is described
//! field by field at [`encode`]. Every quantity is fixed-width, unknown enum
//! codes fall back to their defaults, and everything read passes through
//! [`Snapshot::sanitized`] — a truncated or hand-hexed file loads as far as it
//! can or not at all, never as garbage parameters.
//!
//! The `.equz.json` files the earlier builds wrote still load: anything in the
//! folder that does not open with the magic is tried as JSON. Saving always
//! writes `.pst`.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::gui::state::{BandSnapshot, ResonanceSnapshot, Snapshot};
use crate::params::{
    BandChannel, BandKind, BandResMode, DynMode, ResMode, ResQuality, Slope, MAX_BANDS,
};

/// The JSON schema version, kept for the legacy reader. Bumped when the
/// resonance stage joined the snapshot.
pub const PRESET_VERSION: u32 = 2;

/// `FBPF` — Futureboard Preset File.
const MAGIC: &[u8; 4] = b"FBPF";
/// Bumped only for changes the reader below could not skip over. Version 2
/// added the resonance mode/quality/range and the per-band resonance fields;
/// version 1 files still load, with those fields on their defaults.
const PST_VERSION: u16 = 2;

const EXTENSION: &str = "pst";
const LEGACY_EXTENSION: &str = "equz.json";

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct PresetFile {
    name: String,
    version: u32,
    #[serde(flatten)]
    snapshot: Snapshot,
}

// --- the wire format --------------------------------------------------------

/// Serialise a preset as a Futureboard Preset File.
///
/// Layout, all integers and floats little-endian:
///
/// ```text
/// "FBPF"                magic, 4 bytes
/// u16                   format version (1)
/// u16                   name length in bytes
/// [u8]                  name, UTF-8
/// f32                   output gain, dB
/// u8                    resonance enabled (0/1)
/// f32 × 8               resonance: depth, sharpness, threshold, attack,
///                       release, low, high, mix
/// u8  (v2)              resonance mode (0 adaptive, 1 spectral)
/// u8  (v2)              resonance quality (0 ultra, 1 balanced, 2 high)
/// f32 (v2)              resonance range, dB
/// u8                    band count
/// per band, 36 bytes (v1) / 57 bytes (v2):
///   u8                  kind      (0 lowcut, 1 lowshelf, 2 bell, 3 notch,
///                                  4 bandpass, 5 highshelf, 6 highcut)
///   u8                  channel   (0 stereo, 1 mid, 2 side, 3 left, 4 right)
///   u8                  slope, dB/oct (12..96)
///   u8                  flags     (bit 0 enabled, bit 1 dynamic,
///                                  bit 2 dyn mode = below)
///   f32 × 8             freq, gain, q, dyn range, threshold, attack,
///                       release, resonance
///   u8  (v2)            res mode  (0 off, 1 adaptive, 2 spectral)
///   f32 × 5 (v2)        res: range, sens, width, attack, release
/// ```
fn encode(preset: &PresetFile) -> Vec<u8> {
    let name = preset.name.as_bytes();
    let name_len = name.len().min(u16::MAX as usize);
    let mut out = Vec::with_capacity(4 + 2 + 2 + name_len + 44 + preset.snapshot.bands.len() * 57);

    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&PST_VERSION.to_le_bytes());
    out.extend_from_slice(&(name_len as u16).to_le_bytes());
    out.extend_from_slice(&name[..name_len]);

    out.extend_from_slice(&preset.snapshot.output_gain.to_le_bytes());

    let r = &preset.snapshot.resonance;
    out.push(r.enabled as u8);
    for v in [
        r.depth,
        r.sharpness,
        r.threshold,
        r.attack,
        r.release,
        r.low,
        r.high,
        r.mix,
    ] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.push(match r.mode {
        ResMode::Adaptive => 0,
        ResMode::Spectral => 1,
    });
    out.push(match r.quality {
        ResQuality::Ultra => 0,
        ResQuality::Balanced => 1,
        ResQuality::High => 2,
    });
    out.extend_from_slice(&r.range.to_le_bytes());

    let bands = &preset.snapshot.bands[..preset.snapshot.bands.len().min(MAX_BANDS)];
    out.push(bands.len() as u8);
    for band in bands {
        out.push(match band.kind {
            BandKind::LowCut => 0,
            BandKind::LowShelf => 1,
            BandKind::Bell => 2,
            BandKind::Notch => 3,
            BandKind::BandPass => 4,
            BandKind::HighShelf => 5,
            BandKind::HighCut => 6,
        });
        out.push(match band.channel {
            BandChannel::Stereo => 0,
            BandChannel::Mid => 1,
            BandChannel::Side => 2,
            BandChannel::Left => 3,
            BandChannel::Right => 4,
        });
        out.push(band.slope.db_per_oct() as u8);
        out.push(band.enabled as u8
            | (band.dynamic as u8) << 1
            | ((band.dyn_mode == DynMode::Below) as u8) << 2);
        for v in [
            band.freq,
            band.gain,
            band.q,
            band.dyn_range,
            band.threshold,
            band.attack,
            band.release,
            band.resonance,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.push(match band.res_mode {
            BandResMode::Off => 0,
            BandResMode::Adaptive => 1,
            BandResMode::Spectral => 2,
        });
        for v in [
            band.res_range,
            band.res_sens,
            band.res_width,
            band.res_attack,
            band.res_release,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out
}

/// Parse a Futureboard Preset File. `None` for anything that does not carry
/// the magic, claims a version this build cannot read, or ends early.
fn decode(bytes: &[u8]) -> Option<PresetFile> {
    let mut cur = Cursor { bytes, at: 0 };
    if cur.take(4)? != MAGIC {
        return None;
    }
    let version = u16::from_le_bytes(cur.take(2)?.try_into().ok()?);
    if version == 0 || version > PST_VERSION {
        return None;
    }
    let name_len = u16::from_le_bytes(cur.take(2)?.try_into().ok()?) as usize;
    let name = String::from_utf8(cur.take(name_len)?.to_vec()).ok()?;

    let output_gain = cur.f32()?;
    let mut resonance = ResonanceSnapshot {
        enabled: cur.take(1)?[0] != 0,
        depth: cur.f32()?,
        sharpness: cur.f32()?,
        threshold: cur.f32()?,
        attack: cur.f32()?,
        release: cur.f32()?,
        low: cur.f32()?,
        high: cur.f32()?,
        mix: cur.f32()?,
        ..ResonanceSnapshot::default()
    };
    if version >= 2 {
        resonance.mode = match cur.take(1)?[0] {
            1 => ResMode::Spectral,
            _ => ResMode::Adaptive,
        };
        resonance.quality = match cur.take(1)?[0] {
            1 => ResQuality::Balanced,
            2 => ResQuality::High,
            _ => ResQuality::Ultra,
        };
        resonance.range = cur.f32()?;
    }

    let count = cur.take(1)?[0] as usize;
    let mut bands = Vec::with_capacity(count.min(MAX_BANDS));
    for _ in 0..count.min(MAX_BANDS) {
        let kind = match cur.take(1)?[0] {
            0 => BandKind::LowCut,
            1 => BandKind::LowShelf,
            3 => BandKind::Notch,
            4 => BandKind::BandPass,
            5 => BandKind::HighShelf,
            6 => BandKind::HighCut,
            _ => BandKind::Bell,
        };
        let channel = match cur.take(1)?[0] {
            1 => BandChannel::Mid,
            2 => BandChannel::Side,
            3 => BandChannel::Left,
            4 => BandChannel::Right,
            _ => BandChannel::Stereo,
        };
        let slope =
            Slope::from_db_per_oct(cur.take(1)?[0] as u32).unwrap_or(Slope::S24);
        let flags = cur.take(1)?[0];
        let mut band = BandSnapshot {
            kind,
            channel,
            slope,
            enabled: flags & 1 != 0,
            dynamic: flags & 2 != 0,
            dyn_mode: if flags & 4 != 0 {
                DynMode::Below
            } else {
                DynMode::Above
            },
            freq: cur.f32()?,
            gain: cur.f32()?,
            q: cur.f32()?,
            dyn_range: cur.f32()?,
            threshold: cur.f32()?,
            attack: cur.f32()?,
            release: cur.f32()?,
            resonance: cur.f32()?,
            ..BandSnapshot::default()
        };
        if version >= 2 {
            band.res_mode = match cur.take(1)?[0] {
                0 => BandResMode::Off,
                2 => BandResMode::Spectral,
                _ => BandResMode::Adaptive,
            };
            band.res_range = cur.f32()?;
            band.res_sens = cur.f32()?;
            band.res_width = cur.f32()?;
            band.res_attack = cur.f32()?;
            band.res_release = cur.f32()?;
        }
        bands.push(band);
    }

    Some(PresetFile {
        name,
        version: PRESET_VERSION,
        snapshot: Snapshot {
            bands,
            output_gain,
            resonance,
        },
    })
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let slice = self.bytes.get(self.at..self.at + n)?;
        self.at += n;
        Some(slice)
    }

    fn f32(&mut self) -> Option<f32> {
        Some(f32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
}

/// Where presets live: `<config>/EQUZX/presets`.
///
/// Resolved from the environment rather than through a crate, because the whole
/// requirement is three well-known variables and pulling in a dependency to read
/// them would be the larger cost.
pub fn directory() -> Option<PathBuf> {
    let base = if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    }?;
    Some(base.join("EQUZX").join("presets"))
}

/// Every preset on disk, by name, sorted the way a person would sort them.
pub fn list() -> Vec<String> {
    let Some(dir) = directory() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        // No folder yet simply means no presets yet.
        return Vec::new();
    };

    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file() {
                return None;
            }
            // Read the name the file claims rather than the one it is filed
            // under, so a preset called "Vocal / bright" is not shown as
            // "Vocal___bright" just because a slash cannot be a filename.
            read(&path).map(|preset| preset.name)
        })
        .collect();
    names.sort_by_key(|name| name.to_lowercase());
    names.dedup();
    names
}

pub fn load(name: &str) -> Option<Snapshot> {
    let preset = paths_for(name)?
        .into_iter()
        .filter(|path| path.is_file())
        .find_map(|path| read(&path))?;
    Some(preset.snapshot.sanitized())
}

pub fn save(name: &str, snapshot: &Snapshot) -> bool {
    let name = name.trim();
    if name.is_empty() {
        return false;
    }
    let Some(dir) = directory() else {
        return false;
    };
    if fs::create_dir_all(&dir).is_err() {
        return false;
    }
    let preset = PresetFile {
        name: name.to_owned(),
        version: PRESET_VERSION,
        snapshot: snapshot.sanitized(),
    };
    fs::write(dir.join(file_name(name)), encode(&preset)).is_ok()
}

pub fn delete(name: &str) -> bool {
    // Both spellings go: a legacy JSON left behind would bring the preset
    // back from the dead on the next listing.
    paths_for(name).is_some_and(|paths| {
        paths
            .into_iter()
            .filter(|path| path.is_file())
            .fold(false, |any, path| fs::remove_file(path).is_ok() || any)
    })
}

/// Show the preset folder in the system file manager. This is what stands in for
/// the old export and import dialogs: the folder *is* the exchange format.
pub fn reveal() -> bool {
    let Some(dir) = directory() else {
        return false;
    };
    if fs::create_dir_all(&dir).is_err() {
        return false;
    }
    let command = if cfg!(target_os = "windows") {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(command)
        .arg(&dir)
        .spawn()
        .is_ok()
}

/// Where a preset may be filed: the `.pst` it would be saved as today, then
/// the `.equz.json` an earlier build would have written.
fn paths_for(name: &str) -> Option<[PathBuf; 2]> {
    let dir = directory()?;
    let stem = file_stem(name);
    Some([
        dir.join(format!("{stem}.{EXTENSION}")),
        dir.join(format!("{stem}.{LEGACY_EXTENSION}")),
    ])
}

fn read(path: &Path) -> Option<PresetFile> {
    let bytes = fs::read(path).ok()?;
    let mut preset = if bytes.starts_with(MAGIC) {
        decode(&bytes)?
    } else {
        // Not a Futureboard Preset File: try it as the legacy JSON.
        serde_json::from_slice::<PresetFile>(&bytes).ok()?
    };
    if preset.name.trim().is_empty() {
        preset.name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("preset")
            .trim_end_matches(".pst")
            .trim_end_matches(".equz.json")
            .trim_end_matches(".json")
            .to_owned();
    }
    Some(preset)
}

/// Longest stem a filename is allowed to carry.
///
/// Windows still enforces a 260-character path unless long paths have been
/// switched on, and the folder this sits in is already sixty or so characters
/// deep under `%APPDATA%`. A preset with a paragraph for a name should come back
/// truncated rather than fail to save.
const MAX_STEM: usize = 64;

/// Device names Windows reserves, which cannot be a filename even with an
/// extension on the end — `CON.equz.json` is as unopenable as `CON`.
const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// A filename that every platform will accept, whatever the preset is called.
fn file_name(name: &str) -> String {
    format!("{}.{EXTENSION}", file_stem(name))
}

/// The stem of that filename — shared with the legacy path lookup, so a
/// preset saved as JSON by an earlier build is found under the same name.
///
/// Deliberately the same on every platform rather than conditional: a preset
/// folder should be copyable from a Mac to a PC and still work, and a name that
/// is legal here but not there is exactly what would break that.
fn file_stem(name: &str) -> String {
    let mut stem: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Everything above is ASCII by construction, so truncating by bytes cannot
    // land inside a character.
    stem.truncate(MAX_STEM);
    // After the truncation, so a name that is only trailing space once cut is
    // still caught.
    stem = stem.trim().to_owned();

    if stem.is_empty() {
        stem = "preset".into();
    }
    if RESERVED.contains(&stem.to_ascii_uppercase().as_str()) {
        stem.insert(0, '_');
    }
    stem
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::state::BandSnapshot;
    use crate::params::BandKind;

    #[test]
    fn a_name_becomes_a_filename_every_platform_accepts() {
        assert_eq!(file_name("Vocal Bright"), "Vocal Bright.pst");
        assert_eq!(file_name("bus/master"), "bus_master.pst");
        assert_eq!(file_name("../../etc/passwd"), "______etc_passwd.pst");
        assert_eq!(file_name("  "), "preset.pst");
        assert_eq!(file_name("crème"), "cr_me.pst");
    }

    #[test]
    fn a_filename_never_escapes_the_preset_folder() {
        for hostile in ["../evil", "..\\evil", "/etc/passwd", "a/../../b", "C:\\Windows"] {
            let name = file_name(hostile);
            assert!(!name.contains('/'), "{hostile} produced {name}");
            assert!(!name.contains('\\'), "{hostile} produced {name}");
            assert!(!name.contains(".."), "{hostile} produced {name}");
            assert!(!name.contains(':'), "{hostile} produced {name}");
        }
    }

    /// Windows keeps a handful of names for devices, and refuses them as
    /// filenames whatever extension is stuck on the end.
    #[test]
    fn a_name_windows_has_reserved_is_stepped_around() {
        assert_eq!(file_name("CON"), "_CON.pst");
        assert_eq!(file_name("nul"), "_nul.pst");
        assert_eq!(file_name("Com1"), "_Com1.pst");
        assert_eq!(file_name("LPT9"), "_LPT9.pst");
        // Only the whole stem is reserved, not a name that merely contains one.
        assert_eq!(file_name("Console"), "Console.pst");
        assert_eq!(file_name("COM10"), "COM10.pst");
    }

    /// A path over 260 characters is unopenable on a Windows install without
    /// long paths, and `%APPDATA%` has already spent sixty of them.
    #[test]
    fn a_very_long_name_is_truncated_rather_than_left_to_fail() {
        let name = file_name(&"x".repeat(500));
        assert_eq!(name.len(), MAX_STEM + 1 + EXTENSION.len());
        assert!(name.starts_with("xxxx"));

        // Truncation must not leave a trailing space, which Windows also
        // refuses at the end of a filename component.
        let name = file_name(&format!("{} tail", "y".repeat(MAX_STEM - 1)));
        assert!(!name[..name.len() - EXTENSION.len() - 1].ends_with(' '), "{name}");
    }

    /// Whatever comes out has to be legal on every platform, because a preset
    /// folder is something people copy between machines.
    #[test]
    fn a_filename_holds_no_character_any_platform_rejects() {
        for hostile in [
            "a<b>c:d\"e|f?g*h",
            "trailing.",
            "trailing ",
            "\u{0}nul byte",
            "emoji 🎛 knob",
        ] {
            let name = file_name(hostile);
            assert!(
                name.chars().all(|c| c.is_ascii_alphanumeric()
                    || matches!(c, '-' | '_' | ' ' | '.')),
                "{hostile} produced {name}"
            );
            let stem = &name[..name.len() - EXTENSION.len() - 1];
            assert!(!stem.ends_with(' ') && !stem.ends_with('.'), "{hostile} produced {name}");
            assert!(!stem.is_empty(), "{hostile} produced {name}");
        }
    }

    /// Everything a snapshot can hold survives the binary round trip exactly:
    /// f32s are stored as their own bits, and every enum code maps back.
    #[test]
    fn a_futureboard_preset_file_round_trips() {
        use crate::gui::state::ResonanceSnapshot;
        use crate::params::{BandChannel, DynMode, Slope};

        let snapshot = Snapshot {
            output_gain: -2.25,
            bands: vec![
                BandSnapshot {
                    kind: BandKind::HighCut,
                    channel: BandChannel::Side,
                    freq: 12_000.0,
                    slope: Slope::S96,
                    enabled: false,
                    ..BandSnapshot::default()
                },
                BandSnapshot {
                    dynamic: true,
                    dyn_mode: DynMode::Below,
                    dyn_range: -9.5,
                    threshold: -31.0,
                    resonance: 40.0,
                    res_mode: BandResMode::Spectral,
                    res_range: 9.0,
                    res_sens: -3.5,
                    res_width: 0.75,
                    res_attack: 2.5,
                    res_release: 120.0,
                    ..BandSnapshot::default()
                },
            ],
            resonance: ResonanceSnapshot {
                enabled: true,
                depth: 72.0,
                high: 14_500.0,
                mode: ResMode::Spectral,
                quality: ResQuality::Balanced,
                range: 18.0,
                ..ResonanceSnapshot::default()
            },
        };

        let preset = PresetFile {
            name: "Vocal / bright ✨".into(),
            version: PRESET_VERSION,
            snapshot: snapshot.clone(),
        };
        let bytes = encode(&preset);
        assert_eq!(&bytes[..4], b"FBPF");

        let back = decode(&bytes).expect("should have decoded");
        assert_eq!(back.name, preset.name);
        assert_eq!(back.snapshot, snapshot);
    }

    /// A version-1 file — written before the spectral fields existed — still
    /// loads, with the new fields on their defaults.
    #[test]
    fn a_version_one_preset_still_loads() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"FBPF");
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(b"V1");
        bytes.extend_from_slice(&(-1.5f32).to_le_bytes());
        // The v1 resonance block: enabled + eight floats.
        bytes.push(1);
        for v in [60.0f32, 50.0, 6.0, 5.0, 40.0, 20.0, 20_000.0, 100.0] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        // One v1 band: four bytes of codes, eight floats.
        bytes.push(1);
        bytes.extend_from_slice(&[2, 0, 24, 0b001]);
        for v in [440.0f32, -3.0, 2.0, -6.0, -24.0, 20.0, 200.0, 30.0] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }

        let preset = decode(&bytes).expect("a v1 file should still decode");
        assert_eq!(preset.name, "V1");
        assert_eq!(preset.snapshot.resonance.depth, 60.0);
        // The fields v1 could not carry land on their defaults.
        assert_eq!(preset.snapshot.resonance.mode, ResMode::Adaptive);
        assert_eq!(preset.snapshot.resonance.range, 36.0);
        let band = preset.snapshot.bands[0];
        assert_eq!(band.freq, 440.0);
        assert_eq!(band.resonance, 30.0);
        assert_eq!(band.res_mode, BandResMode::Adaptive);
        assert_eq!(band.res_width, 1.0);
    }

    /// A file cut off mid-band reads as nothing rather than as noise.
    #[test]
    fn a_truncated_preset_file_is_refused() {
        let preset = PresetFile {
            name: "Test".into(),
            version: PRESET_VERSION,
            snapshot: Snapshot {
                bands: vec![BandSnapshot::default()],
                ..Snapshot::default()
            },
        };
        let bytes = encode(&preset);
        for cut in [3, 7, 12, bytes.len() - 5] {
            assert!(decode(&bytes[..cut]).is_none(), "accepted {cut} bytes");
        }
        // A version from the future is refused outright rather than misread.
        let mut wrong = bytes.clone();
        wrong[4] = 0xff;
        assert!(decode(&wrong).is_none());
    }

    /// The `.equz.json` an earlier build wrote still opens.
    #[test]
    fn a_legacy_json_preset_still_loads() {
        let dir = std::env::temp_dir().join("equzx-preset-test");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Legacy.equz.json");
        fs::write(
            &path,
            r#"{"name":"Legacy","version":2,"bands":[{"type":"highcut","freq":9000}],"outputGain":-1.5}"#,
        )
        .unwrap();

        let preset = read(&path).expect("should have parsed");
        assert_eq!(preset.name, "Legacy");
        assert_eq!(preset.snapshot.output_gain, -1.5);
        assert_eq!(preset.snapshot.bands[0].kind, BandKind::HighCut);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_file_with_no_name_is_named_after_itself() {
        let dir = std::env::temp_dir().join("equzx-preset-test");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Fallback.pst");
        let preset = PresetFile {
            name: String::new(),
            version: PRESET_VERSION,
            snapshot: Snapshot::default(),
        };
        fs::write(&path, encode(&preset)).unwrap();

        let preset = read(&path).expect("should have parsed");
        assert_eq!(preset.name, "Fallback");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn an_unparseable_file_is_skipped_rather_than_fatal() {
        let dir = std::env::temp_dir().join("equzx-preset-test");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Broken.equz.json");
        fs::write(&path, "{ this is not json").unwrap();
        assert!(read(&path).is_none());
        let _ = fs::remove_file(&path);
    }
}
