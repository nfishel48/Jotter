//! What happens to a recording once it stops: every offline pass, in order.
//!
//! Each pass is a stage in its own module ([`crate::audio::stage`] has the
//! shared mechanics) and can be run alone — `jotter process`, `jotter
//! transcribe` and `jotter diarize` do exactly that. This module is the one
//! place that knows the *sequence*, and the conditions under which a pass is
//! not worth starting. It exists so that there is exactly one copy of both:
//! the `jotter record` command, a background session and any host application
//! all finish a recording by calling [`finish`], and none of them can drift
//! into running transcription before echo cancellation.
//!
//! The order is not a preference. Transcription reads whichever mic track
//! [`Meta::preferred_mic_path`] hands back, so it must follow the echo pass that
//! decides which one that is; diarization labels the segments transcription
//! wrote, so it must follow that.
//!
//! [`Meta::preferred_mic_path`]: crate::audio::meta::Meta::preferred_mic_path

use std::path::Path;

#[cfg(any(feature = "aec", feature = "transcribe"))]
use crate::audio::meta::Meta;
use crate::config::Settings;

/// Which passes to run.
///
/// Flags rather than a list of stages, because the order is fixed — see the
/// module docs — and the only real choice is whether each one happens. A flag
/// for a stage this build was compiled without is ignored: there is nothing to
/// run, and [`FinishReport`] has no field to report it in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FinishOptions {
    /// Remove speaker echo from the mic track, when both tracks captured audio.
    pub aec: bool,
    /// Turn the recording into `transcript.json`, when any track has audio.
    pub transcribe: bool,
    /// Label who said each system-track segment, when a transcript exists.
    pub diarize: bool,
    /// How many people were on the call. `None` makes the diarization pass
    /// decline — it will not guess; see `audio::diarize::DiarizeOptions`.
    pub speakers: Option<u8>,
}

impl FinishOptions {
    /// The user's stored preferences: what the `jotter` command does when no
    /// flag overrides them.
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            aec: settings.aec_enabled,
            transcribe: settings.transcribe_enabled,
            diarize: settings.diarize_enabled,
            speakers: settings.speaker_count(),
        }
    }
}

/// One pass of the pipeline, for progress reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishStage {
    Aec,
    Transcribe,
    Diarize,
}

impl FinishStage {
    /// Stable identifier, matching `Stage::name` for the same pass.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Aec => "aec",
            Self::Transcribe => "transcribe",
            Self::Diarize => "diarize",
        }
    }
}

/// Why a pass was not started.
///
/// Distinct from a stage's own decline. A decline is the stage looking at the
/// recording and saying no, and it is recorded in `meta.json` so the recording
/// carries the reason. These are answered before any stage runs, from
/// `meta.json` alone, and deliberately leave no trace there: a pass that was
/// switched off, or had nothing to read, has nothing worth recording against
/// the recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// Switched off in [`FinishOptions`].
    Disabled,
    /// The tracks this pass reads captured no audio: both are needed to cancel
    /// echo, and at least one to transcribe. A denied permission is the usual
    /// cause, and the track report already says so.
    NoAudio,
    /// Nothing to label — transcription was off, or declined.
    NoTranscript,
}

impl Skip {
    /// A stable snake_case name, for machine-readable output.
    pub fn kind(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::NoAudio => "no_audio",
            Self::NoTranscript => "no_transcript",
        }
    }
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Disabled => "switched off",
            Self::NoAudio => "no audio was captured on the tracks it reads",
            Self::NoTranscript => "there is no transcript to label",
        })
    }
}

/// What became of one pass.
#[derive(Debug)]
pub enum StageOutcome<R, E> {
    /// Not started, and why.
    Skipped(Skip),
    /// The stage ran. Its report says whether it wrote its artifact or
    /// declined — and a decline is also recorded in `meta.json`.
    Ran(R),
    /// The stage hit an error partway. The recording is untouched: every
    /// stage writes through a temporary file, and the audio is never modified.
    Failed(E),
}

