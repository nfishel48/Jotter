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

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize)]
pub struct Meta {
    pub started_at: f64,
    pub ended_at: f64,
    pub mic: Option<TrackInfo>,
    pub system: Option<TrackInfo>,
}

impl Meta {
    pub fn write(&self, path: &Path) -> io::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)
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
}
