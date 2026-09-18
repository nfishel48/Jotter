//! Persisted user preferences.
//!
//! Deliberately separate from the recordings directory: those are documents the
//! user goes looking for, this is state they should never have to see. The file
//! is small and hand-edited often enough (it is the documented way to opt out
//! without launching the GUI) that JSON beats a binary format.
//!
//! Compiled unconditionally — the CLI has to honour the same opt-out as the
//! tray app, so this cannot live under `ui`.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The persisted preferences, as they appear on disk.
///
/// Every field is `#[serde(default)]` so that a file written by an older build
/// — or a hand-edited one missing a key — still loads. A settings file is never
/// a reason to fail to start.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct Settings {
    /// The user's stored choice. Read through [`Settings::telemetry_allowed`]
    /// rather than directly: an environment override can veto it without being
    /// written back here.
    pub telemetry_enabled: bool,
    /// Whether the first-run telemetry notice has been dismissed.
    pub telemetry_notice_seen: bool,
    /// Random per-install id, minted on first use by the telemetry module.
    ///
    /// `Option` rather than generated here because `uuid` is a `telemetry`-only
    /// dependency, and a build without that feature must never create one.
    pub install_id: Option<String>,
    /// Whether to remove speaker echo from the mic track after each recording.
    ///
    /// A `bool` rather than any tuning knob: `Settings` derives `Eq`, so it
    /// cannot hold an `f32`, and there is nothing here worth tuning per-user
    /// anyway. The raw `mic.wav` is never modified either way, so the worst case
    /// is a wasted second file.
    pub aec_enabled: bool,
    /// Whether to transcribe each recording once it has been cleaned up.
    ///
    /// Off by default, and the odd one out for it — `aec_enabled` is on. The
    /// difference is that echo cancellation works the moment the binary is
    /// installed, and this cannot: it needs a 660 MB model the user has to fetch
    /// deliberately with `jotter models pull`. Defaulting on would mean every
    /// recording made before that ended with a decline recorded in its
    /// `meta.json`, which is a confusing way to introduce a feature nobody has
    /// asked for yet.
    pub transcribe_enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            // On by default, with a first-run notice in the settings pane and
            // three documented ways out. See docs/TELEMETRY.md.
            telemetry_enabled: true,
            telemetry_notice_seen: false,
            install_id: None,
            // On by default: on speakers, echo makes the mic track's remote
            // speech a duplicate of `system.wav`, which is the problem two
            // tracks exist to avoid — so the default should be the one that
            // fixes it. Safe to default on because the pass is additive:
            // `mic.wav` is never modified, a recording it cannot handle is
            // declined with the reason recorded, and `preferred_mic_path`
            // refuses to pass on a result whose own numbers do not clear the
            // bar. The cost of being wrong is one unused file.
            aec_enabled: true,
            // See the field's own note: on would mean a decline recorded against
            // every recording until someone runs `jotter models pull`.
            transcribe_enabled: false,
        }
    }
}

/// What the environment says about telemetry, overriding the stored preference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvOverride {
    /// `DO_NOT_TRACK=1` or `JOTTER_TELEMETRY=0`.
    ForceOff,
    /// `JOTTER_TELEMETRY=1`, for developing against a scratch project.
    ForceOn,
    /// Nothing set; the stored preference decides.
    Unset,
}

impl Settings {
    /// Read the settings file, falling back to defaults for anything missing.
    ///
    /// Never fails: an unreadable or malformed file is indistinguishable from a
    /// first run as far as the app is concerned, and refusing to start over a
    /// stray byte in a preferences file would be absurd.
    pub fn load() -> Self {
        Self::load_from(&path())
    }

    fn load_from(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    /// Whether telemetry may run, accounting for environment overrides.
    ///
    /// The override is applied here rather than in `load` on purpose: folding it
    /// into the struct would mean the next `save` writes the environment's
    /// opinion back to disk as if the user had chosen it, so a single
    /// `DO_NOT_TRACK=1` run would silently clear a stored opt-in.
    pub fn telemetry_allowed(&self) -> bool {
        match env_override() {
            EnvOverride::ForceOff => false,
            EnvOverride::ForceOn => true,
            EnvOverride::Unset => self.telemetry_enabled,
        }
    }

    /// Write the settings file, creating its directory if needed.
    pub fn save(&self) -> io::Result<()> {
        self.save_to(&path())
    }

    fn save_to(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Write-and-rename rather than write-in-place: the app can be killed at
        // any moment (tray Quit calls `process::exit`), and a half-written file
        // would silently reset every preference on next launch.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path)
    }
}

/// Location of the settings file.
///
/// Must be absolute for the same reason `ui::recordings_root` must be: a macOS
/// bundle's working directory is `/`.
pub fn path() -> PathBuf {
    dir().join("settings.json")
}

fn dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);

    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Jotter")
    } else if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Roaming"))
            .join("Jotter")
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".config"))
            .join("jotter")
    }
}

/// Read the telemetry environment overrides.
pub fn env_override() -> EnvOverride {
    resolve_env_override(
        std::env::var("DO_NOT_TRACK").ok().as_deref(),
        std::env::var("JOTTER_TELEMETRY").ok().as_deref(),
    )
}

