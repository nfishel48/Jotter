//! `jotter recordings`: the recent recording directories, and which artifacts
//! each one actually has.
//!
//! The list is the recordings root, one level deep. A host that passed `--out`
//! elsewhere is still visible while that session is the active one — otherwise
//! the recording an agent just started could be missing from the only command
//! that lists recordings.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, SecondsFormat, Utc};
use clap::Args;
use serde::Serialize;

use jotter::audio::live;
use jotter::audio::meta::{self, Meta};

use crate::output::{CliError, Output};
use crate::session::{Lookup, SessionFile};

/// How many recordings `jotter recordings` lists when `--limit` is omitted.
const DEFAULT_LIMIT: usize = 10;

#[derive(Args)]
pub struct RecordingsArgs {
    /// How many recordings to list, newest first.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_LIMIT)]
    limit: usize,
}

/// `jotter recordings`.
pub fn run(args: RecordingsArgs, out: Output) -> Result<(), CliError> {
    let extra = match SessionFile::default_location().lookup()? {
        Lookup::Active(session) => Some(session.dir),
        Lookup::Idle | Lookup::Stale(_) => None,
    };
    let value = RecordingsJson {
        recordings: list(
            jotter::config::recordings_root(),
            extra.as_deref(),
            args.limit,
        )?,
    };
    out.emit(&value, print_human);
    Ok(())
}

/// Newest first. `extra` is included even when it sits outside the root or
/// would otherwise fall past `--limit`, so the session an agent just started
/// is never missing from the only command that lists recordings.
fn list(
    root: impl AsRef<Path>,
    extra: Option<&Path>,
    limit: usize,
) -> Result<Vec<RecordingJson>, CliError> {
    let root = root.as_ref();
    let mut dirs = Vec::new();
    if root.is_dir() {
        for entry in std::fs::read_dir(root)? {
            let path = entry?.path();
            if path.is_dir() && is_recording(&path) {
                dirs.push(path);
            }
        }
    }
    if let Some(extra) = extra
        && extra.is_dir()
        && !dirs.iter().any(|dir| same_dir(dir, extra))
    {
        dirs.push(extra.to_path_buf());
    }

    dirs.sort_by(|a, b| recency(b).total_cmp(&recency(a)).then(b.cmp(a)));
    let mut kept: Vec<PathBuf> = dirs.into_iter().take(limit).collect();
    if let Some(extra) = extra.filter(|dir| dir.is_dir())
        && !kept.iter().any(|dir| same_dir(dir, extra))
        && limit > 0
    {
        if kept.len() == limit {
            kept.pop();
        }
        kept.insert(0, extra.to_path_buf());
    }
    Ok(kept
        .into_iter()
        .map(|dir| RecordingJson::from_dir(&dir))
        .collect())
}

/// Whether `dir` looks like a recording rather than a stray folder.
///
/// Any one artifact is enough. `meta.json` alone counts too: a recording that
/// declined every pass still has the sidecar, and should still be listed.
pub(crate) fn is_recording(dir: &Path) -> bool {
    dir.join("meta.json").is_file()
        || dir.join(meta::MIC_NAME).is_file()
        || dir.join(meta::SYSTEM_NAME).is_file()
        || dir.join(live::FILENAME).is_file()
        || dir.join("transcript.json").is_file()
        || dir.join("mic_aec.wav").is_file()
}

/// Sort key: `meta.json`'s start when the recording was finalised, otherwise
/// the directory's mtime. Either way, larger is newer.
pub(crate) fn recency(dir: &Path) -> f64 {
    if let Ok(meta) = Meta::read(&dir.join("meta.json")) {
        return meta.started_at;
    }
    std::fs::metadata(dir)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    norm(a) == norm(b)
}

#[derive(Serialize)]
struct RecordingsJson {
    recordings: Vec<RecordingJson>,
}

#[derive(Serialize)]
struct RecordingJson {
    dir: PathBuf,
    /// RFC3339 from `meta.json`, or null while the recording has not been
    /// finalised. Always present, so a reader does not have to test for the key.
    started_at: Option<String>,
    /// Null for the same reason as `started_at`.
    duration_secs: Option<f64>,
    artifacts: ArtifactsJson,
}

impl RecordingJson {
    fn from_dir(dir: &Path) -> Self {
        let meta = Meta::read(&dir.join("meta.json")).ok();
        Self {
            dir: dir.to_path_buf(),
            started_at: meta.as_ref().and_then(|m| rfc3339(m.started_at)),
            duration_secs: meta.as_ref().map(|m| m.duration_secs()),
            artifacts: ArtifactsJson::from_dir(dir),
        }
    }
}

