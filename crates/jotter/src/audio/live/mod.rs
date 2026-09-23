//! A transcript of the meeting while it is still happening.
//!
//! The final transcript is an offline pass over the finished recording, and it
//! is the better one: it runs after echo cancellation, with the whole meeting
//! to work from. This is the rough one, written as the words are said, so a
//! meeting can be followed without waiting for it to end.
//!
//! Turn it on with [`LiveConfig`] on [`RecordConfig`](super::RecordConfig), and
//! it appends to `live.jsonl` in the recording directory, one JSON object per
//! line, flushed as each line is finished:
//!
//! ```json
//! {"v":1,"track":"mic","start":1.25,"end":3.8,"text":"..."}
//! ```
//!
//! `start` and `end` are seconds on the **mic track's timeline**, the same
//! convention as `transcript.json`, so the two files can be read against each
//! other. Lines are only ever appended, each one a segment the detector has
//! finished with, and they come out in start order: a segment waits until
//! nothing earlier can still arrive. Read them with [`read`] rather than
//! parsing the file — it is the only reader that knows a line the worker is
//! half way through writing is not a line yet.
//!
//! # Echo
//!
//! On speakers the far end leaks back into the microphone, and the recogniser
//! would transcribe it twice. A mic segment that overlaps a system segment and
//! mostly repeats its words is dropped as the system track heard again; the
//! decision and its limits are [`is_echo`].
//!
//! # When it does not run
//!
//! Live transcription declining never stops the recording. A missing model, a
//! build without the `transcribe` feature, or a failure once it has started are
//! all reported by [`LiveTranscriber::status`] while the recording runs and
//! recorded in `meta.json`'s `live` block when it stops, so a recording always
//! says what happened to its live transcript.
//!
//! It is not its own cargo feature. It adds no dependency and no model the
//! transcription pass does not already need, so a flag would guard nothing —
//! unlike `diarize`, whose cost is models of its own. It is part of
//! `transcribe`, and a build without that still has this module: the config,
//! the status and [`read`] are plain data, and asking for live transcription
//! simply declines as unavailable.
//!
//! # Latency
//!
//! A segment cannot be final until the speaker has paused — the detector ends
//! one after half a second of silence — and it then waits to be decoded and,
//! for a mic segment, for the other track to have passed it so the echo check
//! has seen everything it needs. A second or two behind the room is the normal
//! case; longer means the recogniser fell behind, which is counted rather than
//! hidden.

mod echo;
#[cfg(feature = "transcribe")]
mod engine;
#[cfg(feature = "transcribe")]
mod merge;

use std::fmt;
use std::io::{self, Seek};
use std::path::Path;
use std::str::FromStr;

use serde::Deserialize;

use crate::audio::stage::DeclineReason;
use crate::audio::transcript::{Segment, Track};

#[cfg(feature = "transcribe")]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "transcribe")]
use std::sync::mpsc::{SyncSender, TrySendError};
#[cfg(feature = "transcribe")]
use std::sync::{Arc, Mutex};

#[cfg(feature = "transcribe")]
use engine::Engine;

pub use echo::is_echo;

/// Filename of the live transcript, beside the audio.
pub const FILENAME: &str = "live.jsonl";

/// [`Cursor`] strings start with this, so a cursor from another file fails to
/// parse rather than being read as a byte offset by coincidence.
const CURSOR_PREFIX: &str = "live:";

/// Bumped when the shape of a line changes in a way a reader would notice.
pub const FORMAT_VERSION: u32 = 1;

/// Bumped when the same audio would come out as different lines.
pub const LIVE_VERSION: u32 = 1;

/// Chunks of audio waiting to be transcribed.
///
/// A few tens of seconds at the buffer sizes cpal actually delivers, which is
/// enough to ride out the model loading and a decode that runs long. Past it
/// the audio is dropped and counted: the alternative is the callback blocking,
/// and a stalled callback is a dropout in the recording itself.
#[cfg(feature = "transcribe")]
const QUEUE_CAPACITY: usize = 4096;

