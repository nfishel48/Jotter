//! `jotter context`: what has been said so far.
//!
//! An agent polling a meeting should not have to know whether the recording is
//! still going. This command picks the directory, prefers the final
//! `transcript.json` once the recording has finished, and otherwise reads
//! `live.jsonl` through the library so a half-written line is never a segment.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use clap::Args;
use serde::Serialize;

use jotter::audio::live::{self, Cursor};
use jotter::audio::meta::Meta;
use jotter::audio::transcript::{self, Transcript};

use crate::output::{CliError, ErrorKind, Output};
use crate::session::{self, Lookup, Phase, SessionFile, SessionState};

/// How often `--follow` looks for new lines. Short enough that a sentence
/// appears as soon as the recogniser has finished it; the read itself is a
/// seek to the cursor, so the cost of checking early is nothing.
const FOLLOW_POLL: Duration = Duration::from_millis(200);

#[derive(Args)]
pub struct ContextArgs {
    /// Resume a live read from a cursor `context` printed earlier. Ignored
    /// when the final transcript is served — that file is the whole answer.
    #[arg(long, value_name = "CURSOR")]
    since: Option<String>,

    /// Only segments whose end is within this many seconds of the latest
    /// point on the recording's timeline.
    #[arg(long, value_name = "SECS")]
    last: Option<f64>,

    /// Read this recording instead of the active session, or the most recent
    /// one when nothing is recording.
    #[arg(long, value_name = "DIR")]
    dir: Option<PathBuf>,

    /// Print one JSON object per new live segment until the session ends.
    /// Needs an active session. Line-oriented even without `--json`.
    #[arg(long)]
    follow: bool,
}

/// `jotter context`.
pub fn run(args: ContextArgs, out: Output) -> Result<(), CliError> {
    if let Some(secs) = args.last
        && (!secs.is_finite() || secs < 0.0)
    {
        return Err(CliError::new(
            ErrorKind::InvalidArgument,
            "--last takes a non-negative number of seconds",
        ));
    }

    if args.follow {
        return follow(&args);
    }

    let (dir, capturing) = resolve(args.dir.as_deref())?;
    let now = timeline_now(&dir, capturing);
    let value = snapshot(&dir, capturing, args.since.as_deref(), args.last, now)?;
    out.emit(&value, print_human);
    Ok(())
}

/// Where to read, and whether a recorder is still capturing into it.
///
/// `--dir` wins when it is passed: a caller who named a recording meant that
/// one, even if a session is running elsewhere. Otherwise the active session,
/// otherwise the newest directory under [`jotter::config::recordings_root`].
fn resolve(explicit: Option<&Path>) -> Result<(PathBuf, bool), CliError> {
    let active = active_session()?;
    if let Some(dir) = explicit {
        if !dir.is_dir() {
            return Err(not_found(&format!("no recording at {}", dir.display())));
        }
        let dir = session::absolute(dir.to_path_buf());
        let capturing = active.as_ref().is_some_and(|s| capturing_into(s, &dir));
        return Ok((dir, capturing));
    }
    if let Some(session) = active {
        let capturing = capturing_phase(&session);
        return Ok((session.dir, capturing));
    }
    newest_recording().ok_or_else(|| {
        not_found("no recording to read; start one with `jotter start`, or pass --dir")
    })
}

fn active_session() -> Result<Option<SessionState>, CliError> {
    Ok(match SessionFile::default_location().lookup()? {
        Lookup::Active(session) => Some(session),
        Lookup::Idle | Lookup::Stale(_) => None,
    })
}

/// Capture is still open: the recorder has not finalised `meta.json` yet.
/// `Stopping` counts — the process is alive and may still append a line.
fn capturing_phase(session: &SessionState) -> bool {
    matches!(
        session.state,
        Phase::Starting | Phase::Recording | Phase::Stopping
    )
}

fn capturing_into(session: &SessionState, dir: &Path) -> bool {
    capturing_phase(session) && same_dir(&session.dir, dir)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    norm(a) == norm(b)
}

