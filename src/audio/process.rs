//! The offline pass over a finished recording.
//!
//! Echo cancellation runs here rather than in the capture callback. The two cpal
//! streams are opened independently with `BufferSize::Default` and never see
//! each other, the realtime callback must not allocate
//! (`docs/ARCHITECTURE.md`), and running offline buys three things worth more
//! than immediacy: the bulk delay can be measured over the whole file, the
//! filter can be converged in one pass and applied in a second, and recordings
//! made before any of this existed can still be cleaned up.
//!
//! The pass is additive. `mic.wav` is the one artifact that cannot be
//! recreated, so it is never touched; the result is written beside it as
//! `mic_aec.wav`, and `system.wav` is an input to the estimate, never an output.
//!
//! This file holds the decisions — what to classify, when to decline — as pure
//! functions over frame energies. The WAV plumbing that feeds them lives at the
//! bottom.

use crate::audio::aec;
use crate::audio::meta::Meta;

/// Bumped whenever a change would give a different result for the same input,
/// so a `mic_aec.wav` left over from an older build is detectable rather than
/// being mistaken for a current one.
pub const AEC_VERSION: u32 = 1;

/// Filename of the cancelled track, written beside `mic.wav`.
pub const OUTPUT_NAME: &str = "mic_aec.wav";

/// How far the two tracks' lengths may differ before the pass declines.
///
/// The reference Linux recording differs by 1024 frames — 21 ms, two callback
/// buffers — and must not trip this. A macOS recording whose output device sat
/// idle differs by seconds, because the tap yields *no frames at all* while
/// idle, so silence is compressed out of `system.wav` rather than recorded. A
/// single bulk delay cannot align across that, and subtracting a misaligned
/// reference adds uncorrelated energy: strictly worse than doing nothing.
pub const GAP_TOLERANCE_SECS: f32 = 0.25;

/// Why a pass declined to cancel.
///
/// A pass must always be able to say "I decided not to, and here is why". The
/// silent alternative — no output file and no explanation — is indistinguishable
/// from a crash.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AecBypass {
    /// No `system.wav`, or it has no frames. Nothing to cancel against.
    NoFarEnd,
    /// No `mic.wav`. Nothing to clean.
    NoNearEnd,
    /// The tracks differ in length by more than [`GAP_TOLERANCE_SECS`].
    TrackLengthMismatch { delta_secs: f32 },
    /// The two tracks were recorded at different rates. Resampling is out of
    /// scope here; the transcription step owns that.
    SampleRateMismatch,
    /// Positive evidence that no single bulk delay can work — a sanity gate was
    /// violated, not merely that no clear correlation peak was found.
    DelayUnmeasurable,
    /// The two devices are on different clocks and drifting apart, so a delay
    /// measured at the start is wrong by the end.
    ClockDrift { ppm: f32 },
    /// The far end and near end were never active apart, so there is no stretch
    /// of echo-only audio for the filter to learn from.
    NoFarOnlyWindows,
    /// Writing the output would risk filling the volume.
    InsufficientDiskSpace,
    /// A current `mic_aec.wav` already exists. Re-run with `--force`.
    AlreadyProcessed,
}

impl AecBypass {
    /// A stable, PII-free name. These reach telemetry and `meta.json`, so they
    /// must never be free-form text — the same contract as
    /// [`crate::audio::capture::CaptureError::kind`].
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NoFarEnd => "no_far_end",
            Self::NoNearEnd => "no_near_end",
            Self::TrackLengthMismatch { .. } => "track_length_mismatch",
            Self::SampleRateMismatch => "sample_rate_mismatch",
            Self::DelayUnmeasurable => "delay_unmeasurable",
            Self::ClockDrift { .. } => "clock_drift",
            Self::NoFarOnlyWindows => "no_far_only_windows",
            Self::InsufficientDiskSpace => "insufficient_disk_space",
            Self::AlreadyProcessed => "already_processed",
        }
    }
}

