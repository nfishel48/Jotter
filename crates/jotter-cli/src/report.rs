//! The `--json` shapes of what a recording and its passes produced.
//!
//! Explicit structs mapped from the library's reports rather than `Serialize`
//! derived on the reports themselves. The library's types are for programs that
//! link it, and change shape when a pass grows a new figure; these are a wire
//! format that agents parse, and must only ever grow. Keeping the two apart is
//! what lets the library refactor a report without breaking a script.
//!
//! Numbers keep their natural units — seconds, decibels, milliseconds — and a
//! figure that does not apply is `null` rather than absent, so a reader can
//! rely on every key being present.

use std::path::{Path, PathBuf};

use serde::Serialize;

use jotter::audio::meta::{Meta, TrackInfo};
#[cfg(any(feature = "aec", feature = "transcribe"))]
use jotter::audio::{Skip, StageOutcome};

/// A finished recording: what `record`, and `stop`, return.
#[derive(Serialize)]
pub struct RecordingJson {
    pub dir: PathBuf,
    pub duration_secs: f64,
    pub tracks: TracksJson,
    /// Seconds the system track started after the mic track. `null` unless
    /// both tracks produced audio.
    pub track_offset_secs: Option<f64>,
    /// `null` when the passes were not run (`stop --no-finish`).
    pub finish: Option<FinishJson>,
    /// The transcript the passes just wrote, if they wrote one.
    pub transcript_path: Option<PathBuf>,
}

impl RecordingJson {
    pub fn new(dir: &Path, meta: &Meta) -> Self {
        Self {
            dir: dir.to_path_buf(),
            duration_secs: meta.duration_secs(),
            tracks: TracksJson {
                mic: meta.mic.as_ref().map(|t| TrackJson::new(dir, t)),
                system: meta.system.as_ref().map(|t| TrackJson::new(dir, t)),
            },
            track_offset_secs: meta.track_offset_secs(),
            finish: None,
            transcript_path: None,
        }
    }

    /// Attach what [`jotter::audio::finish`] did.
    pub fn with_finish(mut self, report: &jotter::audio::FinishReport) -> Self {
        self.transcript_path = report.transcript_path().map(Path::to_path_buf);
        self.finish = Some(FinishJson::new(report));
        self
    }
}

/// One entry per source, `null` for a source that was not recorded.
#[derive(Serialize)]
pub struct TracksJson {
    pub mic: Option<TrackJson>,
    pub system: Option<TrackJson>,
}

#[derive(Serialize)]
pub struct TrackJson {
    pub path: PathBuf,
    pub device: String,
    pub device_id: Option<String>,
    pub sample_rate: u32,
    pub source_channels: u16,
    pub frames: u64,
    pub secs: f64,
    /// Non-zero means the track is suspect: the stream reported errors while
    /// recording, and may have stopped partway.
    pub stream_errors: u64,
}

impl TrackJson {
    fn new(dir: &Path, track: &TrackInfo) -> Self {
        Self {
            path: track.resolve(dir),
            device: track.device_name.clone(),
            device_id: track.device_id.clone(),
            sample_rate: track.sample_rate,
            source_channels: track.source_channels,
            frames: track.frames,
            secs: track.frames as f64 / track.sample_rate.max(1) as f64,
            stream_errors: track.stream_errors,
        }
    }
}

/// A pass that looked at the recording and decided not to write anything.
#[cfg(any(feature = "aec", feature = "transcribe"))]
#[derive(Serialize)]
pub struct Declined {
    /// Stable snake_case reason, the same string `meta.json` records.
    pub kind: &'static str,
    pub message: String,
}

#[cfg(any(feature = "aec", feature = "transcribe"))]
impl Declined {
    fn new(reason: &impl jotter::audio::stage::DeclineReason) -> Self {
        Self {
            kind: reason.kind(),
            message: reason.to_string(),
        }
    }
}