impl<R, E> StageOutcome<R, E> {
    /// The report, when the stage ran.
    pub fn report(&self) -> Option<&R> {
        match self {
            Self::Ran(report) => Some(report),
            Self::Skipped(_) | Self::Failed(_) => None,
        }
    }
}

/// What [`finish`] did, one field per pass compiled into this build.
///
/// Never an `Err` as a whole, because nothing here can fail the recording: the
/// audio and `meta.json` were on disk before the first pass started, and each
/// pass can be redone on its own later. A caller that exits non-zero on a
/// failed transcription would be telling its user the capture failed when it
/// did not.
#[derive(Debug)]
pub struct FinishReport {
    #[cfg(feature = "aec")]
    pub aec: StageOutcome<crate::audio::process::AecReport, crate::audio::process::ProcessError>,
    #[cfg(feature = "transcribe")]
    pub transcribe: StageOutcome<
        crate::audio::transcribe::TranscriptReport,
        crate::audio::transcribe::TranscribeError,
    >,
    #[cfg(feature = "diarize")]
    pub diarize:
        StageOutcome<crate::audio::diarize::DiarizeReport, crate::audio::diarize::DiarizeError>,
}

impl FinishReport {
    /// The transcript this call wrote, if it wrote one. Diarization labels the
    /// same file in place, so this is the final transcript either way.
    pub fn transcript_path(&self) -> Option<&Path> {
        #[cfg(feature = "transcribe")]
        {
            self.transcribe.report()?.output.as_deref()
        }
        #[cfg(not(feature = "transcribe"))]
        {
            None
        }
    }
}

/// Called with `0.0` as each pass starts and `1.0` as it ends, and in between
/// with whatever fraction the pass itself reports. Echo cancellation reports
/// nothing in between; transcription reports seconds of audio consumed;
/// diarization only the part before its single model call.
pub type FinishProgressFn<'a> = &'a mut dyn FnMut(FinishStage, f32);

/// Run every enabled pass over a finished recording directory, in order.
///
/// Blocks for as long as the passes take — minutes, for transcribing a long
/// meeting — and holds no audio device, so it can run on any thread, or in a
/// different process from the one that recorded.
pub fn finish(dir: &Path, options: &FinishOptions) -> FinishReport {
    finish_with_progress(dir, options, &mut |_, _| {})
}

