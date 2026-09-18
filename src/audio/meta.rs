//! The `meta.json` sidecar.
//!
//! The two tracks are written by two independent cpal streams, so nothing in
//! the WAV files themselves says how they line up. This records enough to
//! realign them downstream: wall-clock bounds for the recording, and the
//! `StreamInstant` of each stream's first callback. On macOS both instants
//! derive from host time, so their difference is the offset between the tracks.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Filenames of the two captured tracks.
///
/// Here rather than in `audio::start` for the same reason as
/// [`timestamp_dir_name`]: they are part of the `meta.json` contract, and the
/// writer stores them verbatim as [`TrackInfo::path`].
pub const MIC_NAME: &str = "mic.wav";
pub const SYSTEM_NAME: &str = "system.wav";

/// Turn a stored track path into one the caller can open.
///
/// **Every path in `meta.json` is relative to the recording directory** — a
/// recording is a self-contained folder, and the moment a path reaches outside
/// it the file stops being findable after the folder is moved, copied to
/// another machine, or simply recorded by a CLI run whose working directory is
/// long gone.
///
/// Older files do not honour that. Until this was fixed, `TrackInfo::path` was
/// whatever `audio::start` was handed: absolute from the GUI, relative to the
/// CLI's working directory from `jotter record --out some/dir`, while
/// `AecInfo::path` in the same file was already a bare filename. So anything
/// with a directory component is a pre-fix record, and only its file name is
/// worth believing.
fn resolve_track_path(dir: &Path, stored: &str) -> PathBuf {
    let stored = Path::new(stored);
    match stored.file_name() {
        Some(name) => dir.join(name),
        // `""`, `"."`, `".."` — nothing to resolve. Returning the directory
        // keeps the result inside the recording, which joining the stored value
        // would not.
        None => dir.to_path_buf(),
    }
}

/// `Deserialize` as well as `Serialize` because the offline echo-cancellation
/// pass reads this file back — it runs long after `stop()` has returned, and
/// `first_callback_nanos` is one of its inputs.
///
/// Read it with `serde_json::from_str`, never via `serde_json::Value`: the
/// latter cannot represent a `u128` above `u64::MAX`. Real values are ~3e14 so
/// nothing overflows today, but the failure would be silent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackInfo {
    /// File name of the track, relative to the recording directory. Resolve it
    /// with [`TrackInfo::resolve`] rather than reading it directly — old
    /// recordings hold a full path here.
    pub path: String,
    pub device_name: String,
    pub device_id: Option<String>,
    /// Native device rate, written to the WAV as-is. Resampling to 16 kHz for
    /// Whisper is deliberately left to the transcription step, where a proper
    /// resampler can be used.
    pub sample_rate: u32,
    /// Channels in the WAV. Always 1 — both tracks are downmixed to mono.
    pub channels: u16,
    /// Channels the device actually delivered, before downmixing.
    pub source_channels: u16,
    pub frames: u64,
    /// Nanoseconds of the first callback's `StreamInstant`. Comparable across
    /// the two tracks; `None` if the stream never produced a callback.
    pub first_callback_nanos: Option<u128>,
    /// How many times cpal's error callback fired. Non-zero means the track is
    /// suspect — a stream that dies mid-meeting otherwise just yields a short
    /// file with no other indication.
    pub stream_errors: u64,
}

impl TrackInfo {
    /// This track's file inside `dir`, the recording directory.
    pub fn resolve(&self, dir: &Path) -> PathBuf {
        resolve_track_path(dir, &self.path)
    }
}

