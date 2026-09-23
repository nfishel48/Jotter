//! `jotter config` — read and change the stored preferences.
//!
//! The settings file is still plain JSON and still fine to edit by hand, but a
//! command is what an agent, a script or a person who does not know where
//! `~/Library/Application Support` is can use. Only the keys a user would
//! reasonably change are exposed, under the short names people say out loud
//! ("turn transcription on"), not the file's field names; bookkeeping such as
//! the install id is deliberately not reachable from here.

use clap::{Args, Subcommand, ValueEnum};
use serde::Serialize;

use jotter::config::{self, Settings};

use crate::output::{CliError, ErrorKind, Output};

#[derive(Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    command: Option<ConfigCommand>,
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Print one setting
    Get { key: Key },
    /// Change one setting. Booleans take true/false (or on/off, yes/no, 1/0);
    /// `speakers` takes a count, or `none` to unset it
    Set { key: Key, value: String },
}

#[derive(Clone, Copy, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
enum Key {
    /// Remove speaker echo from the mic track when a recording stops
    Aec,
    /// Transcribe every recording when it stops
    Transcribe,
    /// Label the people on the system track after transcribing
    Diarize,
    /// How many people are on your calls; diarization declines without it
    Speakers,
    /// Send anonymous usage and crash reports
    Telemetry,
}

impl Key {
    const ALL: [Key; 5] = [
        Key::Aec,
        Key::Transcribe,
        Key::Diarize,
        Key::Speakers,
        Key::Telemetry,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Aec => "aec",
            Self::Transcribe => "transcribe",
            Self::Diarize => "diarize",
            Self::Speakers => "speakers",
            Self::Telemetry => "telemetry",
        }
    }

    fn get(self, settings: &Settings) -> Value {
        match self {
            Self::Aec => Value::Flag(settings.aec_enabled),
            Self::Transcribe => Value::Flag(settings.transcribe_enabled),
            Self::Diarize => Value::Flag(settings.diarize_enabled),
            Self::Speakers => Value::Count(settings.speaker_count()),
            Self::Telemetry => Value::Flag(settings.telemetry_enabled),
        }
    }

    /// Parse `raw` for this key and store it in `settings`, returning what was
    /// stored. Each value is parsed before anything is assigned, so a value
    /// that does not parse changes nothing.
    fn set(self, settings: &mut Settings, raw: &str) -> Result<Value, CliError> {
        match self {
            Self::Aec => settings.aec_enabled = parse_flag(self, raw)?,
            Self::Transcribe => settings.transcribe_enabled = parse_flag(self, raw)?,
            Self::Diarize => settings.diarize_enabled = parse_flag(self, raw)?,
            Self::Telemetry => {
                settings.telemetry_enabled = parse_flag(self, raw)?;
                // What `jotter telemetry --enable/--disable` does, and for the
                // same reason: having made the choice, the user has read
                // enough, and the first-run notice would only repeat it.
                settings.telemetry_notice_seen = true;
            }
            // `0` is the file's spelling of "not set"; see
            // `Settings::speaker_count`.
            Self::Speakers => settings.diarize_speakers = parse_count(raw)?.unwrap_or(0),
        }
        Ok(self.get(settings))
    }
}

/// A setting's value as `--json` prints it: a boolean, or a count that is
/// `null` when unset.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(untagged)]
enum Value {
    Flag(bool),
    Count(Option<u8>),
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Flag(on) => write!(f, "{on}"),
            Self::Count(Some(n)) => write!(f, "{n}"),
            Self::Count(None) => f.write_str("none"),
        }
    }
}

/// Generous in what it accepts, since this is typed by people as often as by
/// scripts, and every spelling here is unambiguous.
fn parse_flag(key: Key, raw: &str) -> Result<bool, CliError> {
    match raw.to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "1" => Ok(true),
        "false" | "off" | "no" | "0" => Ok(false),
        _ => Err(CliError::new(
            ErrorKind::InvalidArgument,
            format!("{} takes true or false, not {raw:?}", key.name()),
        )),
    }
}

fn parse_count(raw: &str) -> Result<Option<u8>, CliError> {
    let raw = raw.to_ascii_lowercase();
    if matches!(raw.as_str(), "none" | "unset" | "0") {
        return Ok(None);
    }
    match raw.parse::<u8>() {
        Ok(n) => Ok(Some(n)),
        Err(_) => Err(CliError::new(
            ErrorKind::InvalidArgument,
            format!(
                "speakers takes a number of people from 1 to {}, or none to unset it; not {raw:?}",
                u8::MAX
            ),
        )),
    }
}