/// What to transcribe while recording, and with what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveConfig {
    /// Catalogue id of the recogniser, or `None` for the default. The same
    /// choice the offline pass makes, resolved the same way.
    pub model: Option<String>,
    /// Which of the recorded tracks to transcribe. A track named here that is
    /// not being recorded is simply absent.
    pub tracks: super::Sources,
}

impl Default for LiveConfig {
    fn default() -> Self {
        Self {
            model: None,
            tracks: super::Sources::Both,
        }
    }
}

/// Why live transcription never started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveDecline {
    /// This build has no transcription support, so there is nothing to run.
    Unavailable,
    /// The recogniser or the voice-activity model is not on disk.
    ModelMissing {
        model_id: &'static str,
        files: usize,
    },
    /// Live transcription was asked for, but no recorded track was named.
    NoTracks,
}

impl DeclineReason for LiveDecline {
    fn kind(&self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::ModelMissing { .. } => "model_missing",
            Self::NoTracks => "no_tracks",
        }
    }
}

impl fmt::Display for LiveDecline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => write!(f, "this build has no transcription support"),
            Self::ModelMissing { model_id, files } => write!(
                f,
                "the {model_id} model is not downloaded ({files} file(s) missing)"
            ),
            Self::NoTracks => write!(f, "none of the tracks being recorded were asked for"),
        }
    }
}

/// Why live transcription started and then stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveFailure {
    /// The recogniser or the detector could not be built.
    Engine,
    /// `live.jsonl` could not be created or written.
    Io,
}

impl LiveFailure {
    /// A stable name for `meta.json` and [`LiveStatus::kind`].
    pub fn kind(self) -> &'static str {
        match self {
            Self::Engine => "engine",
            Self::Io => "io",
        }
    }
}

/// Where live transcription has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveState {
    /// Asked for, and the worker is still loading its model.
    Starting,
    /// Transcribing. Lines are being appended.
    Running,
    /// Looked, and declined. The recording is unaffected.
    Declined,
    /// Started, and then broke. Lines written before it broke are kept.
    Failed,
}

/// A snapshot of live transcription, for whoever is driving the recording.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LiveStatus {
    pub state: LiveState,
    /// A stable name for why, when [`state`](Self::state) is `Declined` or
    /// `Failed`: a [`LiveDecline::kind`] or a [`LiveFailure::kind`]. Absent
    /// otherwise. Owned, because a status read back from `meta.json` has no
    /// static string to point at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The same reason as a sentence, for whoever is reading it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Lines written so far.
    pub segments: u32,
    /// Where the latest line ended, in seconds on the mic timeline.
    pub last_end_secs: Option<f64>,
    /// Audio the worker never saw because it fell behind. The recording is
    /// unaffected; the live transcript has a gap.
    pub dropped_secs: f32,
}

/// Live transcription for one recording.
///
/// Started by [`super::start`] when a [`RecordConfig`](super::RecordConfig)
/// asks for it, and stopped by [`RecordingHandle::stop`](super::RecordingHandle::stop).
/// It can also be driven directly — [`feed`](Self::feed) hands back a
/// [`LiveFeed`] per track — which is how a recording already on disk is
/// replayed through the same path.
pub struct LiveTranscriber {
    #[cfg(feature = "transcribe")]
    inner: Engine,
    #[cfg(not(feature = "transcribe"))]
    decline: LiveDecline,
}

impl LiveTranscriber {
    /// Begin transcribing into `dir`. Never fails: a reason not to run is a
    /// decline, reported by [`status`](Self::status) and recorded by
    /// [`stop`](Self::stop), because the recording this serves must not depend
    /// on it.
    #[cfg_attr(not(feature = "transcribe"), allow(unused_variables))]
    pub fn start(dir: &Path, config: &LiveConfig, sources: super::Sources) -> Self {
        #[cfg(feature = "transcribe")]
        {
            if sources.intersect(config.tracks).is_none() {
                return Self {
                    inner: Engine::declined_only(LiveDecline::NoTracks),
                };
            }
            Self {
                inner: Engine::start(dir, config, sources),
            }
        }
        #[cfg(not(feature = "transcribe"))]
        {
            let _ = dir;
            Self {
                decline: LiveDecline::Unavailable,
            }
        }
    }