#[derive(Serialize)]
struct ArtifactsJson {
    #[serde(rename = "mic.wav")]
    mic_wav: bool,
    #[serde(rename = "system.wav")]
    system_wav: bool,
    #[serde(rename = "live.jsonl")]
    live_jsonl: bool,
    #[serde(rename = "transcript.json")]
    transcript_json: bool,
    #[serde(rename = "mic_aec.wav")]
    mic_aec_wav: bool,
}

impl ArtifactsJson {
    fn from_dir(dir: &Path) -> Self {
        Self {
            mic_wav: dir.join(meta::MIC_NAME).is_file(),
            system_wav: dir.join(meta::SYSTEM_NAME).is_file(),
            live_jsonl: dir.join(live::FILENAME).is_file(),
            transcript_json: dir.join("transcript.json").is_file(),
            mic_aec_wav: dir.join("mic_aec.wav").is_file(),
        }
    }
}

fn rfc3339(unix: f64) -> Option<String> {
    if !unix.is_finite() {
        return None;
    }
    let secs = unix.floor() as i64;
    let nanos = ((unix - secs as f64) * 1e9)
        .round()
        .clamp(0.0, 999_999_999.0) as u32;
    DateTime::<Utc>::from_timestamp(secs, nanos)
        .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn print_human(value: &RecordingsJson) {
    if value.recordings.is_empty() {
        println!("no recordings");
        return;
    }
    for recording in &value.recordings {
        let when = recording.started_at.as_deref().unwrap_or("not finalised");
        let duration = recording
            .duration_secs
            .map(|s| format!("{s:.1}s"))
            .unwrap_or_else(|| "-".into());
        let artifacts = artifact_names(&recording.artifacts);
        println!(
            "{}  {when}  {duration}  {}",
            recording.dir.display(),
            if artifacts.is_empty() {
                "no artifacts".to_string()
            } else {
                artifacts.join(" ")
            }
        );
    }
}

fn artifact_names(artifacts: &ArtifactsJson) -> Vec<&'static str> {
    let mut names = Vec::new();
    if artifacts.mic_wav {
        names.push("mic.wav");
    }
    if artifacts.system_wav {
        names.push("system.wav");
    }
    if artifacts.live_jsonl {
        names.push("live.jsonl");
    }
    if artifacts.transcript_json {
        names.push("transcript.json");
    }
    if artifacts.mic_aec_wav {
        names.push("mic_aec.wav");
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "jotter-recordings-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"").unwrap();
    }

    #[test]
    fn lists_newest_first_and_reports_which_artifacts_exist() {
        let root = scratch("root");
        let older = root.join("older");
        let newer = root.join("newer");
        std::fs::create_dir_all(&older).unwrap();
        std::fs::create_dir_all(&newer).unwrap();
        touch(&older.join("mic.wav"));
        Meta {
            started_at: 1_000.0,
            ended_at: 1_010.0,
            mic: None,
            system: None,
            aec: None,
            transcript: None,
            diarization: None,
            live: None,
        }
        .write(&older.join("meta.json"))
        .unwrap();
        touch(&newer.join("system.wav"));
        touch(&newer.join(live::FILENAME));
        touch(&newer.join("transcript.json"));
        Meta {
            started_at: 2_000.0,
            ended_at: 2_030.5,
            mic: None,
            system: None,
            aec: None,
            transcript: None,
            diarization: None,
            live: None,
        }
        .write(&newer.join("meta.json"))
        .unwrap();
        // Not a recording: no artifact, so it must not appear.
        std::fs::create_dir_all(root.join("notes")).unwrap();

        let listed = list(&root, None, 10).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].dir, newer);
        assert!(listed[0].artifacts.system_wav);
        assert!(listed[0].artifacts.live_jsonl);
        assert!(listed[0].artifacts.transcript_json);
        assert!(!listed[0].artifacts.mic_wav);
        assert!(!listed[0].artifacts.mic_aec_wav);
        assert_eq!(listed[0].duration_secs, Some(30.5));
        assert!(
            listed[0]
                .started_at
                .as_deref()
                .unwrap()
                .starts_with("1970-01-01T00:33:20")
        );
        assert_eq!(listed[1].dir, older);
        assert!(listed[1].artifacts.mic_wav);
    }

    #[test]
    fn limit_keeps_the_newest_and_an_outside_session_dir_is_included() {
        let root = scratch("limit");
        for name in ["a", "b", "c"] {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            touch(&dir.join("mic.wav"));
        }
        let outside = scratch("outside");
        touch(&outside.join("mic.wav"));

        let listed = list(&root, Some(&outside), 2).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(
            listed.iter().any(|r| same_dir(&r.dir, &outside)),
            "the active session dir is part of the list even outside the root"
        );
    }
}