/// What the echo-cancellation pass did, or decided not to do.
///
/// Every field is either a number or a short fixed string, so the whole struct
/// is safe to report as telemetry except for `path`.
///
/// `#[serde(default)]` on the struct, the same discipline as
/// [`crate::config::Settings`]: an `aec` block written by an older build must
/// still load, or changing the canceller would make every previously processed
/// recording unreadable. `version` is what distinguishes a stale block from a
/// current one — absence of a field never should.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AecInfo {
    /// Relative path of the cancelled track. Absent when the pass declined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Algorithm and parameter generation. Bumped whenever a change would give
    /// a different result, so a stale `mic_aec.wav` is detectable rather than
    /// being mistaken for a fresh one.
    pub version: u32,
    /// Bulk delay removed before cancelling, in frames. Positive means the mic
    /// track lagged the system track, which is the normal direction.
    pub delay_frames: i64,
    /// `"measured"`, `"meta_offset"` or `"zero"`.
    pub delay_source: String,
    pub delay_confidence: f32,
    pub delay_spread_ms: f32,
    pub drift_ppm: f32,
    /// AEC3's own estimate of the echo delay, in milliseconds. Recorded
    /// alongside our own measurement as an independent cross-check — they
    /// should broadly agree, and a wide disagreement is the first thing to look
    /// at when a recording cancels badly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_delay_ms: Option<u32>,
    /// Echo return loss enhancement over far-end-active frames. The headline
    /// number: how much echo actually came out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub erle_db: Option<f32>,
    /// Level change over frames where the far end was silent. Near 0 is good;
    /// clearly negative means the filter ate some of the user's own voice —
    /// the failure an ERLE figure cannot see.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub near_gain_db: Option<f32>,
    pub silence_secs: f32,
    pub near_only_secs: f32,
    pub far_only_secs: f32,
    pub double_talk_secs: f32,
    /// Far-end audio missing from `system.wav` because the output device was
    /// idle and the tap yielded no frames. Non-zero is why a pass bails out.
    pub far_gap_secs: f32,
    /// `Some(reason)` when the pass looked and declined, from
    /// `AecBypass::kind()`. The pass must always be able to say "I decided not
    /// to, and here is why" rather than leaving no file and no explanation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bypassed: Option<String>,
}

/// What the transcription pass did, or decided not to do.
///
/// Same discipline as [`AecInfo`], for the same reasons: `#[serde(default)]` on
/// the struct so a block written by an older build still loads, `version` rather
/// than field presence to tell a stale block from a current one, and every field
/// either a number or a short fixed string so the whole struct is safe to report
/// as telemetry except `path`.
///
/// The transcript itself is **not** here. It is a separate file — a meeting's
/// worth of text has no business in a sidecar every stage reads and rewrites,
/// and keeping it out means `meta.json` stays something you can `cat`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TranscriptInfo {
    /// Relative path of the transcript. Absent when the pass declined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Model and parameter generation, in the sense of [`AecInfo::version`].
    pub version: u32,
    /// Catalogue id of the model used, e.g. `"parakeet-tdt-0.6b-v2-int8"`.
    /// Recorded because it is the single biggest determinant of the output, and
    /// a transcript is worth redoing when it changes.
    pub model: String,
    /// The inference engine, e.g. `"sherpa-onnx"`.
    pub engine: String,
    /// Segments written, across both tracks.
    pub segments: u32,
    /// Segments attributed to you, and to everyone else. Split out because a
    /// recording where one of these is zero is a recording where one track was
    /// silent, which is worth seeing without opening the transcript.
    pub mic_segments: u32,
    pub system_segments: u32,
    /// Whitespace-separated words across every segment. A crude figure, and
    /// enough to tell "it transcribed the meeting" from "it transcribed a cough".
    pub words: u32,
    /// Seconds the voice-activity pass called speech, summed over both tracks.
    pub speech_secs: f32,
    /// Seconds of audio read, summed over both tracks.
    pub audio_secs: f32,
    /// Wall clock of the pass. With `audio_secs` this gives the real-time
    /// factor, which is the number that decides whether this is usable on a
    /// given machine.
    pub elapsed_secs: f32,
    /// `Some(reason)` when the pass looked and declined, from
    /// `TranscribeDecline::kind()`. Same contract as [`AecInfo::bypassed`]: a
    /// pass must always be able to say "I decided not to, and here is why".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declined: Option<String>,
}