    /// A feed for one recorded track, or `None` when this track is not being
    /// transcribed — declined, or not asked for.
    ///
    /// `sample_rate` is the track's own rate. The feed counts samples from the
    /// first push, so the two tracks line up by when their audio actually
    /// started.
    #[cfg_attr(not(feature = "transcribe"), allow(unused_variables))]
    pub fn feed(&self, track: Track, sample_rate: u32) -> Option<LiveFeed> {
        #[cfg(feature = "transcribe")]
        {
            self.inner.feed(track, sample_rate)
        }
        #[cfg(not(feature = "transcribe"))]
        {
            None
        }
    }

    /// How it is getting on. Cheap, and safe to call from anywhere.
    pub fn status(&self) -> LiveStatus {
        #[cfg(feature = "transcribe")]
        {
            self.inner.status()
        }
        #[cfg(not(feature = "transcribe"))]
        {
            LiveStatus {
                state: LiveState::Declined,
                kind: Some(self.decline.kind().to_string()),
                reason: Some(self.decline.to_string()),
                segments: 0,
                last_end_secs: None,
                dropped_secs: 0.0,
            }
        }
    }

    /// Transcribe whatever audio is still queued, then stop.
    ///
    /// Blocks until the tail is done, but only up to a bound: a worker that
    /// does not finish is abandoned and the recording moves on, with
    /// `drain_timed_out` set in the record it returns. The final transcript
    /// covers the same audio afterwards.
    pub fn stop(self) -> crate::audio::meta::LiveInfo {
        #[cfg(feature = "transcribe")]
        {
            self.inner.stop()
        }
        #[cfg(not(feature = "transcribe"))]
        {
            crate::audio::meta::LiveInfo {
                version: LIVE_VERSION,
                declined: Some(self.decline.kind().into()),
                ..crate::audio::meta::LiveInfo::default()
            }
        }
    }
}

/// One track's way in.
///
/// Held by whoever produces the audio — a capture callback, or something
/// replaying a file — and pushed to as samples arrive. Pushing never blocks
/// and never fails the caller: a worker that has fallen behind or already
/// stopped loses the audio and counts it, which is the only outcome a realtime
/// thread can afford.
#[cfg(feature = "transcribe")]
pub struct LiveFeed {
    tx: SyncSender<Chunk>,
    track: Track,
    sample_rate: u32,
    /// Samples offered so far, whether or not the worker took them. What keeps
    /// the timeline honest across a dropped chunk.
    position: u64,
    /// The worker's drop counter for this track: an atomic the callback holds
    /// directly, so counting a drop never takes a lock.
    dropped: Arc<AtomicU64>,
}

/// A stretch of one track, as the worker receives it.
#[cfg(feature = "transcribe")]
pub(crate) struct Chunk {
    pub(crate) track: Track,
    pub(crate) sample_rate: u32,
    /// Samples of this track before this chunk. The worker fills the gap with
    /// silence, so a drop moves the timeline on rather than sliding it back.
    pub(crate) position: u64,
    /// When the callback that produced this chunk fired, if anyone knows.
    /// `None` from a caller replaying a file, which has no callback instants.
    pub(crate) origin_nanos: Option<u128>,
    pub(crate) samples: Vec<i16>,
}

#[cfg(feature = "transcribe")]
pub(crate) type Shared = Arc<Mutex<engine::Progress>>;