/// [`finish`], reporting progress as it goes.
// A build with no stage compiled in has nothing to run, and so nothing to read
// the arguments for.
#[cfg_attr(
    not(any(feature = "aec", feature = "transcribe")),
    allow(unused_variables)
)]
pub fn finish_with_progress(
    dir: &Path,
    options: &FinishOptions,
    progress: FinishProgressFn<'_>,
) -> FinishReport {
    // Read once, for the checks that make starting a pass pointless. An
    // unreadable `meta.json` answers none of them, so every enabled pass is
    // attempted and fails with its own error — which says what was wrong with
    // the file far better than a skip here could.
    #[cfg(any(feature = "aec", feature = "transcribe"))]
    let meta = Meta::read(&dir.join("meta.json")).ok();

    #[cfg(feature = "aec")]
    let aec = {
        use crate::audio::process;

        if !options.aec {
            StageOutcome::Skipped(Skip::Disabled)
        } else if meta.as_ref().is_some_and(|m| !has_audio(m, Needs::Both)) {
            // Both, or there is nothing to cancel against. The stage would
            // decline too, but it would record that against a single-track
            // recording nobody expected echo cancellation on.
            StageOutcome::Skipped(Skip::NoAudio)
        } else {
            progress(FinishStage::Aec, 0.0);
            // One blocking call with nothing to report from inside it.
            let result = process::run(dir, process::ProcessOptions::default());
            progress(FinishStage::Aec, 1.0);
            outcome(result)
        }
    };

    #[cfg(feature = "transcribe")]
    let transcribe = {
        use crate::audio::transcribe;

        if !options.transcribe {
            StageOutcome::Skipped(Skip::Disabled)
        } else if meta.as_ref().is_some_and(|m| !has_audio(m, Needs::Either)) {
            // Knowable without loading a 660 MB model. Everything else — a
            // model that is not downloaded, a recording with no speech — is the
            // stage's call, and it records the reason in `meta.json`.
            StageOutcome::Skipped(Skip::NoAudio)
        } else {
            progress(FinishStage::Transcribe, 0.0);
            let result = transcribe::run_with_progress(
                dir,
                transcribe::TranscribeOptions::default(),
                &mut |fraction| progress(FinishStage::Transcribe, fraction),
            );
            progress(FinishStage::Transcribe, 1.0);
            outcome(result)
        }
    };

    #[cfg(feature = "diarize")]
    let diarize = {
        use crate::audio::diarize;

        // Re-read rather than reusing `meta`: transcription has just rewritten
        // the file, and whether it produced a transcript is the whole question.
        let has_transcript = || {
            Meta::read(&dir.join("meta.json"))
                .map(|m| m.transcript.as_ref().is_some_and(|t| t.path.is_some()))
                // Unreadable: attempt the pass, for the reason given above.
                .unwrap_or(true)
        };

        if !options.diarize {
            StageOutcome::Skipped(Skip::Disabled)
        } else if !has_transcript() {
            // Not a decline: the stage would record `no_transcript` against a
            // recording whose transcription was off or declined for a reason
            // already on record, and that second entry would say nothing new.
            StageOutcome::Skipped(Skip::NoTranscript)
        } else {
            progress(FinishStage::Diarize, 0.0);
            let options = diarize::DiarizeOptions {
                speakers: options.speakers,
                ..Default::default()
            };
            let result = diarize::run_with_progress(dir, options, &mut |fraction| {
                progress(FinishStage::Diarize, fraction)
            });
            progress(FinishStage::Diarize, 1.0);
            outcome(result)
        }
    };

    FinishReport {
        #[cfg(feature = "aec")]
        aec,
        #[cfg(feature = "transcribe")]
        transcribe,
        #[cfg(feature = "diarize")]
        diarize,
    }
}

#[cfg(any(feature = "aec", feature = "transcribe"))]
fn outcome<R, E>(result: Result<R, E>) -> StageOutcome<R, E> {
    match result {
        Ok(report) => StageOutcome::Ran(report),
        Err(e) => StageOutcome::Failed(e),
    }
}

#[cfg(any(feature = "aec", feature = "transcribe"))]
#[derive(Clone, Copy)]
enum Needs {
    #[cfg(feature = "aec")]
    Both,
    #[cfg(feature = "transcribe")]
    Either,
}

/// Whether the tracks a pass reads captured anything. Frames, not file
/// existence: a denied permission still produces a valid, empty WAV.
#[cfg(any(feature = "aec", feature = "transcribe"))]
fn has_audio(meta: &Meta, needs: Needs) -> bool {
    let mut captured = [meta.mic.as_ref(), meta.system.as_ref()]
        .into_iter()
        .map(|track| track.is_some_and(|t| t.frames > 0));
    match needs {
        #[cfg(feature = "aec")]
        Needs::Both => captured.all(|c| c),
        #[cfg(feature = "transcribe")]
        Needs::Either => captured.any(|c| c),
    }
}

#[cfg(all(test, any(feature = "aec", feature = "transcribe")))]
mod tests {
    use super::*;
    use crate::audio::meta::TrackInfo;
    use std::path::PathBuf;

    fn track(frames: u64) -> TrackInfo {
        TrackInfo {
            path: "mic.wav".into(),
            device_name: "Test".into(),
            device_id: None,
            sample_rate: 48_000,
            channels: 1,
            source_channels: 1,
            frames,
            first_callback_nanos: Some(1),
            stream_errors: 0,
        }
    }