/// The newest recording directory under the recordings root, if there is one.
///
/// A directory counts when it holds a recording artifact. Empty folders left
/// by a failed start are not meetings.
fn newest_recording() -> Option<(PathBuf, bool)> {
    let root = jotter::config::recordings_root();
    let entries = std::fs::read_dir(&root).ok()?;
    let mut found: Vec<(f64, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir() && crate::recordings::is_recording(p))
        .map(|p| (crate::recordings::recency(&p), p))
        .collect();
    found.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.cmp(&a.1)));
    found.into_iter().next().map(|(_, dir)| (dir, false))
}

/// Seconds on the mic timeline "now" is, so `--last` means the tail of the
/// meeting rather than the tail of whatever has been transcribed.
fn timeline_now(dir: &Path, capturing: bool) -> Option<f64> {
    if capturing {
        return active_session()
            .ok()
            .flatten()
            .filter(|s| same_dir(&s.dir, dir))
            .map(|s| session::elapsed(&s.started_at));
    }
    Meta::read(&dir.join("meta.json"))
        .ok()
        .map(|m| m.duration_secs())
}

fn snapshot(
    dir: &Path,
    capturing: bool,
    since: Option<&str>,
    last: Option<f64>,
    now: Option<f64>,
) -> Result<ContextJson, CliError> {
    // Finished means the recorder is not still writing this directory, or it
    // has already finalised `meta.json` (the offline passes may still be
    // running). An old `transcript.json` in a directory that is being recorded
    // into again must not hide the live lines.
    let meta_ended = Meta::read(&dir.join("meta.json")).is_ok();
    let finished = !capturing || meta_ended;
    let transcript_path = dir.join("transcript.json");
    if finished && !capturing && transcript_path.is_file() {
        let transcript = Transcript::read(&transcript_path)?;
        let mut segments = transcript.segments;
        segments = filter_last(segments, last, now);
        return Ok(ContextJson {
            dir: dir.to_path_buf(),
            source: "transcript",
            complete: true,
            cursor: None,
            segments: segments.into_iter().map(SegmentJson::from).collect(),
        });
    }

    let cursor = since.map(parse_cursor).transpose()?;
    let chunk = read_live(dir, cursor)?;
    let mut segments = chunk.segments;
    segments = filter_last(segments, last, now);
    Ok(ContextJson {
        dir: dir.to_path_buf(),
        source: "live",
        complete: false,
        cursor: Some(chunk.cursor.to_string()),
        segments: segments.into_iter().map(SegmentJson::from).collect(),
    })
}

fn parse_cursor(raw: &str) -> Result<Cursor, CliError> {
    Cursor::from_str(raw).map_err(|_| {
        CliError::new(
            ErrorKind::InvalidArgument,
            format!("{raw:?} is not a live cursor (expected live:<hex>)"),
        )
    })
}

fn read_live(dir: &Path, cursor: Option<Cursor>) -> Result<live::LiveChunk, CliError> {
    live::read(dir, cursor).map_err(|e| {
        // A cursor past the end, or a line that is not a segment, is the
        // caller's input. Anything else is the disk.
        if e.kind() == std::io::ErrorKind::InvalidData {
            CliError::new(ErrorKind::InvalidArgument, e.to_string())
        } else {
            e.into()
        }
    })
}

fn filter_last(
    mut segments: Vec<transcript::Segment>,
    last: Option<f64>,
    now: Option<f64>,
) -> Vec<transcript::Segment> {
    let Some(secs) = last else {
        return segments;
    };
    let reference = now
        .filter(|t| t.is_finite())
        .or_else(|| segments.iter().map(|s| s.end).reduce(f64::max));
    let Some(reference) = reference else {
        return segments;
    };
    let horizon = reference - secs;
    segments.retain(|s| s.end >= horizon);
    segments
}

/// `--follow`: one JSON object per new segment, then exit once the recorder
/// is gone and `live.jsonl` has been drained.
///
/// Always JSON, even without `--json`. A human flag would only exist to be
/// wrong: the point of following is a program reading lines as they appear.
fn follow(args: &ContextArgs) -> Result<(), CliError> {
    let Some(session) = active_session()? else {
        return Err(session::no_session());
    };
    if let Some(dir) = &args.dir
        && !same_dir(dir, &session.dir)
    {
        return Err(CliError::new(
            ErrorKind::InvalidArgument,
            "--follow reads the active session; --dir names a different recording",
        ));
    }

    let dir = session.dir.clone();
    let session_id = session.session_id;
    let mut cursor = args.since.as_deref().map(parse_cursor).transpose()?;

    loop {
        let chunk = read_live(&dir, cursor)?;
        emit_new(&chunk, args.last, timeline_now(&dir, true))?;
        cursor = Some(chunk.cursor);
        if !still_capturing(&session_id, &dir)? {
            // The recorder may have appended its last line as it exited.
            let tail = read_live(&dir, cursor)?;
            emit_new(&tail, args.last, timeline_now(&dir, false))?;
            break;
        }
        std::thread::sleep(FOLLOW_POLL);
    }
    Ok(())
}