#[cfg(feature = "transcribe")]
impl LiveFeed {
    /// Hand samples over without waiting.
    ///
    /// `origin_nanos` is the instant the first of them was captured, when the
    /// caller has one. The worker only needs it once per track — it is how the
    /// two tracks land on one timeline — so later calls may pass `None`.
    ///
    /// Returns whether the worker took them. `false` means it was behind or
    /// gone, and the samples were counted as dropped.
    pub fn try_push(&mut self, samples: Vec<i16>, origin_nanos: Option<u128>) -> bool {
        let chunk = self.chunk(samples, origin_nanos);
        match self.tx.try_send(chunk) {
            Ok(()) => true,
            Err(TrySendError::Full(chunk)) => {
                self.dropped(chunk.samples.len());
                false
            }
            // The worker has stopped. Nothing to count: it is not behind, it
            // is gone, and the caller is about to find out from `status`.
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    /// Hand samples over, waiting until the worker has room.
    ///
    /// For a caller replaying audio faster than real time, where dropping it
    /// would change the result. A capture callback must use [`try_push`](Self::try_push):
    /// this one can block.
    pub fn push(&mut self, samples: Vec<i16>, origin_nanos: Option<u128>) {
        let chunk = self.chunk(samples, origin_nanos);
        // Gone rather than behind, as in `try_push`.
        let _ = self.tx.send(chunk);
    }

    fn chunk(&mut self, samples: Vec<i16>, origin_nanos: Option<u128>) -> Chunk {
        let position = self.position;
        self.position += samples.len() as u64;
        Chunk {
            track: self.track,
            sample_rate: self.sample_rate,
            position,
            origin_nanos,
            samples,
        }
    }

    fn dropped(&self, samples: usize) {
        self.dropped.fetch_add(samples as u64, Ordering::Relaxed);
    }
}

/// One track's way in, in a build with nothing to feed: it cannot be
/// constructed, and the methods exist so callers compile either way.
#[cfg(not(feature = "transcribe"))]
pub struct LiveFeed {
    _private: (),
}

#[cfg(not(feature = "transcribe"))]
impl LiveFeed {
    /// Hand samples over without waiting. See the `transcribe` build.
    pub fn try_push(&mut self, samples: Vec<i16>, origin_nanos: Option<u128>) -> bool {
        let _ = (samples, origin_nanos);
        false
    }

    /// Hand samples over, waiting until the worker has room.
    pub fn push(&mut self, samples: Vec<i16>, origin_nanos: Option<u128>) {
        let _ = (samples, origin_nanos);
    }
}

/// Where a reader has got to in `live.jsonl`.
///
/// Opaque on purpose: it is a byte offset, and a caller that computed one
/// would be coupling itself to the file's layout. Print it and parse it back;
/// do not take it apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Cursor(u64);

impl Cursor {
    /// The start of the file.
    pub const START: Cursor = Cursor(0);
}

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{CURSOR_PREFIX}{:x}", self.0)
    }
}

impl FromStr for Cursor {
    type Err = io::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex = s
            .strip_prefix(CURSOR_PREFIX)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not a live cursor"))?;
        let offset = u64::from_str_radix(hex, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not a live cursor"))?;
        Ok(Cursor(offset))
    }
}

/// What [`read`] found since the cursor it was given.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveChunk {
    pub segments: Vec<Segment>,
    /// Where to read from next time. Unchanged when nothing new was there,
    /// including when the file does not exist yet.
    pub cursor: Cursor,
}