    /// A recording directory holding only `meta.json`: every check here is
    /// answered before any audio would be opened.
    fn recording(name: &str, mic: Option<TrackInfo>, system: Option<TrackInfo>) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("jotter-pipeline-{name}-{unique}"));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Meta {
            started_at: 0.0,
            ended_at: 10.0,
            mic,
            system,
            aec: None,
            transcript: None,
            diarization: None,
            live: None,
        }
        .write(&dir.join("meta.json"))
        .expect("write meta");
        dir
    }

    fn all_on() -> FinishOptions {
        FinishOptions {
            aec: true,
            transcribe: true,
            diarize: true,
            speakers: Some(2),
        }
    }

    /// A skip is answered from `meta.json` and must leave it exactly as it was:
    /// the stages' own declines are what get recorded, and a pass that never
    /// started has no business writing one.
    fn assert_meta_untouched(dir: &Path) {
        let meta = Meta::read(&dir.join("meta.json")).expect("meta");
        assert!(meta.aec.is_none(), "aec block written: {:?}", meta.aec);
        assert!(meta.transcript.is_none(), "transcript block written");
        assert!(meta.diarization.is_none(), "diarization block written");
    }

    #[test]
    fn a_recording_where_nothing_was_captured_runs_no_pass() {
        let dir = recording("silent", Some(track(0)), Some(track(0)));
        let report = finish(&dir, &all_on());

        #[cfg(feature = "aec")]
        assert!(matches!(report.aec, StageOutcome::Skipped(Skip::NoAudio)));
        #[cfg(feature = "transcribe")]
        assert!(matches!(
            report.transcribe,
            StageOutcome::Skipped(Skip::NoAudio)
        ));
        // Transcription never started, so there is nothing to label.
        #[cfg(feature = "diarize")]
        assert!(matches!(
            report.diarize,
            StageOutcome::Skipped(Skip::NoTranscript)
        ));
        assert_eq!(report.transcript_path(), None);
        assert_meta_untouched(&dir);
    }

    #[test]
    fn switched_off_passes_are_not_started_even_with_audio_to_read() {
        let dir = recording("off", Some(track(48_000)), Some(track(48_000)));
        let options = FinishOptions {
            aec: false,
            transcribe: false,
            diarize: false,
            speakers: Some(2),
        };
        let report = finish(&dir, &options);

        #[cfg(feature = "aec")]
        assert!(matches!(report.aec, StageOutcome::Skipped(Skip::Disabled)));
        #[cfg(feature = "transcribe")]
        assert!(matches!(
            report.transcribe,
            StageOutcome::Skipped(Skip::Disabled)
        ));
        #[cfg(feature = "diarize")]
        assert!(matches!(
            report.diarize,
            StageOutcome::Skipped(Skip::Disabled)
        ));
        assert_meta_untouched(&dir);
    }

    /// `--only mic`: one good track is enough to transcribe, and not enough to
    /// cancel echo. The two passes must not share one "has audio" rule.
    #[cfg(feature = "aec")]
    #[test]
    fn a_single_track_recording_skips_echo_cancellation() {
        let dir = recording("mic-only", Some(track(48_000)), None);
        let options = FinishOptions {
            transcribe: false,
            diarize: false,
            ..all_on()
        };
        let report = finish(&dir, &options);

        assert!(matches!(report.aec, StageOutcome::Skipped(Skip::NoAudio)));
        assert_meta_untouched(&dir);
    }

    /// The contract callers rely on: a pass that cannot even read the
    /// recording lands in the report, never in a panic or an `Err` that would
    /// have them report the capture itself as failed.
    #[test]
    fn a_pass_that_cannot_read_the_recording_is_reported_as_failed() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let missing = std::env::temp_dir().join(format!("jotter-pipeline-missing-{unique}"));
        let report = finish(&missing, &all_on());

        #[cfg(feature = "aec")]
        assert!(matches!(report.aec, StageOutcome::Failed(_)));
        #[cfg(feature = "transcribe")]
        assert!(matches!(report.transcribe, StageOutcome::Failed(_)));
        // Unreadable is not "no transcript": the pass is attempted, and says
        // for itself what was wrong.
        #[cfg(feature = "diarize")]
        assert!(matches!(report.diarize, StageOutcome::Failed(_)));
        assert!(!missing.exists(), "a failed pass created the directory");
    }
}