fn still_capturing(session_id: &str, dir: &Path) -> Result<bool, CliError> {
    Ok(match SessionFile::default_location().lookup()? {
        Lookup::Active(session) => {
            session.session_id == session_id && capturing_into(&session, dir)
        }
        Lookup::Idle | Lookup::Stale(_) => false,
    })
}

fn emit_new(chunk: &live::LiveChunk, last: Option<f64>, now: Option<f64>) -> Result<(), CliError> {
    let cursor = chunk.cursor.to_string();
    let segments = filter_last(chunk.segments.clone(), last, now);
    for segment in segments {
        print_line(&FollowLine {
            cursor: cursor.clone(),
            segment: SegmentJson::from(segment),
        });
    }
    Ok(())
}

fn print_line(value: &impl Serialize) {
    let line = serde_json::to_string(value).expect("follow lines always serialise");
    println!("{line}");
}

fn print_human(value: &ContextJson) {
    let complete = if value.complete {
        "complete"
    } else {
        "still open"
    };
    println!(
        "{} of {} ({}, {} segments)",
        value.source,
        value.dir.display(),
        complete,
        value.segments.len()
    );
    if let Some(cursor) = &value.cursor {
        println!("  cursor {cursor}");
    }
    for segment in &value.segments {
        let speaker = segment
            .speaker
            .as_deref()
            .map(|s| format!(" {s}"))
            .unwrap_or_default();
        println!(
            "  {:>7.2}–{:<7.2} {}{speaker}: {}",
            segment.start, segment.end, segment.track, segment.text
        );
    }
}

fn not_found(message: &str) -> CliError {
    CliError::new(ErrorKind::NotFound, message)
}

#[derive(Serialize)]
struct ContextJson {
    dir: PathBuf,
    source: &'static str,
    complete: bool,
    /// `null` when `source` is `transcript`: a finished transcript has no
    /// resume point. A live cursor otherwise, including `live:0` before the
    /// first line.
    cursor: Option<String>,
    segments: Vec<SegmentJson>,
}

/// One segment, in the shape both `context` and `--follow` print.
///
/// `speaker` is omitted rather than null while nobody has labelled it, the
/// same way `transcript.json` writes the field.
#[derive(Serialize)]
struct SegmentJson {
    track: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    speaker: Option<String>,
    start: f64,
    end: f64,
    text: String,
}

impl From<transcript::Segment> for SegmentJson {
    fn from(segment: transcript::Segment) -> Self {
        Self {
            track: segment.track.as_str().to_string(),
            speaker: segment.speaker,
            start: segment.start,
            end: segment.end,
            text: segment.text,
        }
    }
}

/// A `--follow` line. `cursor` is where this read ended, not a private offset
/// for this one segment: several lines from the same read share it, and
/// `--since` that value skips all of them.
#[derive(Serialize)]
struct FollowLine {
    cursor: String,
    #[serde(flatten)]
    segment: SegmentJson,
}

