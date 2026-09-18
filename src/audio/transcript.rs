//! The `transcript.json` artifact.
//!
//! Deliberately **not** feature-gated, for the same reason [`crate::audio::meta`]
//! is not: reading a transcript and producing one are different jobs. The
//! producer needs a 660 MB model and an ONNX runtime; the consumers — a future
//! diarization pass, note extraction, anything that wants to search old meetings
//! — need neither, and should not have to build the inference stack to open a
//! file. The stage that writes this lives in [`crate::audio::transcribe`] and is
//! gated.
//!
//! The format exists because a meeting is two recordings, not one. `mic.wav` is
//! you and `system.wav` is everyone else, and transcribing them separately is
//! what turns speaker attribution from a hard problem into a field — see the
//! note on two tracks in `docs/ARCHITECTURE.md`. Every segment therefore says
//! which track it came from, on one shared timeline.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::audio::stage::write_atomic;

/// Bumped when the shape of the file changes in a way a reader would notice.
/// Distinct from the *stage* version, which moves whenever the text would come
/// out differently — a better model does not change the format.
pub const FORMAT_VERSION: u32 = 1;

/// Which recording a segment came from.
///
/// The cheap half of speaker attribution, and the reason it is cheap: the
/// operating system already separated these two signals for us, so "was this me
/// or someone else" needs no inference at all. Telling apart the several people
/// inside [`Track::System`] is the part that needs diarization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Track {
    /// Your microphone.
    Mic,
    /// System audio: everyone else in the meeting.
    System,
}

impl Track {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mic => "mic",
            Self::System => "system",
        }
    }
}

/// One stretch of speech.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    /// Seconds from the start of the recording, on the **mic track's**
    /// timeline. System segments are shifted onto it when the transcript is
    /// assembled; see [`Segment::shifted`].
    pub start: f64,
    pub end: f64,
    pub track: Track,
    /// Who said it, once anyone knows.
    ///
    /// Present in the format from the first version and written by nobody yet:
    /// diarization is a later stage, and having the field already means it can
    /// be additive — it fills this in rather than changing the shape of a file
    /// other things have started reading. Omitted from the JSON entirely while
    /// it is `None`, so an undiarized transcript carries no misleading nulls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
    pub text: String,
}

impl Segment {
    /// The same segment, moved onto another timeline.
    ///
    /// Used for exactly one thing, and it is load-bearing: the two cpal streams
    /// start at different instants, so a timestamp from `system.wav` and a
    /// timestamp from `mic.wav` are not directly comparable. Skip this and the
    /// two tracks interleave wrongly — by tens of milliseconds normally, which
    /// is enough to put an answer before the question it answers.
    pub fn shifted(mut self, by_secs: f64) -> Self {
        self.start += by_secs;
        self.end += by_secs;
        self
    }
}

/// A whole transcript, as it sits on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    pub version: u32,
    /// Catalogue id of the model that produced this, so a transcript carries
    /// the evidence for how much to trust it.
    pub model: String,
    pub segments: Vec<Segment>,
}

impl Transcript {
    pub fn new(model: impl Into<String>, segments: Vec<Segment>) -> Self {
        Self {
            version: FORMAT_VERSION,
            model: model.into(),
            segments,
        }
    }

    /// Whitespace-separated words across every segment.
    ///
    /// Crude on purpose. It is not a linguistic count, it is the figure that
    /// tells "it transcribed the meeting" from "it transcribed a cough".
    pub fn words(&self) -> u32 {
        self.segments
            .iter()
            .map(|s| s.text.split_whitespace().count() as u32)
            .sum()
    }

    pub fn segments_from(&self, track: Track) -> u32 {
        self.segments.iter().filter(|s| s.track == track).count() as u32
    }

    /// Writes through a temporary sibling, like every other artifact: the tray's
    /// Quit calls `process::exit(0)`, and half a JSON file is no better than
    /// half a WAV.
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        write_atomic(path, |tmp| {
            std::fs::write(tmp, serde_json::to_string_pretty(self)?)
        })
    }

    pub fn read(path: &Path) -> std::io::Result<Self> {
        let json = std::fs::read_to_string(path)?;
        serde_json::from_str(&json).map_err(std::io::Error::other)
    }
}

/// Interleave two tracks' segments onto one timeline.
///
/// Sorted by start time, which is what makes the file readable top to bottom as
/// a conversation rather than as two monologues. `sort_by` on a `f64` key needs
/// `total_cmp`; the timestamps come from sample counts so they are never NaN,
/// but reaching for `partial_cmp().unwrap()` here would be a panic waiting for
/// the one recording that is.
///
/// Ties keep the mic first. Arbitrary, but *stably* arbitrary: two segments
/// starting in the same sample is only realistic when both tracks begin with
/// speech already in progress, and a deterministic order means re-running the
/// stage over one recording produces one file.
pub fn merge(mic: Vec<Segment>, system: Vec<Segment>) -> Vec<Segment> {
    let mut all = mic;
    all.extend(system);
    all.sort_by(|a, b| {
        a.start
            .total_cmp(&b.start)
            .then_with(|| track_order(a.track).cmp(&track_order(b.track)))
    });
    all
}