/// The lines appended to `dir`'s `live.jsonl` since `cursor`.
///
/// `None` reads from the start. A file that does not exist yet is an empty
/// chunk at the same cursor, not an error: a reader polling a recording that
/// has only just started is the ordinary case, and the file appears when the
/// first line does.
///
/// A trailing line with no newline is the worker mid-write, and it is never
/// returned. The cursor stops before it, so the next call reads it whole once
/// the worker has finished it.
pub fn read(dir: &Path, cursor: Option<Cursor>) -> io::Result<LiveChunk> {
    let cursor = cursor.unwrap_or(Cursor::START);
    let path = dir.join(FILENAME);

    let mut file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(LiveChunk {
                segments: Vec::new(),
                cursor,
            });
        }
        Err(e) => return Err(e),
    };

    let len = file.seek(io::SeekFrom::End(0))?;
    if cursor.0 > len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cursor is past the end of live.jsonl",
        ));
    }
    file.seek(io::SeekFrom::Start(cursor.0))?;

    // One read rather than a buffered line loop: the file is a meeting's worth
    // of short lines, and reading it whole is what makes "up to the last
    // newline" a single decision instead of a state machine.
    let mut buf = Vec::new();
    io::Read::read_to_end(&mut file, &mut buf)?;

    let boundary = match buf.iter().rposition(|&b| b == b'\n') {
        Some(at) => at + 1,
        None => {
            return Ok(LiveChunk {
                segments: Vec::new(),
                cursor,
            });
        }
    };

    let mut segments = Vec::new();
    for (line_index, line) in buf[..boundary].split(|&b| b == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let text = std::str::from_utf8(line).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "live.jsonl is not valid UTF-8")
        })?;
        let parsed: Line = serde_json::from_str(text).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("live.jsonl line {} is not a segment", line_index + 1),
            )
        })?;
        if parsed.v != FORMAT_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "live.jsonl is format version {}, not {FORMAT_VERSION}",
                    parsed.v
                ),
            ));
        }
        segments.push(parsed.segment);
    }

    Ok(LiveChunk {
        segments,
        cursor: Cursor(cursor.0 + boundary as u64),
    })
}

/// A line, as it sits on disk. `segment` carries the fields a caller sees;
/// `v` is the format's, and stays out of it.
#[derive(Deserialize)]
struct Line {
    v: u32,
    #[serde(flatten)]
    segment: Segment,
}