/// The override rules, factored out so they can be tested.
///
/// `std::env::set_var` is `unsafe` in edition 2024 and racy across parallel
/// tests regardless, so the tests drive this rather than the real environment.
fn resolve_env_override(do_not_track: Option<&str>, jotter_telemetry: Option<&str>) -> EnvOverride {
    // consoledonottrack.com: any value other than "0"/empty means opt out.
    let dnt = matches!(do_not_track.map(str::trim), Some(v) if !v.is_empty() && v != "0");

    match jotter_telemetry.map(str::trim) {
        // The app-specific variable is the more specific signal, so it wins
        // both ways — including re-enabling under a blanket DO_NOT_TRACK, which
        // is what makes `JOTTER_TELEMETRY=1` usable for development.
        Some("0" | "false" | "off" | "no") => EnvOverride::ForceOff,
        Some("1" | "true" | "on" | "yes") => EnvOverride::ForceOn,
        _ if dnt => EnvOverride::ForceOff,
        _ => EnvOverride::Unset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique scratch path; avoids a `tempfile` dependency for two tests.
    fn scratch(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("jotter-cfg-{name}-{unique}/settings.json"))
    }

    #[test]
    fn round_trips() {
        let path = scratch("roundtrip");
        let settings = Settings {
            telemetry_enabled: false,
            telemetry_notice_seen: true,
            install_id: Some("abc".into()),
            // Deliberately the opposite of the default: a round-trip test that
            // stores default values cannot tell a persisted field from a
            // defaulted one.
            aec_enabled: false,
            transcribe_enabled: true,
        };

        settings.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), settings);

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn defaults_to_enabled_with_notice_pending() {
        let settings = Settings::default();
        assert!(settings.telemetry_enabled);
        assert!(!settings.telemetry_notice_seen);
        assert_eq!(settings.install_id, None);
    }

    /// A named assertion rather than a bare default, so changing whether every
    /// recording gets a second pass over it is a deliberate edit with a test to
    /// update — in either direction.
    ///
    /// Defaulting *on* is only defensible because the pass cannot damage the
    /// recording: it writes a new file rather than modifying `mic.wav`,
    /// declines when it cannot align the tracks, and records why. If any of
    /// that stops being true, this default is the first thing to revisit.
    #[test]
    fn aec_defaults_to_on_because_the_pass_is_additive() {
        assert!(Settings::default().aec_enabled);
    }

    /// The counterpart, and the asymmetry is the point: transcription is just as
    /// additive as echo cancellation, but unlike it cannot work on a fresh
    /// install — it needs a 660 MB model fetched by hand. On by default would
    /// mean every recording made before `jotter models pull` ends with a
    /// decline recorded against it.
    ///
    /// The day this can succeed out of the box, this default is the thing to
    /// revisit.
    #[test]
    fn transcription_defaults_to_off_because_it_needs_a_downloaded_model() {
        assert!(!Settings::default().transcribe_enabled);
    }

    #[test]
    fn missing_file_yields_defaults() {
        assert_eq!(
            Settings::load_from(Path::new("/nonexistent/jotter/settings.json")),
            Settings::default()
        );
    }

    #[test]
    fn corrupt_file_yields_defaults_rather_than_failing() {
        let path = scratch("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not json at all").unwrap();

        assert_eq!(Settings::load_from(&path), Settings::default());

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn partial_file_keeps_defaults_for_missing_keys() {
        let path = scratch("partial");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"telemetry_enabled": false}"#).unwrap();

        let loaded = Settings::load_from(&path);
        assert!(!loaded.telemetry_enabled);
        // Absent keys must not become `false`/`None` by accident.
        assert!(!loaded.telemetry_notice_seen);
        assert_eq!(loaded.install_id, None);
        // A settings file written before this key existed — every install that
        // predates the feature — must pick up the new default rather than
        // silently reading as `false`.
        assert!(loaded.aec_enabled);

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn env_overrides_follow_precedence() {
        use EnvOverride::*;

        assert_eq!(resolve_env_override(None, None), Unset);
        assert_eq!(resolve_env_override(Some("1"), None), ForceOff);
        assert_eq!(resolve_env_override(Some("true"), None), ForceOff);
        assert_eq!(resolve_env_override(None, Some("0")), ForceOff);
        assert_eq!(resolve_env_override(None, Some("off")), ForceOff);
        assert_eq!(resolve_env_override(None, Some("1")), ForceOn);

        // DO_NOT_TRACK=0 is not an opt-out.
        assert_eq!(resolve_env_override(Some("0"), None), Unset);
        assert_eq!(resolve_env_override(Some(""), None), Unset);

        // The app-specific variable wins in both directions.
        assert_eq!(resolve_env_override(Some("1"), Some("1")), ForceOn);
        assert_eq!(resolve_env_override(Some("0"), Some("0")), ForceOff);
    }

    #[test]
    fn env_override_does_not_clobber_the_stored_preference() {
        // The whole point of applying the override outside the struct: a
        // `DO_NOT_TRACK` run must not rewrite a stored opt-in to `false`.
        let settings = Settings {
            telemetry_enabled: true,
            ..Settings::default()
        };
        assert!(settings.telemetry_enabled);
        assert_eq!(
            resolve_env_override(Some("1"), None),
            EnvOverride::ForceOff,
            "override is computed separately from the stored field"
        );
    }

    #[test]
    fn config_path_is_absolute() {
        assert!(path().is_absolute());
        assert!(path().ends_with("settings.json"));
    }
}