/// The ERLE below which the cancelled track is not worth preferring.
///
/// Well under the 14-18 dB ceiling the reference recording's coherence implies,
/// but comfortably above the 0.6-6.7 dB that delay-and-subtract reached on it —
/// so it separates "the filter worked" from "the filter found nothing".
const USABLE_ERLE_DB: f32 = 6.0;

/// The most near-end damage tolerated before the cancelled track is rejected.
/// Below where transcription accuracy moves.
const MAX_NEAR_LOSS_DB: f32 = -1.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub started_at: f64,
    pub ended_at: f64,
    pub mic: Option<TrackInfo>,
    pub system: Option<TrackInfo>,
    /// Written by the offline pass, not by `stop()`. Absent on every recording
    /// made before this existed, and on any the pass never ran over.
    ///
    /// `skip_serializing_if` is load-bearing: without it every freshly recorded
    /// `meta.json` grows an `"aec": null`, and the file's shape is quoted in
    /// `docs/AUDIO_CAPTURE.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aec: Option<AecInfo>,
    /// Written by the transcription pass, which runs after the echo pass and
    /// reads its verdict through [`Meta::preferred_mic_path`].
    ///
    /// `skip_serializing_if` for the same load-bearing reason as `aec`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<TranscriptInfo>,
}

impl Meta {
    pub fn write(&self, path: &Path) -> io::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)
    }

    pub fn read(path: &Path) -> io::Result<Self> {
        // from_str, not from_value: see the note on `TrackInfo`.
        let json = std::fs::read_to_string(path)?;
        serde_json::from_str(&json).map_err(io::Error::other)
    }

    /// The mic track a transcriber should use, inside `dir`.
    ///
    /// Returns the cancelled track only when the pass's own recorded numbers
    /// clear the bar, so the recording carries the evidence for whether the
    /// cancellation is worth using and no downstream consumer has to re-derive
    /// the policy. Falls back to the raw mic track, which is always present.
    ///
    /// Takes the recording directory and returns a resolved path because the
    /// two branches read their filename from different fields, written by
    /// different stages. Handing back a bare `&str` meant a caller's path
    /// silently changed convention depending on whether the pass declined.
    pub fn preferred_mic_path(&self, dir: &Path) -> Option<PathBuf> {
        let raw = self.mic.as_ref().map(|t| t.resolve(dir));
        let Some(aec) = self.aec.as_ref() else {
            return raw;
        };
        let Some(path) = aec.path.as_deref() else {
            return raw;
        };
        let good = aec.erle_db.is_some_and(|e| e >= USABLE_ERLE_DB)
            && aec.near_gain_db.is_some_and(|g| g >= MAX_NEAR_LOSS_DB);
        if good {
            Some(resolve_track_path(dir, path))
        } else {
            raw
        }
    }

    /// Offset between the two tracks in seconds, if both produced callbacks.
    /// Positive means the system track started later than the mic track.
    pub fn track_offset_secs(&self) -> Option<f64> {
        let mic = self.mic.as_ref()?.first_callback_nanos?;
        let system = self.system.as_ref()?.first_callback_nanos?;
        Some((system as f64 - mic as f64) / 1e9)
    }

    pub fn duration_secs(&self) -> f64 {
        self.ended_at - self.started_at
    }
}

/// Folder name for a new recording, e.g. `2026-09-15_14-32-08`.
///
/// Local time, and zero-padded so lexical order matches chronological order.
/// Lives here rather than in either front end so the GUI and the CLI cannot
/// drift apart on the layout of a recordings directory.
pub fn timestamp_dir_name() -> String {
    chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string()
}