#[cfg(feature = "transcribe")]
pub(crate) fn track_index(track: Track) -> usize {
    match track {
        Track::Mic => 0,
        Track::System => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("jotter-live-{name}-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn seg(track: Track, start: f64, end: f64, text: &str) -> Segment {
        Segment {
            start,
            end,
            track,
            speaker: None,
            text: text.into(),
        }
    }

    #[test]
    fn a_missing_file_is_an_empty_chunk_at_the_same_cursor() {
        let dir = scratch("missing");
        let chunk = read(&dir, None).unwrap();
        assert!(chunk.segments.is_empty());
        assert_eq!(chunk.cursor, Cursor::START);

        // A cursor the caller already holds comes back unchanged, so a reader
        // polling before the file exists does not lose its place.
        let held = "live:2a".parse().unwrap();
        assert_eq!(read(&dir, Some(held)).unwrap().cursor, held);
    }

    #[test]
    fn a_partial_trailing_line_is_not_returned() {
        let dir = scratch("partial");
        let whole = "{\"v\":1,\"track\":\"mic\",\"start\":0.0,\"end\":1.5,\"text\":\"hello\"}\n";
        let partial = "{\"v\":1,\"track\":\"system\",\"start\":2.0,\"end\":3.5,\"text\":\"wor";
        std::fs::write(dir.join(FILENAME), format!("{whole}{partial}")).unwrap();

        let chunk = read(&dir, None).unwrap();
        assert_eq!(chunk.segments, vec![seg(Track::Mic, 0.0, 1.5, "hello")]);

        // The cursor stops at the partial line, so finishing it makes the next
        // read return it rather than skipping it.
        let rest = "ld\"}\n";
        std::fs::write(dir.join(FILENAME), format!("{whole}{partial}{rest}")).unwrap();
        let next = read(&dir, Some(chunk.cursor)).unwrap();
        assert_eq!(next.segments, vec![seg(Track::System, 2.0, 3.5, "world")]);
    }

    #[test]
    fn a_line_without_a_newline_is_never_a_line() {
        let dir = scratch("nonewline");
        std::fs::write(
            dir.join(FILENAME),
            "{\"v\":1,\"track\":\"mic\",\"start\":0.0,\"end\":1.0,\"text\":\"hi\"}",
        )
        .unwrap();
        let chunk = read(&dir, None).unwrap();
        assert!(chunk.segments.is_empty());
        assert_eq!(chunk.cursor, Cursor::START);
    }

    #[test]
    fn the_cursor_round_trips_and_resumes() {
        let dir = scratch("cursor");
        let mut body = String::new();
        body.push_str("{\"v\":1,\"track\":\"mic\",\"start\":0.0,\"end\":1.0,\"text\":\"one\"}\n");
        body.push_str(
            "{\"v\":1,\"track\":\"system\",\"start\":1.5,\"end\":3.0,\"text\":\"two\"}\n",
        );
        std::fs::write(dir.join(FILENAME), &body).unwrap();

        let first = read(&dir, None).unwrap();
        assert_eq!(first.segments.len(), 2);

        let printed = first.cursor.to_string();
        let parsed: Cursor = printed.parse().unwrap();
        assert_eq!(parsed, first.cursor);
        assert!(printed.starts_with("live:"));

        // Resuming from it finds nothing new.
        let again = read(&dir, Some(parsed)).unwrap();
        assert!(again.segments.is_empty());
        assert_eq!(again.cursor, parsed);

        // And a third line, appended, is all the next read returns.
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join(FILENAME))
            .unwrap();
        file.write_all(
            b"{\"v\":1,\"track\":\"mic\",\"start\":4.0,\"end\":5.0,\"text\":\"three\"}\n",
        )
        .unwrap();
        let third = read(&dir, Some(parsed)).unwrap();
        assert_eq!(third.segments.len(), 1);
        assert_eq!(third.segments[0].text, "three");
    }

    #[test]
    fn a_cursor_from_somewhere_else_does_not_parse() {
        // A transcript cursor, a bare number, a truncated one: all refused,
        // rather than read as an offset into the wrong file.
        for bad in ["0", "transcript:0", "live:", "live:zz", ""] {
            assert!(Cursor::from_str(bad).is_err(), "{bad} parsed");
        }
    }

    #[test]
    fn a_cursor_past_the_end_is_an_error() {
        let dir = scratch("past-end");
        std::fs::write(dir.join(FILENAME), "{\"v\":1}\n").unwrap();
        let err = read(&dir, Some(Cursor(10_000))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_newer_format_is_refused_rather_than_misread() {
        let dir = scratch("version");
        std::fs::write(
            dir.join(FILENAME),
            "{\"v\":2,\"track\":\"mic\",\"start\":0.0,\"end\":1.0,\"text\":\"hi\"}\n",
        )
        .unwrap();
        assert!(read(&dir, None).is_err());
    }

    #[test]
    fn a_malformed_line_names_itself() {
        let dir = scratch("malformed");
        std::fs::write(dir.join(FILENAME), "{\"v\":1,\"track\":\"mic\"}\n").unwrap();
        let err = read(&dir, None).unwrap_err();
        assert!(
            err.to_string().contains("line 1"),
            "the error should say which line: {err}"
        );
    }

    #[test]
    fn status_serialises_as_a_flat_object() {
        let running = LiveStatus {
            state: LiveState::Running,
            kind: None,
            reason: None,
            segments: 3,
            last_end_secs: Some(12.5),
            dropped_secs: 0.0,
        };
        let json = serde_json::to_value(&running).unwrap();
        assert_eq!(json["state"], "running");
        assert!(json.get("kind").is_none(), "an absent reason is omitted");
        assert_eq!(json["segments"], 3);
        assert_eq!(json["last_end_secs"], 12.5);

        let declined = LiveStatus {
            state: LiveState::Declined,
            kind: Some("model_missing".to_string()),
            reason: Some("missing".into()),
            segments: 0,
            last_end_secs: None,
            dropped_secs: 0.0,
        };
        let json = serde_json::to_value(&declined).unwrap();
        assert_eq!(json["state"], "declined");
        assert_eq!(json["kind"], "model_missing");
    }
}
