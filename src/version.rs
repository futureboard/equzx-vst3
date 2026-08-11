//! Release metadata.
//!
//! `version.json` at the repo root is the single source of truth: the CI
//! workflow names its artifacts from it, and both installers stamp themselves
//! with it. Cargo needs its own literal in `Cargo.toml`, so the one thing that
//! can go wrong is the two drifting apart — which is what the test below is for.
//!
//! The scheme is CalVer, `YYYY.M.D`, starting at 2026.8.11. That happens to be
//! valid semver as well, which is what VST3 and CLAP hosts expect to parse.

/// The version the plugin reports to the host, from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `version.json`, verbatim. Small enough to carry, and it keeps the file from
/// being deleted without the build noticing.
pub const MANIFEST: &str = include_str!("../version.json");

#[cfg(test)]
mod tests {
    use super::*;
    use nih_plug::prelude::{ClapPlugin, Plugin};

    fn manifest() -> serde_json::Value {
        serde_json::from_str(MANIFEST).expect("version.json is not valid JSON")
    }

    #[test]
    fn the_manifest_and_cargo_agree_on_the_version() {
        let manifest = manifest();
        let declared = manifest["version"].as_str().expect("no version field");
        assert_eq!(
            declared, VERSION,
            "version.json says {declared} but Cargo.toml says {VERSION} — update both"
        );
    }

    #[test]
    fn the_manifest_matches_the_plugin_identity() {
        let manifest = manifest();
        assert_eq!(manifest["name"], crate::Equzx::NAME);
        assert_eq!(manifest["vendor"], crate::Equzx::VENDOR);
        assert_eq!(manifest["bundleId"], crate::Equzx::CLAP_ID);
    }

    #[test]
    fn the_version_is_calver_a_host_can_still_parse_as_semver() {
        let version = manifest();
        let version = version["version"].as_str().unwrap();
        let parts: Vec<&str> = version.split('.').collect();
        assert_eq!(parts.len(), 3, "expected YYYY.M.D, got {version}");

        let year: u32 = parts[0].parse().expect("year");
        let month: u32 = parts[1].parse().expect("month");
        let day: u32 = parts[2].parse().expect("day");
        assert!((2026..3000).contains(&year), "implausible year {year}");
        assert!((1..=12).contains(&month), "bad month {month}");
        assert!((1..=31).contains(&day), "bad day {day}");
        // Leading zeros would still parse as a number but are not valid semver.
        assert!(!parts.iter().any(|p| p.len() > 1 && p.starts_with('0')));
    }

    #[test]
    fn the_release_date_matches_the_version() {
        let manifest = manifest();
        let version = manifest["version"].as_str().unwrap();
        let released = manifest["released"].as_str().expect("no released field");

        let v: Vec<u32> = version.split('.').map(|p| p.parse().unwrap()).collect();
        let d: Vec<u32> = released.split('-').map(|p| p.parse().unwrap()).collect();
        assert_eq!(
            v, d,
            "version {version} and release date {released} disagree"
        );
    }
}