/// What [`jotter::audio::finish`] did, one key per pass this build contains.
#[derive(Serialize)]
pub struct FinishJson {
    #[cfg(feature = "aec")]
    pub aec: StageJson<AecJson>,
    #[cfg(feature = "transcribe")]
    pub transcribe: StageJson<TranscriptJson>,
    #[cfg(feature = "diarize")]
    pub diarize: StageJson<DiarizeJson>,
}

impl FinishJson {
    // A build without any pass reports an empty object.
    #[cfg_attr(
        not(any(feature = "aec", feature = "transcribe")),
        allow(unused_variables)
    )]
    fn new(report: &jotter::audio::FinishReport) -> Self {
        Self {
            #[cfg(feature = "aec")]
            aec: StageJson::new(&report.aec, |r| AecJson::new(r, false)),
            #[cfg(feature = "transcribe")]
            transcribe: StageJson::new(&report.transcribe, |r| TranscriptJson::new(r, false)),
            #[cfg(feature = "diarize")]
            diarize: StageJson::new(&report.diarize, |r| DiarizeJson::new(r, false)),
        }
    }
}

/// One pass of [`FinishJson`]. `status` is the discriminant; a pass that ran
/// carries its report's fields beside it.
#[cfg(any(feature = "aec", feature = "transcribe"))]
#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum StageJson<R> {
    Ran(R),
    /// Not started. `reason` is `Skip::kind()`: `disabled`, `no_audio` or
    /// `no_transcript`.
    Skipped {
        reason: &'static str,
        message: String,
    },
    Failed {
        message: String,
    },
}

#[cfg(any(feature = "aec", feature = "transcribe"))]
impl<R> StageJson<R> {
    fn new<T, E: std::fmt::Display>(
        outcome: &StageOutcome<T, E>,
        report: impl FnOnce(&T) -> R,
    ) -> Self {
        match outcome {
            StageOutcome::Ran(r) => Self::Ran(report(r)),
            StageOutcome::Skipped(skip) => Self::skipped(*skip),
            StageOutcome::Failed(e) => Self::Failed {
                message: e.to_string(),
            },
        }
    }

    fn skipped(skip: Skip) -> Self {
        Self::Skipped {
            reason: skip.kind(),
            message: skip.to_string(),
        }
    }
}

#[cfg(feature = "aec")]
#[derive(Serialize)]
pub struct AecJson {
    pub dry_run: bool,
    pub declined: Option<Declined>,
    /// Seconds of each kind of activity across the recording. `null` when the
    /// pass stopped before measuring any.
    pub activity: Option<ActivityJson>,
    /// `null` when the pass declined before estimating a delay.
    pub delay: Option<DelayJson>,
    /// Echo removed where system audio was playing. `null` when not measured
    /// (dry run, declined) or not measurable (no echo-only passages).
    pub erle_db: Option<f32>,
    /// Level change on your own voice. Clearly negative means the filter cut
    /// into it, and `mic.wav` is still the one to use. `null` when unverified.
    pub near_gain_db: Option<f32>,
    /// Seconds of system audio missing because the output device was idle.
    pub far_gap_secs: f32,
    /// `mic_aec.wav`, when the pass wrote it.
    pub output: Option<PathBuf>,
}

#[cfg(feature = "aec")]
#[derive(Serialize)]
pub struct ActivityJson {
    pub silence_secs: f32,
    pub you_secs: f32,
    pub them_secs: f32,
    pub both_secs: f32,
}

#[cfg(feature = "aec")]
#[derive(Serialize)]
pub struct DelayJson {
    pub ms: f32,
    /// `measured`, `meta_offset` or `zero`.
    pub source: &'static str,
    pub segments: usize,
    pub spread_ms: f32,
    pub confidence: f32,
    pub drift_ppm: f32,
    /// The canceller's own estimate, as a cross-check.
    pub aec3_ms: Option<u32>,
}