fn track_order(track: Track) -> u8 {
    match track {
        Track::Mic => 0,
        Track::System => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(start: f64, end: f64, track: Track, text: &str) -> Segment {
        Segment {
            start,
            end,
            track,
            speaker: None,
            text: text.into(),
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("jotter-transcript-{name}-{unique}"));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn a_transcript_round_trips_through_its_file() {
        let dir = scratch("roundtrip");
        let path = dir.join("transcript.json");

        let original = Transcript::new(
            "parakeet-tdt-0.6b-v2-int8",
            vec![
                segment(0.4, 3.1, Track::Mic, "morning all"),
                segment(3.2, 8.0, Track::System, "morning, shall we start"),
            ],
        );

        original.write(&path).expect("write");
        assert_eq!(Transcript::read(&path).expect("read"), original);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `speaker` exists so diarization can be additive. Until it runs, the field
    /// must be absent rather than null — a reader should be able to tell "nobody
    /// has attributed this" from "attributed to nothing", and a file full of
    /// nulls invites the second reading.
    #[test]
    fn an_undiarized_segment_carries_no_speaker_key() {
        let transcript = Transcript::new("m", vec![segment(0.0, 1.0, Track::Mic, "hello")]);
        let json = serde_json::to_string(&transcript).expect("serialize");
        assert!(
            !json.contains("speaker"),
            "unexpected speaker key in {json}"
        );

        // And once it is set it survives the round trip, since that is the whole
        // point of reserving it.
        let attributed = Transcript::new(
            "m",
            vec![Segment {
                speaker: Some("speaker_01".into()),
                ..segment(0.0, 1.0, Track::System, "hello")
            }],
        );
        let json = serde_json::to_string(&attributed).expect("serialize");
        let parsed: Transcript = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.segments[0].speaker.as_deref(), Some("speaker_01"));
    }

    /// The bug this guards, and the whole reason `shifted` exists: the two cpal
    /// streams start at different instants, so timestamps read out of the two
    /// files are not comparable. Merging them raw puts the reply before the
    /// remark.
    #[test]
    fn the_system_track_is_moved_onto_the_mic_timeline_before_merging() {
        // The system stream started 120 ms after the mic stream, so a system
        // segment at t=1.00 in its own file really happened at t=1.12.
        let offset = 0.12;

        let mic = vec![segment(1.05, 1.90, Track::Mic, "what do you think")];
        let system = vec![segment(1.00, 2.50, Track::System, "sounds right to me")];

        let shifted: Vec<_> = system
            .clone()
            .into_iter()
            .map(|s| s.shifted(offset))
            .collect();
        let merged = merge(mic.clone(), shifted);

        assert_eq!(merged[0].track, Track::Mic, "the question must come first");
        assert_eq!(merged[1].track, Track::System);
        assert!((merged[1].start - 1.12).abs() < 1e-9);
        assert!((merged[1].end - 2.62).abs() < 1e-9);

        // Without the shift the same two segments come out the wrong way round —
        // this is the failure, asserted so a regression cannot pass quietly.
        let unshifted = merge(mic, system);
        assert_eq!(unshifted[0].track, Track::System);
    }

    #[test]
    fn merging_orders_the_conversation_by_time() {
        let mic = vec![
            segment(0.0, 1.0, Track::Mic, "a"),
            segment(4.0, 5.0, Track::Mic, "c"),
        ];
        let system = vec![
            segment(2.0, 3.0, Track::System, "b"),
            segment(6.0, 7.0, Track::System, "d"),
        ];

        let merged = merge(mic, system);
        let text: Vec<_> = merged.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(text, ["a", "b", "c", "d"]);
    }

    /// An exact tie has to resolve the same way every run, or re-transcribing
    /// one recording produces two different files.
    #[test]
    fn simultaneous_segments_order_deterministically() {
        let mic = vec![segment(1.0, 2.0, Track::Mic, "me")];
        let system = vec![segment(1.0, 2.0, Track::System, "them")];

        for _ in 0..8 {
            let merged = merge(mic.clone(), system.clone());
            assert_eq!(merged[0].text, "me");
        }
    }

    #[test]
    fn counts_describe_what_is_in_the_file() {
        let transcript = Transcript::new(
            "m",
            vec![
                segment(0.0, 1.0, Track::Mic, "one two three"),
                segment(2.0, 3.0, Track::System, "four five"),
                segment(4.0, 5.0, Track::System, ""),
            ],
        );

        assert_eq!(transcript.words(), 5);
        assert_eq!(transcript.segments_from(Track::Mic), 1);
        assert_eq!(transcript.segments_from(Track::System), 2);
    }
}