pub fn to_unix_secs(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(first_callback_nanos: Option<u128>) -> TrackInfo {
        TrackInfo {
            path: MIC_NAME.into(),
            device_name: "Test".into(),
            device_id: None,
            sample_rate: 48_000,
            channels: 1,
            source_channels: 2,
            frames: 48_000,
            first_callback_nanos,
            stream_errors: 0,
        }
    }

    fn meta(mic: Option<u128>, system: Option<u128>) -> Meta {
        Meta {
            started_at: 100.0,
            ended_at: 110.5,
            mic: Some(track(mic)),
            system: Some(track(system)),
            aec: None,
            transcript: None,
        }
    }

    fn transcript_info() -> TranscriptInfo {
        TranscriptInfo {
            path: Some("transcript.json".into()),
            version: 1,
            model: "parakeet-tdt-0.6b-v2-int8".into(),
            engine: "sherpa-onnx".into(),
            segments: 214,
            mic_segments: 88,
            system_segments: 126,
            words: 3_104,
            speech_secs: 487.5,
            audio_secs: 1_062.0,
            elapsed_secs: 73.2,
            declined: None,
        }
    }

    fn aec_info(erle_db: Option<f32>, near_gain_db: Option<f32>) -> AecInfo {
        AecInfo {
            path: Some("mic_aec.wav".into()),
            version: 1,
            delay_frames: 1_430,
            delay_source: "measured".into(),
            delay_confidence: 11.3,
            delay_spread_ms: 2.4,
            drift_ppm: 0.2,
            reported_delay_ms: Some(31),
            erle_db,
            near_gain_db,
            silence_secs: 28.0,
            near_only_secs: 91.0,
            far_only_secs: 22.0,
            double_talk_secs: 394.0,
            far_gap_secs: 0.0,
            bypassed: None,
        }
    }

    #[test]
    fn offset_is_system_relative_to_mic() {
        // System started 7ms after the mic -> positive offset.
        let m = meta(Some(1_000_000_000), Some(1_007_000_000));
        assert!((m.track_offset_secs().unwrap() - 0.007).abs() < 1e-9);

        // And negative in the other direction, rather than underflowing the
        // u128 subtraction.
        let m = meta(Some(1_007_000_000), Some(1_000_000_000));
        assert!((m.track_offset_secs().unwrap() + 0.007).abs() < 1e-9);
    }

    #[test]
    fn offset_is_none_without_both_callbacks() {
        assert!(meta(None, Some(1)).track_offset_secs().is_none());
        assert!(meta(Some(1), None).track_offset_secs().is_none());

        // A track that never opened at all, not merely one without callbacks.
        let m = Meta {
            mic: None,
            ..meta(Some(1), Some(2))
        };
        assert!(m.track_offset_secs().is_none());
    }

    #[test]
    fn duration_is_wall_clock_span() {
        assert!((meta(None, None).duration_secs() - 10.5).abs() < 1e-9);
    }

    /// The bug this guards: `Deserialize` drifting from `Serialize` — a renamed
    /// or newly-required field. `jotter process` would then read
    /// `first_callback_nanos: None`, fall back to a zero offset, and mis-align
    /// every recording while reporting success.
    ///
    /// Compares re-serialised JSON rather than deriving `PartialEq` on types
    /// that hold `f32`s.
    #[test]
    fn meta_json_round_trips() {
        let mut original = meta(Some(302_534_622_096_218), Some(302_534_627_429_470));
        original.aec = Some(aec_info(Some(12.4), Some(-0.2)));
        original.transcript = Some(transcript_info());

        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: Meta = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(json, serde_json::to_string(&parsed).expect("reserialize"));
    }

    /// Verbatim from `recordings/1789411995/meta.json`, a recording made before
    /// echo cancellation existed. Old recordings must stay loadable, since
    /// `jotter process` exists precisely to clean them up.
    ///
    /// Note `system.frames: 0` with a null `first_callback_nanos` — the macOS
    /// signature of an output device that was idle for the whole recording.
    #[test]
    fn reads_a_meta_json_written_before_aec_existed() {
        let json = r#"{
          "started_at": 1789411995.0,
          "ended_at": 1789412005.33,
          "mic": {
            "path": "recordings/1789411995/mic.wav",
            "device_name": "MacBook Pro Microphone",
            "device_id": "coreaudio:BuiltInMicrophoneDevice",
            "sample_rate": 48000,
            "channels": 1,
            "source_channels": 1,
            "frames": 480256,
            "first_callback_nanos": 22186248869375,
            "stream_errors": 0
          },
          "system": {
            "path": "recordings/1789411995/system.wav",
            "device_name": "MacBook Pro Speakers",
            "device_id": "coreaudio:BuiltInSpeakerDevice",
            "sample_rate": 48000,
            "channels": 1,
            "source_channels": 2,
            "frames": 0,
            "first_callback_nanos": null,
            "stream_errors": 0
          }
        }"#;

        let parsed: Meta = serde_json::from_str(json).expect("pre-AEC meta.json must load");
        assert!(parsed.aec.is_none());
        assert!(parsed.transcript.is_none());
        assert_eq!(parsed.system.as_ref().expect("system track").frames, 0);
        assert_eq!(
            parsed.mic.as_ref().expect("mic track").first_callback_nanos,
            Some(22_186_248_869_375)
        );

        // Its paths are relative to a working directory that no longer exists,
        // so they resolve against the recording directory the file was found
        // in. `recordings/1789411995/mic.wav` must not be joined as-is.
        let dir = Path::new("/archive/1789411995");
        assert_eq!(
            parsed.preferred_mic_path(dir),
            Some(dir.join(MIC_NAME)),
            "an old meta.json must still name a findable file"
        );
        assert_eq!(
            parsed.system.expect("system track").resolve(dir),
            dir.join(SYSTEM_NAME)
        );
    }

    /// The real value from the reference recording. If this ever fails the field
    /// must become `u64` — nanoseconds in 64 bits still hold 584 years — rather
    /// than being quietly truncated.
    #[test]
    fn first_callback_nanos_survives_json() {
        let nanos = 302_534_622_096_218u128;
        let json = serde_json::to_string(&meta(Some(nanos), None)).expect("serialize");
        let parsed: Meta = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            parsed.mic.expect("mic track").first_callback_nanos,
            Some(nanos)
        );
    }

    /// Guards a failed cancellation being fed to transcription anyway. A pass
    /// that ran but achieved nothing is worse than no pass at all, because it
    /// looks like a finished job.
    #[test]
    fn prefers_the_cancelled_track_only_when_the_numbers_clear_the_bar() {
        let dir = Path::new("/recordings/2026-09-15_14-32-08");
        let raw = dir.join(MIC_NAME);

        let no_pass = meta(None, None);
        assert_eq!(no_pass.preferred_mic_path(dir), Some(raw.clone()));

        let mut good = meta(None, None);
        good.aec = Some(aec_info(Some(12.4), Some(-0.2)));
        assert_eq!(
            good.preferred_mic_path(dir),
            Some(dir.join("mic_aec.wav")),
            "a pass whose numbers clear the bar"
        );

        // Cancelled almost nothing.
        let mut weak = meta(None, None);
        weak.aec = Some(aec_info(Some(2.1), Some(-0.2)));
        assert_eq!(weak.preferred_mic_path(dir), Some(raw.clone()));

        // Cancelled plenty, but ate the user's voice doing it.
        let mut damaging = meta(None, None);
        damaging.aec = Some(aec_info(Some(14.0), Some(-3.5)));
        assert_eq!(damaging.preferred_mic_path(dir), Some(raw.clone()));

        // Declined, so there is no track to prefer.
        let mut bypassed = meta(None, None);
        bypassed.aec = Some(AecInfo {
            path: None,
            bypassed: Some("track_length_mismatch".into()),
            erle_db: None,
            near_gain_db: None,
            ..aec_info(None, None)
        });
        assert_eq!(bypassed.preferred_mic_path(dir), Some(raw));
    }

    /// Both branches must land in the recording directory. The bug this guards:
    /// `preferred_mic_path` used to hand back `mic.path` verbatim — absolute
    /// from the GUI — but `aec.path` as a bare filename, so the convention a
    /// caller got depended on a decision taken in a different stage.
    #[test]
    fn both_branches_resolve_into_the_recording_directory() {
        let dir = Path::new("/recordings/2026-09-15_14-32-08");

        let mut declined = meta(None, None);
        declined.mic = Some(TrackInfo {
            path: "/Users/nick/Documents/Jotter/2026-09-15_14-32-08/mic.wav".into(),
            ..track(None)
        });
        let mut applied = declined.clone();
        applied.aec = Some(aec_info(Some(12.4), Some(-0.2)));

        for m in [declined, applied] {
            let path = m.preferred_mic_path(dir).expect("a mic track");
            assert_eq!(
                path.parent(),
                Some(dir),
                "{} escaped {dir:?}",
                path.display()
            );
        }
    }

    /// Paths written before the convention was fixed. Each of these is a real
    /// shape found on disk, and all three must resolve to the same file — a
    /// silent mis-resolution would point transcription at a stale recording, or
    /// at nothing at all.
    #[test]
    fn resolves_old_and_new_stored_paths_to_the_same_file() {
        let dir = Path::new("/recordings/2026-09-15_14-32-08");
        let expected = dir.join(MIC_NAME);

        for stored in [
            // Current: relative to the recording directory.
            "mic.wav",
            // GUI, before the fix: recordings_root()/<timestamp>/mic.wav.
            "/Users/nick/Documents/Jotter/2026-09-15_14-32-08/mic.wav",
            // CLI, before the fix: relative to whatever cwd it ran in.
            "recordings/1789411995/mic.wav",
        ] {
            let info = TrackInfo {
                path: stored.into(),
                ..track(None)
            };
            assert_eq!(info.resolve(dir), expected, "stored as {stored:?}");
        }
    }

    /// A stored value with no file name at all — corruption rather than an old
    /// format, but `dir.join("..")` would walk out of the recording, which is
    /// the one outcome worth ruling out.
    #[test]
    fn a_pathless_stored_value_stays_inside_the_recording() {
        let dir = Path::new("/recordings/2026-09-15_14-32-08");
        for stored in ["", ".", "..", "/"] {
            let info = TrackInfo {
                path: stored.into(),
                ..track(None)
            };
            assert_eq!(info.resolve(dir), dir, "stored as {stored:?}");
        }
    }

    /// Guards `"aec": null` and `"transcript": null` appearing in every
    /// recording's `meta.json`, which would invalidate the file shape quoted in
    /// `docs/AUDIO_CAPTURE.md`. One test for both, because the mistake is a
    /// single missing attribute and it is the same one either time.
    #[test]
    fn a_stage_that_has_not_run_serializes_to_no_key_at_all() {
        let json = serde_json::to_string(&meta(None, None)).expect("serialize");
        assert!(!json.contains("aec"), "unexpected aec key in {json}");
        assert!(
            !json.contains("transcript"),
            "unexpected transcript key in {json}"
        );
    }

    /// A decline writes a block with a reason and no `path`, and both halves
    /// have to survive the round trip: the reason is what a re-run reconsiders,
    /// and the absent path is what stops [`crate::audio::stage::Stage::is_current`]
    /// reading a decline as finished work.
    #[test]
    fn a_declined_transcript_block_round_trips_with_no_path() {
        let mut original = meta(None, None);
        original.transcript = Some(TranscriptInfo {
            path: None,
            declined: Some("model_missing".into()),
            ..transcript_info()
        });

        // Scoped to the transcript block: the tracks have a `path` of their own,
        // and a substring check over the whole file would only ever see theirs.
        let json = serde_json::to_string(&original.transcript).expect("serialize");
        assert!(!json.contains("\"path\""), "a decline wrote a path: {json}");

        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: Meta = serde_json::from_str(&json).expect("deserialize");
        let block = parsed.transcript.expect("transcript block");
        assert!(block.path.is_none());
        assert_eq!(block.declined.as_deref(), Some("model_missing"));
    }
}