#[cfg(feature = "aec")]
impl AecJson {
    pub fn new(report: &jotter::audio::process::AecReport, dry_run: bool) -> Self {
        let census = &report.census;
        let measured = census.silence + census.near_only + census.far_only + census.double_talk;
        let declined = report.bypass.as_ref().map(Declined::new);
        let ran = declined.is_none();
        let cancelled = ran && !dry_run;
        Self {
            dry_run,
            activity: (measured > 0.0).then_some(ActivityJson {
                silence_secs: census.silence,
                you_secs: census.near_only,
                them_secs: census.far_only,
                both_secs: census.double_talk,
            }),
            delay: ran.then(|| DelayJson {
                ms: report.delay.frames as f32 * 1_000.0 / report.config.sample_rate.max(1) as f32,
                source: report.delay.source.as_str(),
                segments: report.delay.segments_used,
                spread_ms: report.delay.spread_ms,
                confidence: report.delay.confidence,
                drift_ppm: report.delay.drift_ppm,
                aec3_ms: report.stats.reported_delay_ms,
            }),
            erle_db: report.stats.erle_db.filter(|_| cancelled),
            near_gain_db: report.stats.near_gain_db.filter(|_| cancelled),
            far_gap_secs: report.far_gap_secs,
            output: report.output.clone(),
            declined,
        }
    }
}

#[cfg(feature = "transcribe")]
#[derive(Serialize)]
pub struct TranscriptJson {
    pub dry_run: bool,
    pub declined: Option<Declined>,
    pub model: &'static str,
    pub engine: &'static str,
    /// Seconds of audio read, across the tracks transcribed.
    pub audio_secs: f32,
    /// Seconds the voice-activity pass called speech.
    pub speech_secs: f32,
    pub segments: u32,
    /// Segments from your microphone and from everyone else. One of these at
    /// zero usually means that track was silent.
    pub mic_segments: u32,
    pub system_segments: u32,
    pub words: u32,
    pub elapsed_secs: f32,
    /// `transcript.json`, when the pass wrote it.
    pub output: Option<PathBuf>,
}

#[cfg(feature = "transcribe")]
impl TranscriptJson {
    pub fn new(report: &jotter::audio::transcribe::TranscriptReport, dry_run: bool) -> Self {
        Self {
            dry_run,
            declined: report.decline.as_ref().map(Declined::new),
            model: report.model_id,
            engine: report.engine,
            audio_secs: report.audio_secs,
            speech_secs: report.speech_secs,
            segments: report.segments,
            mic_segments: report.mic_segments,
            system_segments: report.system_segments,
            words: report.words,
            elapsed_secs: report.elapsed_secs,
            output: report.output.clone(),
        }
    }
}

#[cfg(feature = "diarize")]
#[derive(Serialize)]
pub struct DiarizeJson {
    pub dry_run: bool,
    pub declined: Option<Declined>,
    pub segmentation_model: &'static str,
    pub embedding_model: &'static str,
    pub engine: &'static str,
    /// Seconds of system audio read.
    pub audio_secs: f32,
    pub speakers: u32,
    /// System segments considered, and how many got a speaker. The gap is
    /// segments the clustering could not place, which are left unlabelled.
    pub system_segments: u32,
    pub attributed_segments: u32,
    pub elapsed_secs: f32,
    /// The transcript it labelled in place, when it did.
    pub output: Option<PathBuf>,
}

#[cfg(feature = "diarize")]
impl DiarizeJson {
    pub fn new(report: &jotter::audio::diarize::DiarizeReport, dry_run: bool) -> Self {
        Self {
            dry_run,
            declined: report.decline.as_ref().map(Declined::new),
            segmentation_model: report.segmentation_model_id,
            embedding_model: report.embedding_model_id,
            engine: report.engine,
            audio_secs: report.audio_secs,
            speakers: report.speakers,
            system_segments: report.system_segments,
            attributed_segments: report.attributed_segments,
            elapsed_secs: report.elapsed_secs,
            output: report.output.clone(),
        }
    }
}
