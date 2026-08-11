//! The preset store.
//!
//! The web UI kept presets in `localStorage` and exchanged them through the
//! browser's download and file-picker. Neither exists here, so presets are plain
//! files in a folder the user can open — which is a better answer anyway: saving
//! one *is* exporting it, and importing is dropping a file in.
//!
//! The format is unchanged from `state/presets.ts`, so a `.equz.json` written by
//! the old UI loads here and vice versa. See [`crate::gui::state`] for the
//! shapes and for what a file is allowed to get away with.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::gui::state::Snapshot;

/// Bumped when the resonance stage joined the snapshot. Nothing reads it to
/// migrate — the format only ever gains fields, and the sanitiser fills in
/// whatever an older file left out — but a file should still say what shape it
/// was written in.
pub const PRESET_VERSION: u32 = 2;

const EXTENSION: &str = "equz.json";

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct PresetFile {
    name: String,
    version: u32,
    #[serde(flatten)]
    snapshot: Snapshot,
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
    let path = path_for(name)?;
    read(&path)
        .filter(|preset| preset.name == name)
        .or_else(|| {
            // Filed under a sanitised name but claiming a different one, or an
            // older file whose name field is missing: fall back to the path.
            read(&path)
        })
        .map(|preset| preset.snapshot.sanitized())
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
    let Ok(text) = serde_json::to_string_pretty(&preset) else {
        return false;
    };
    fs::write(dir.join(file_name(name)), text).is_ok()
}

pub fn delete(name: &str) -> bool {
    path_for(name).is_some_and(|path| fs::remove_file(path).is_ok())
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

fn path_for(name: &str) -> Option<PathBuf> {
    Some(directory()?.join(file_name(name)))
}

fn read(path: &Path) -> Option<PresetFile> {
    let text = fs::read_to_string(path).ok()?;
    let mut preset: PresetFile = serde_json::from_str(&text).ok()?;
    if preset.name.trim().is_empty() {
        preset.name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("preset")
            .trim_end_matches(".equz.json")
            .trim_end_matches(".json")
            .to_owned();
    }
    Some(preset)
}

/// A filename that every platform will accept, whatever the preset is called.
fn file_name(name: &str) -> String {
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
    stem = stem.trim().to_owned();
    if stem.is_empty() {
        stem = "preset".into();
    }
    format!("{stem}.{EXTENSION}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::state::BandSnapshot;
    use crate::params::BandKind;

    #[test]
    fn a_name_becomes_a_filename_every_platform_accepts() {
        assert_eq!(file_name("Vocal Bright"), "Vocal Bright.equz.json");
        assert_eq!(file_name("bus/master"), "bus_master.equz.json");
        assert_eq!(file_name("../../etc/passwd"), "______etc_passwd.equz.json");
        assert_eq!(file_name("  "), "preset.equz.json");
        assert_eq!(file_name("crème"), "cr_me.equz.json");
    }

    #[test]
    fn a_filename_never_escapes_the_preset_folder() {
        for hostile in ["../evil", "..\\evil", "/etc/passwd", "a/../../b"] {
            let name = file_name(hostile);
            assert!(!name.contains('/'), "{hostile} produced {name}");
            assert!(!name.contains('\\'), "{hostile} produced {name}");
            assert!(!name.contains(".."), "{hostile} produced {name}");
        }
    }

    #[test]
    fn a_preset_file_round_trips() {
        let snapshot = Snapshot {
            output_gain: -2.0,
            bands: vec![BandSnapshot {
                kind: BandKind::HighCut,
                freq: 12_000.0,
                ..BandSnapshot::default()
            }],
            ..Snapshot::default()
        };

        let preset = PresetFile {
            name: "Test".into(),
            version: PRESET_VERSION,
            snapshot: snapshot.clone(),
        };
        let text = serde_json::to_string(&preset).unwrap();
        // Flattened, so a file looks the way `state/presets.ts` wrote one.
        assert!(text.contains(r#""name":"Test""#));
        assert!(text.contains(r#""outputGain":-2.0"#));
        assert!(text.contains(r#""type":"highcut""#));

        let back: PresetFile = serde_json::from_str(&text).unwrap();
        assert_eq!(back.snapshot, snapshot);
    }

    #[test]
    fn a_file_with_no_name_is_named_after_itself() {
        let dir = std::env::temp_dir().join("equzx-preset-test");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Fallback.equz.json");
        fs::write(&path, r#"{"name":"","version":2,"bands":[],"outputGain":0}"#).unwrap();

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