#[cfg(test)]
mod tests {
    use super::*;
    use jotter::audio::transcript::{Segment, Track};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "jotter-context-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn segment(start: f64, end: f64, text: &str) -> Segment {
        Segment {
            start,
            end,
            track: Track::Mic,
            speaker: None,
            text: text.into(),
        }
    }

    fn write_live(dir: &Path, lines: &[&str]) {
        let body: String = lines
            .iter()
            .map(|text| {
                format!(
                    "{{\"v\":1,\"track\":\"mic\",\"start\":0.0,\"end\":1.0,\"text\":{text}}}\n",
                    text = serde_json::to_string(text).unwrap()
                )
            })
            .collect();
        std::fs::write(dir.join(live::FILENAME), body).unwrap();
    }

    #[test]
    fn a_finished_transcript_wins_over_live_lines() {
        let dir = scratch("finished");
        write_live(&dir, &["rough"]);
        Transcript::new("model", vec![segment(0.0, 1.5, "final")])
            .write(&dir.join("transcript.json"))
            .unwrap();
        Meta {
            started_at: 10.0,
            ended_at: 12.0,
            mic: None,
            system: None,
            aec: None,
            transcript: None,
            diarization: None,
            live: None,
        }
        .write(&dir.join("meta.json"))
        .unwrap();

        let value = snapshot(&dir, false, None, None, Some(2.0)).unwrap();
        assert_eq!(value.source, "transcript");
        assert!(value.complete);
        assert!(value.cursor.is_none());
        assert_eq!(value.segments.len(), 1);
        assert_eq!(value.segments[0].text, "final");
    }

    #[test]
    fn an_open_recording_stays_on_the_live_file_even_if_an_old_transcript_is_there() {
        let dir = scratch("open");
        write_live(&dir, &["now"]);
        Transcript::new("model", vec![segment(0.0, 1.0, "old")])
            .write(&dir.join("transcript.json"))
            .unwrap();

        let value = snapshot(&dir, true, None, None, Some(5.0)).unwrap();
        assert_eq!(value.source, "live");
        assert!(!value.complete);
        assert!(value.cursor.as_deref().unwrap().starts_with("live:"));
        assert_eq!(value.segments[0].text, "now");
    }

    #[test]
    fn since_returns_only_lines_past_the_cursor() {
        let dir = scratch("since");
        std::fs::write(
            dir.join(live::FILENAME),
            "{\"v\":1,\"track\":\"mic\",\"start\":0.0,\"end\":1.0,\"text\":\"one\"}\n\
             {\"v\":1,\"track\":\"system\",\"start\":1.0,\"end\":2.0,\"text\":\"two\"}\n",
        )
        .unwrap();

        let first = snapshot(&dir, true, None, None, None).unwrap();
        assert_eq!(first.segments.len(), 2);
        let cursor = first.cursor.clone().unwrap();

        std::fs::write(
            dir.join(live::FILENAME),
            "{\"v\":1,\"track\":\"mic\",\"start\":0.0,\"end\":1.0,\"text\":\"one\"}\n\
             {\"v\":1,\"track\":\"system\",\"start\":1.0,\"end\":2.0,\"text\":\"two\"}\n\
             {\"v\":1,\"track\":\"mic\",\"start\":2.0,\"end\":3.0,\"text\":\"three\"}\n",
        )
        .unwrap();
        let next = snapshot(&dir, true, Some(&cursor), None, None).unwrap();
        assert_eq!(next.segments.len(), 1);
        assert_eq!(next.segments[0].text, "three");
        assert_eq!(next.segments[0].track, "mic");
    }

    #[test]
    fn last_keeps_segments_whose_end_falls_in_the_window() {
        let dir = scratch("last");
        Transcript::new(
            "model",
            vec![segment(0.0, 10.0, "early"), segment(40.0, 50.0, "late")],
        )
        .write(&dir.join("transcript.json"))
        .unwrap();

        let value = snapshot(&dir, false, None, Some(15.0), Some(60.0)).unwrap();
        assert_eq!(value.segments.len(), 1);
        assert_eq!(value.segments[0].text, "late");
    }

    #[test]
    fn a_bad_cursor_is_an_invalid_argument() {
        let dir = scratch("cursor");
        match snapshot(&dir, true, Some("nope"), None, None) {
            Err(err) => assert_eq!(err.kind, ErrorKind::InvalidArgument),
            Ok(_) => panic!("a cursor that is not live:<hex> must be rejected"),
        }
    }

    #[test]
    fn speaker_is_kept_when_the_transcript_has_one() {
        let dir = scratch("speaker");
        let mut labelled = segment(1.0, 2.0, "morning");
        labelled.track = Track::System;
        labelled.speaker = Some("speaker_01".into());
        Transcript::new("model", vec![labelled])
            .write(&dir.join("transcript.json"))
            .unwrap();

        let value = snapshot(&dir, false, None, None, None).unwrap();
        assert_eq!(value.segments[0].speaker.as_deref(), Some("speaker_01"));
        assert_eq!(value.segments[0].track, "system");
    }
}