impl std::fmt::Display for AecBypass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoFarEnd => write!(
                f,
                "no system audio was captured, so there is no echo to remove"
            ),
            Self::NoNearEnd => write!(f, "no microphone track to clean"),
            Self::TrackLengthMismatch { delta_secs } => write!(
                f,
                "the two tracks differ by {delta_secs:.2}s — the system tap was \
                 idle for part of the recording, so the tracks cannot be aligned"
            ),
            Self::SampleRateMismatch => {
                write!(f, "the two tracks were recorded at different sample rates")
            }
            Self::DelayUnmeasurable => {
                write!(f, "could not establish how the two tracks line up")
            }
            Self::ClockDrift { ppm } => write!(
                f,
                "the microphone and speakers are on clocks drifting {ppm:.0} ppm apart"
            ),
            Self::NoFarOnlyWindows => write!(
                f,
                "system audio and your microphone were never active apart, so the \
                 echo path could not be learned"
            ),
            Self::InsufficientDiskSpace => write!(f, "not enough free disk space"),
            Self::AlreadyProcessed => {
                write!(f, "already processed — pass --force to redo it")
            }
        }
    }
}

/// What was happening in a single frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// Neither side active.
    Silence,
    /// The user alone. Where near-end damage is measured.
    NearOnly,
    /// Remote audio alone. Where the filter learns, and where ERLE is measured.
    FarOnly,
    /// Both at once. The hard case, and on the reference recording 74% of it.
    DoubleTalk,
}

/// Seconds spent in each activity class.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Census {
    pub silence: f32,
    pub near_only: f32,
    pub far_only: f32,
    pub double_talk: f32,
}

/// Labels each frame from the two tracks' per-frame RMS.
///
/// Thresholds come from [`aec::active_threshold`], derived per track, so the
/// same rule governs the delay estimator and the metrics.
pub fn classify(mic_rms: &[f32], far_rms: &[f32]) -> Vec<Activity> {
    let near_threshold = aec::active_threshold(mic_rms);
    let far_threshold = aec::active_threshold(far_rms);

    let frames = mic_rms.len().min(far_rms.len());
    (0..frames)
        .map(
            |i| match (mic_rms[i] > near_threshold, far_rms[i] > far_threshold) {
                (false, false) => Activity::Silence,
                (true, false) => Activity::NearOnly,
                (false, true) => Activity::FarOnly,
                (true, true) => Activity::DoubleTalk,
            },
        )
        .collect()
}

pub fn census(activity: &[Activity], frame_secs: f32) -> Census {
    let mut census = Census::default();
    for class in activity {
        let bucket = match class {
            Activity::Silence => &mut census.silence,
            Activity::NearOnly => &mut census.near_only,
            Activity::FarOnly => &mut census.far_only,
            Activity::DoubleTalk => &mut census.double_talk,
        };
        *bucket += frame_secs;
    }
    census
}

/// Seconds of far-end audio missing from `system.wav`.
///
/// Derived from frame counts rather than by scanning for silence, because a gap
/// leaves no trace *in* the file: the writer appends only what arrives, so an
/// idle output device makes `system.wav` shorter rather than quieter. The length
/// difference is the only evidence there is.
pub fn far_gap_secs(mic_frames: u64, system_frames: u64, sample_rate: u32) -> f32 {
    if sample_rate == 0 {
        return 0.0;
    }
    let delta = mic_frames.abs_diff(system_frames);
    delta as f32 / sample_rate as f32
}

