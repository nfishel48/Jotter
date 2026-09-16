//! The `meta.json` sidecar.
//!
//! The two tracks are written by two independent cpal streams, so nothing in
//! the WAV files themselves says how they line up. This records enough to
//! realign them downstream: wall-clock bounds for the recording, and the
//! `StreamInstant` of each stream's first callback. On macOS both instants
//! derive from host time, so their difference is the offset between the tracks.

use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// `Deserialize` as well as `Serialize` because the offline echo-cancellation
/// pass reads this file back — it runs long after `stop()` has returned, and
/// `first_callback_nanos` is one of its inputs.
///
/// Read it with `serde_json::from_str`, never via `serde_json::Value`: the
/// latter cannot represent a `u128` above `u64::MAX`. Real values are ~3e14 so
/// nothing overflows today, but the failure would be silent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackInfo {
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

/// What the echo-cancellation pass did, or decided not to do.
///
/// Every field is either a number or a short fixed string, so the whole struct
/// is safe to report as telemetry except for `path`.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub filter_ms: u32,
    pub frame_ms: u32,
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

    /// The mic track a transcriber should use.
    ///
    /// Returns the cancelled track only when the pass's own recorded numbers
    /// clear the bar, so the recording carries the evidence for whether the
    /// cancellation is worth using and no downstream consumer has to re-derive
    /// the policy. Falls back to the raw mic track, which is always present.
    pub fn preferred_mic_path(&self) -> Option<&str> {
        let raw = self.mic.as_ref().map(|t| t.path.as_str());
        let Some(aec) = self.aec.as_ref() else {
            return raw;
        };
        let Some(path) = aec.path.as_deref() else {
            return raw;
        };
        let good = aec.erle_db.is_some_and(|e| e >= USABLE_ERLE_DB)
            && aec.near_gain_db.is_some_and(|g| g >= MAX_NEAR_LOSS_DB);
        if good { Some(path) } else { raw }
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
            path: "t.wav".into(),
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
            filter_ms: 150,
            frame_ms: 10,
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
        assert_eq!(parsed.system.expect("system track").frames, 0);
        assert_eq!(
            parsed.mic.expect("mic track").first_callback_nanos,
            Some(22_186_248_869_375)
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
        let raw = "t.wav";

        let no_pass = meta(None, None);
        assert_eq!(no_pass.preferred_mic_path(), Some(raw));

        let mut good = meta(None, None);
        good.aec = Some(aec_info(Some(12.4), Some(-0.2)));
        assert_eq!(good.preferred_mic_path(), Some("mic_aec.wav"));

        // Cancelled almost nothing.
        let mut weak = meta(None, None);
        weak.aec = Some(aec_info(Some(2.1), Some(-0.2)));
        assert_eq!(weak.preferred_mic_path(), Some(raw));

        // Cancelled plenty, but ate the user's voice doing it.
        let mut damaging = meta(None, None);
        damaging.aec = Some(aec_info(Some(14.0), Some(-3.5)));
        assert_eq!(damaging.preferred_mic_path(), Some(raw));

        // Declined, so there is no track to prefer.
        let mut bypassed = meta(None, None);
        bypassed.aec = Some(AecInfo {
            path: None,
            bypassed: Some("track_length_mismatch".into()),
            erle_db: None,
            near_gain_db: None,
            ..aec_info(None, None)
        });
        assert_eq!(bypassed.preferred_mic_path(), Some(raw));
    }

    /// Guards `"aec": null` appearing in every recording's `meta.json`, which
    /// would invalidate the file shape quoted in `docs/AUDIO_CAPTURE.md`.
    #[test]
    fn aec_none_serializes_to_no_key_at_all() {
        let json = serde_json::to_string(&meta(None, None)).expect("serialize");
        assert!(!json.contains("aec"), "unexpected aec key in {json}");
    }
}