#[derive(Serialize)]
struct AllJson {
    path: std::path::PathBuf,
    settings: std::collections::BTreeMap<&'static str, Value>,
}

#[derive(Serialize)]
struct OneJson {
    key: Key,
    value: Value,
}

/// `jotter config [get KEY | set KEY VALUE]`.
///
/// Takes no `Telemetry`, for the reason `jotter telemetry` gives: this edits a
/// file, and one of the things it can do is turn reporting off.
pub fn config(args: ConfigArgs, out: Output) -> Result<(), CliError> {
    let mut settings = Settings::load();

    match args.command {
        None => {
            let all = AllJson {
                path: config::path(),
                settings: Key::ALL
                    .into_iter()
                    .map(|key| (key.name(), key.get(&settings)))
                    .collect(),
            };
            out.emit(&all, |all| {
                for key in Key::ALL {
                    println!("{:<11} {}", key.name(), all.settings[key.name()]);
                }
                if let Some(note) = telemetry_override_note() {
                    println!("\n{note}");
                }
                println!("\nstored in {}", all.path.display());
            });
        }
        Some(ConfigCommand::Get { key }) => {
            let one = OneJson {
                key,
                value: key.get(&settings),
            };
            out.emit(&one, |one| println!("{}", one.value));
        }
        Some(ConfigCommand::Set { key, value }) => {
            let value = key.set(&mut settings, &value)?;
            settings.save()?;
            out.emit(&OneJson { key, value }, |one| {
                println!("{} = {}", one.key.name(), one.value);
                if matches!(key, Key::Telemetry)
                    && let Some(note) = telemetry_override_note()
                {
                    println!("{note}");
                }
            });
        }
    }
    Ok(())
}

/// The stored telemetry choice is not the whole story when the environment
/// overrides it, and showing only the stored value would mislead exactly the
/// person who set `DO_NOT_TRACK` and is checking it took.
fn telemetry_override_note() -> Option<&'static str> {
    match config::env_override() {
        config::EnvOverride::ForceOff => {
            Some("note: telemetry is overridden to OFF by DO_NOT_TRACK / JOTTER_TELEMETRY")
        }
        config::EnvOverride::ForceOn => {
            Some("note: telemetry is overridden to ON by JOTTER_TELEMETRY")
        }
        config::EnvOverride::Unset => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_then_get_round_trips_every_key() {
        let mut settings = Settings::default();
        for (key, raw, want) in [
            (Key::Aec, "off", Value::Flag(false)),
            (Key::Transcribe, "true", Value::Flag(true)),
            (Key::Diarize, "YES", Value::Flag(true)),
            (Key::Speakers, "4", Value::Count(Some(4))),
            (Key::Telemetry, "0", Value::Flag(false)),
        ] {
            assert_eq!(key.set(&mut settings, raw).unwrap(), want);
            assert_eq!(key.get(&settings), want, "{}", key.name());
        }
    }

    /// Turning telemetry off from here must count as having seen the notice,
    /// or the next run would announce reporting the user just disabled.
    #[test]
    fn setting_telemetry_marks_the_notice_seen() {
        let mut settings = Settings::default();
        Key::Telemetry.set(&mut settings, "false").unwrap();
        assert!(settings.telemetry_notice_seen);
    }

    #[test]
    fn speakers_can_be_unset_and_rejects_out_of_range() {
        let mut settings = Settings::default();
        Key::Speakers.set(&mut settings, "3").unwrap();
        assert_eq!(
            Key::Speakers.set(&mut settings, "none").unwrap(),
            Value::Count(None)
        );
        assert_eq!(settings.diarize_speakers, 0);

        for bad in ["256", "-1", "three"] {
            let err = Key::Speakers.set(&mut settings, bad).unwrap_err();
            assert_eq!(err.kind, ErrorKind::InvalidArgument, "{bad}");
        }
    }

    #[test]
    fn a_bad_value_changes_nothing() {
        let mut settings = Settings::default();
        let before = settings.clone();
        let err = Key::Transcribe.set(&mut settings, "maybe").unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidArgument);
        assert_eq!(settings, before);
    }
}