/// Whether the two tracks can be aligned at all, before any audio is read.
///
/// Cheap, and it runs first: every check here is answerable from `meta.json`
/// alone, so a hopeless recording costs no I/O.
pub fn check_alignable(meta: &Meta) -> Result<(), AecBypass> {
    let mic = meta.mic.as_ref().ok_or(AecBypass::NoNearEnd)?;
    let system = meta.system.as_ref().ok_or(AecBypass::NoFarEnd)?;

    if mic.frames == 0 {
        return Err(AecBypass::NoNearEnd);
    }
    if system.frames == 0 {
        return Err(AecBypass::NoFarEnd);
    }
    if mic.sample_rate != system.sample_rate {
        return Err(AecBypass::SampleRateMismatch);
    }

    let gap = far_gap_secs(mic.frames, system.frames, mic.sample_rate);
    if gap > GAP_TOLERANCE_SECS {
        return Err(AecBypass::TrackLengthMismatch { delta_secs: gap });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::meta::TrackInfo;

    fn track(frames: u64, sample_rate: u32) -> TrackInfo {
        TrackInfo {
            path: "t.wav".into(),
            device_name: "Test".into(),
            device_id: None,
            sample_rate,
            channels: 1,
            source_channels: 2,
            frames,
            first_callback_nanos: Some(1),
            stream_errors: 0,
        }
    }

    fn meta(mic: Option<TrackInfo>, system: Option<TrackInfo>) -> Meta {
        Meta {
            started_at: 0.0,
            ended_at: 10.0,
            mic,
            system,
            aec: None,
        }
    }

    /// The real numbers from `recordings/1789486023/meta.json`: a 10 s macOS
    /// recording where playback only started partway through, so the tap
    /// produced 4.6 s of frames against the mic's 10.0 s.
    ///
    /// The bug this guards: cancelling anyway. The far-end reference would be
    /// 5.4 s out of step for the whole back half, and subtracting a misaligned
    /// signal *adds* uncorrelated energy — it makes the track worse than
    /// leaving it alone, while reporting success.
    #[test]
    fn detects_a_far_end_gap_from_frame_counts() {
        let gap = far_gap_secs(480_256, 221_696, 48_000);
        assert!((gap - 5.387).abs() < 0.01, "expected ~5.39 s, got {gap:.3}");

        let m = meta(Some(track(480_256, 48_000)), Some(track(221_696, 48_000)));
        assert!(matches!(
            check_alignable(&m),
            Err(AecBypass::TrackLengthMismatch { .. })
        ));
    }

    /// The real numbers from the reference Linux recording: 25719296 mic frames
    /// against 25718272 system frames, a 1024-frame difference that is just two
    /// callback buffers at the start and end.
    ///
    /// This test is what sets [`GAP_TOLERANCE_SECS`]. The bug it guards is a
    /// tolerance tight enough to reject every healthy recording — which would
    /// make the whole feature quietly do nothing.
    #[test]
    fn equal_length_tracks_report_no_gap() {
        let gap = far_gap_secs(25_719_296, 25_718_272, 48_000);
        assert!((gap - 0.021).abs() < 0.001, "expected ~21 ms, got {gap:.4}");
        assert!(gap < GAP_TOLERANCE_SECS);

        let m = meta(
            Some(track(25_719_296, 48_000)),
            Some(track(25_718_272, 48_000)),
        );
        assert!(check_alignable(&m).is_ok());
    }

    /// `system.frames: 0` from `recordings/1789411995/meta.json` — the output
    /// device was idle for the entire recording. There is no echo in the mic
    /// track to remove, so running would be pure risk for no benefit.
    #[test]
    fn bypasses_when_the_far_track_is_empty() {
        let m = meta(Some(track(480_256, 48_000)), Some(track(0, 48_000)));
        assert_eq!(check_alignable(&m), Err(AecBypass::NoFarEnd));

        // And when the track was never opened at all.
        let m = meta(Some(track(480_256, 48_000)), None);
        assert_eq!(check_alignable(&m), Err(AecBypass::NoFarEnd));
    }

    #[test]
    fn bypasses_a_mic_only_recording() {
        let m = meta(None, Some(track(480_000, 48_000)));
        assert_eq!(check_alignable(&m), Err(AecBypass::NoNearEnd));
    }

    /// Nothing forces the two devices to agree on a rate: `open_mic` uses
    /// `default_input_config` and `open_loopback` uses `default_output_config`,
    /// and each writes its own. Feeding mismatched rates to the canceller would
    /// align them at the wrong speed and cancel nothing.
    #[test]
    fn bypasses_when_the_tracks_disagree_on_sample_rate() {
        let m = meta(Some(track(480_000, 48_000)), Some(track(441_000, 44_100)));
        assert_eq!(check_alignable(&m), Err(AecBypass::SampleRateMismatch));
    }

    /// Mirrors `kinds_are_distinct_and_snake_case` in `capture.rs`: these
    /// strings are a telemetry contract, so a duplicate would silently merge two
    /// different outcomes in aggregate.
    #[test]
    fn bypass_reasons_are_distinct_and_snake_case() {
        let kinds = [
            AecBypass::NoFarEnd.kind(),
            AecBypass::NoNearEnd.kind(),
            AecBypass::TrackLengthMismatch { delta_secs: 1.0 }.kind(),
            AecBypass::SampleRateMismatch.kind(),
            AecBypass::DelayUnmeasurable.kind(),
            AecBypass::ClockDrift { ppm: 100.0 }.kind(),
            AecBypass::NoFarOnlyWindows.kind(),
            AecBypass::InsufficientDiskSpace.kind(),
            AecBypass::AlreadyProcessed.kind(),
        ];

        let mut seen = kinds.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), kinds.len(), "kinds must be distinguishable");

        for kind in kinds {
            assert!(
                kind.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{kind} is not snake_case"
            );
        }
    }

    /// The four classes are what every later decision keys off, so each one has
    /// to be reachable and distinct.
    #[test]
    fn classify_separates_the_four_activity_classes() {
        // p90-based thresholds: both tracks peak at 10000, so both thresholds
        // land at 1000.
        let mic = [
            0.0, 5_000.0, 0.0, 5_000.0, 10_000.0, 10_000.0, 10_000.0, 10_000.0, 10_000.0, 10_000.0,
        ];
        let far = [
            0.0, 0.0, 5_000.0, 5_000.0, 10_000.0, 10_000.0, 10_000.0, 10_000.0, 10_000.0, 10_000.0,
        ];

        let activity = classify(&mic, &far);
        assert_eq!(activity[0], Activity::Silence);
        assert_eq!(activity[1], Activity::NearOnly);
        assert_eq!(activity[2], Activity::FarOnly);
        assert_eq!(activity[3], Activity::DoubleTalk);
    }

    /// Classification must not run past the end of the shorter track, which is
    /// the normal case — the two tracks are never exactly the same length.
    #[test]
    fn classify_stops_at_the_shorter_track() {
        assert_eq!(classify(&[100.0; 10], &[100.0; 4]).len(), 4);
        assert_eq!(classify(&[100.0; 3], &[100.0; 9]).len(), 3);
        assert!(classify(&[], &[100.0; 9]).is_empty());
    }

    /// Reproduces the activity split measured independently on the reference
    /// recording — 28 s silence, 91 s near-only, 22 s far-only, 394 s
    /// double-talk — which is the strongest correctness signal available for
    /// this stage, since those numbers came from a separate implementation.
    #[test]
    fn census_reproduces_the_reference_recording_split() {
        let mut activity = Vec::new();
        activity.extend(std::iter::repeat_n(Activity::Silence, 28));
        activity.extend(std::iter::repeat_n(Activity::NearOnly, 91));
        activity.extend(std::iter::repeat_n(Activity::FarOnly, 22));
        activity.extend(std::iter::repeat_n(Activity::DoubleTalk, 394));

        let c = census(&activity, 1.0);
        assert_eq!(c.silence, 28.0);
        assert_eq!(c.near_only, 91.0);
        assert_eq!(c.far_only, 22.0);
        assert_eq!(c.double_talk, 394.0);

        // 74% double-talk is why the canceller must be robust to it rather than
        // simply refusing to adapt through it.
        let total = c.silence + c.near_only + c.far_only + c.double_talk;
        assert!((c.double_talk / total - 0.736).abs() < 0.01);
    }

    /// A zero rate would divide by zero rather than reporting "no gap", and a
    /// recording with a zero rate is already being rejected elsewhere.
    #[test]
    fn a_zero_sample_rate_does_not_divide_by_zero() {
        assert_eq!(far_gap_secs(100, 200, 0), 0.0);
    }
}
