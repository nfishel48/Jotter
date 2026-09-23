//! The transcription pass over a finished recording.
//!
//! The second offline stage, after echo cancellation, and built on the same
//! mechanics: it reads a recording directory, writes one artifact beside the
//! audio, and records in `meta.json` what it did or why it declined. The shared
//! half of that lives in [`crate::audio::stage`]; what is here are the decisions
//! — which track to read, where to cut it, and when not to bother.
//!
//! # Both tracks, one timeline
//!
//! A meeting is two recordings. Transcribing them separately is the whole
//! argument for capturing them separately: overlapping speech does not collapse,
//! and "was this me or someone else" is answered by which file the audio came
//! out of rather than by inference. The two are then merged onto the mic track's
//! timeline — see [`transcript::Segment::shifted`] for why that shift is not
//! optional.
//!
//! The mic side is whatever [`Meta::preferred_mic_path`] hands back, so a
//! cancelled track is used only when the echo pass's own numbers cleared the
//! bar. The policy for that lives with `meta`, and this stage deliberately does
//! not second-guess it.
//!
//! # Why voice activity detection
//!
//! An hour of meeting will not go through a FastConformer encoder in one call,
//! and a fixed window would cut mid-word. Silero VAD gives speech-bounded
//! segments instead: bounded memory, timestamps that mean something, and — not
//! incidentally — exactly the segmentation a later diarization pass wants.
//!
//! Everything is resampled to 16 kHz once, up front. The recogniser would do it
//! for us, but the VAD would not, and having the two disagree about what a
//! sample index means is a class of bug worth designing out.
//!
//! # Extending this
//!
//! Two hooks are deliberately left visible rather than built:
//!
//! - [`Transcriber`] is the seam for a second engine. Parakeet is the only
//!   implementation today; a Whisper or SenseVoice pass is a new impl plus a
//!   [`crate::models`] catalogue entry, and nothing in the file-level flow below
//!   has to know.
//! - A domain dictionary — the meeting vocabulary that generic ASR reliably
//!   mangles, and whose damage propagates downstream (see the keyterm notes in
//!   `action_items_chunked.sh`, where "FedRAMP" became a person called Pat Ramp)
//!   — plugs into `OfflineRecognizerConfig`'s `hotwords_file` and
//!   `hotwords_score` in [`ParakeetTranscriber::create`]. Not wired up now
//!   because there is nowhere for a user to put such a list yet, and a dead
//!   option is worse than an absent one.

use std::path::{Path, PathBuf};
use std::time::Instant;

use sherpa_onnx::{
    LinearResampler, OfflineRecognizer, OfflineRecognizerConfig, OfflineTransducerModelConfig,
    SileroVadModelConfig, VadModelConfig, VoiceActivityDetector,
};

use crate::audio::meta::{Meta, TranscriptInfo};
use crate::audio::stage::{DeclineReason, Stage, StageRecord, wav};
use crate::audio::transcript::{self, Segment, Track, Transcript};
use crate::models::{self, Model, ResolvedModel, Role};

/// Bumped whenever a change would give a different transcript for the same
/// input, so a `transcript.json` left over from an older build is detectable
/// rather than being mistaken for a current one. A model change is one such
/// change, but it is recorded separately in `meta.transcript.model` because
/// that one is a user's choice rather than a property of the build.
pub const TRANSCRIBE_VERSION: u32 = 1;

/// Filename of the transcript, written beside the audio.
pub const OUTPUT_NAME: &str = "transcript.json";

// The detector's settings below are `pub` so the benchmark harness can record
// them alongside a word error rate. A WER measured under unknown segmentation
// is not reproducible, and re-declaring the numbers in `benchmarks/` would let
// the record drift away from what actually ran.

/// What the models are trained on, and therefore what everything here is
/// resampled to before anything looks at it.
pub const ENGINE_RATE: i32 = 16_000;

/// Samples per VAD call. Silero's own frame size at 16 kHz; feeding it anything
/// else is not a tuning choice, it is the wrong input.
const VAD_WINDOW: usize = 512;

/// How much audio the detector may hold while it makes up its mind. Has to
/// exceed [`MAX_SPEECH_SECS`] or a long segment is truncated by its own buffer.
pub const VAD_BUFFER_SECS: f32 = 60.0;

/// Silence that ends a segment.
///
/// Not the shortest detectable pause: cutting at every 250 ms breath fragments
/// sentences, which costs the recogniser the context it uses to disambiguate and
/// leaves a transcript that reads like a stutter. Half a second is a turn
/// boundary; anything less is someone thinking.
pub const MIN_SILENCE_SECS: f32 = 0.5;

/// Speech shorter than this is not a segment. Filters coughs, keyboard noise and
/// the click of a mute button without touching real one-word answers, which are
/// longer than they feel.
pub const MIN_SPEECH_SECS: f32 = 0.25;

/// The longest a single segment may run before it is cut regardless of silence.
///
/// A cap on the encoder's working set rather than a linguistic judgement:
/// somebody who talks for four minutes without a half-second pause should not
/// decide how much memory this takes.
pub const MAX_SPEECH_SECS: f32 = 20.0;

/// Silero's speech/not-speech threshold. Its default, and left alone — moving it
/// trades missed speech against transcribed silence, and there is no evidence
/// here for which way to go.
pub const VAD_THRESHOLD: f32 = 0.5;

// The two relationships between those numbers that are not free choices. Checked
// at compile time rather than in a test, because they are properties of the
// constants themselves: a build where they do not hold should not exist.
//
// A detector buffer shorter than the longest segment it may emit would truncate
// a long speaker by an implementation detail, and a minimum silence shorter than
// the minimum speech would let a pause end a segment that was never allowed to
// start.
const _: () = assert!(VAD_BUFFER_SECS > MAX_SPEECH_SECS);
const _: () = assert!(MIN_SILENCE_SECS > MIN_SPEECH_SECS);

/// How long before the detector admits to a segment that segment may begin.
///
/// Silero only calls speech once it has lasted [`MIN_SPEECH_SECS`], and
/// sherpa-onnx then backdates the start by two windows so the onset is not
/// clipped. A segment can therefore start this far before the moment the
/// detector first reports it; one more window covers how coarsely that moment
/// is observed. What [`Segmenter::settled_secs`] subtracts.
const DETECTION_LAG_SECS: f64 =
    MIN_SPEECH_SECS as f64 + 3.0 * VAD_WINDOW as f64 / ENGINE_RATE as f64;

/// Samples read from a track at a time.
///
/// The conversion to f32 and the resample are done a chunk at a time rather than
/// over the whole track, and that is a memory decision, not a tidiness one: an
/// hour of 48 kHz mono is 173 M samples, so materialising it as `f32` would cost
/// 690 MB on top of the 346 MB the `i16` track already occupies.
const CHUNK: usize = 48_000;

/// Why a pass declined to transcribe.
///
/// A stage must always be able to say "I decided not to, and here is why" — the
/// silent alternative, no output file and no explanation, is indistinguishable
/// from a crash.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscribeDecline {
    /// The model is not on disk. By far the most likely reason on a first run,
    /// and the one with a one-line fix.
    ModelMissing {
        model_id: &'static str,
        files: usize,
    },
    /// Neither track has any audio in it.
    NoAudio,
    /// Audio, but the detector found no speech anywhere in it.
    NoSpeech,
    /// A current `transcript.json` already exists. Re-run with `--force`.
    AlreadyTranscribed,
}

impl DeclineReason for TranscribeDecline {
    fn kind(&self) -> &'static str {
        match self {
            Self::ModelMissing { .. } => "model_missing",
            Self::NoAudio => "no_audio",
            Self::NoSpeech => "no_speech",
            Self::AlreadyTranscribed => "already_transcribed",
        }
    }
}

impl std::fmt::Display for TranscribeDecline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ModelMissing { model_id, files } => write!(
                f,
                "the {model_id} model is not downloaded ({files} file(s) missing) — \
                 run `jotter models pull`"
            ),
            Self::NoAudio => write!(f, "neither track captured any audio"),
            Self::NoSpeech => write!(f, "no speech was detected in either track"),
            Self::AlreadyTranscribed => {
                write!(f, "already transcribed — pass --force to redo it")
            }
        }
    }
}

/// The transcription stage.
///
/// A unit struct for the same reason [`crate::audio::process::Aec`] is: the pass
/// keeps nothing between runs, and this exists to hang the [`Stage`]
/// implementation on — which is what tells the shared re-run check where in
/// `meta.json` to look.
pub struct Transcribe;

impl Stage for Transcribe {
    type Decline = TranscribeDecline;

    fn name(&self) -> &'static str {
        "transcribe"
    }

    fn version(&self) -> u32 {
        TRANSCRIBE_VERSION
    }

    fn record<'m>(&self, meta: &'m Meta) -> Option<StageRecord<'m>> {
        meta.transcript.as_ref().map(|info| StageRecord {
            version: info.version,
            output: info.path.as_deref(),
            declined: info.declined.as_deref(),
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct TranscribeOptions {
    /// Report what would happen and write nothing.
    pub dry_run: bool,
    /// Overwrite an existing `transcript.json`.
    pub force: bool,
    /// Catalogue id, or `None` for [`models::DEFAULT_TRANSCRIPTION_MODEL`].
    pub model: Option<String>,
    /// Which tracks to read. Both, normally.
    pub tracks: Tracks,
}

/// Which of the two recordings to transcribe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tracks {
    #[default]
    Both,
    /// You only. Halves the work, and drops everyone else in the meeting.
    Mic,
    /// Everyone else only.
    System,
}

impl Tracks {
    fn wants(self, track: Track) -> bool {
        matches!(
            (self, track),
            (Self::Both, _) | (Self::Mic, Track::Mic) | (Self::System, Track::System)
        )
    }
}

/// What the pass did. `decline` set means nothing was written.
#[derive(Debug, Clone)]
pub struct TranscriptReport {
    /// Catalogue data, so `&'static str` rather than `String`. Not a
    /// nicety: telemetry property values are `&'static str` everywhere by
    /// convention, which is what makes it hard to send something
    /// user-shaped by accident, and a `String` here would be the first
    /// exception to that.
    pub model_id: &'static str,
    pub engine: &'static str,
    pub segments: u32,
    pub mic_segments: u32,
    pub system_segments: u32,
    pub words: u32,
    /// Seconds the detector called speech, summed over the tracks read.
    pub speech_secs: f32,
    /// Seconds of audio read, summed over the tracks read.
    pub audio_secs: f32,
    pub elapsed_secs: f32,
    pub decline: Option<TranscribeDecline>,
    pub output: Option<PathBuf>,
}

#[derive(Debug)]
pub enum TranscribeError {
    Io(std::io::Error),
    Wav(hound::Error),
    /// sherpa-onnx refused to build a recogniser, a detector, or a resampler.
    /// It reports failure as a null pointer and logs the reason to stderr, so
    /// there is nothing more specific to pass on than which one failed.
    Engine(&'static str),
}

impl std::fmt::Display for TranscribeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Wav(e) => write!(f, "{e}"),
            Self::Engine(what) => write!(
                f,
                "could not initialise the {what} — the model files may be for a \
                 different engine version; see the log above"
            ),
        }
    }
}

impl std::error::Error for TranscribeError {}

impl From<std::io::Error> for TranscribeError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<hound::Error> for TranscribeError {
    fn from(e: hound::Error) -> Self {
        Self::Wav(e)
    }
}

/// Turns audio into text. The seam a second engine slots into.
///
/// Takes 16 kHz mono `f32` because that is what every model here wants and
/// because resampling is done once, before the detector — so an implementation
/// never has to think about the recording's native rate.
pub trait Transcriber {
    /// `None` when the segment decoded to nothing, which is ordinary: the
    /// detector is tuned to prefer keeping audio, so some segments are noise.
    fn transcribe(&self, samples: &[f32]) -> Option<String>;
}

/// NVIDIA Parakeet, and any other sherpa-onnx offline transducer.
pub struct ParakeetTranscriber {
    recognizer: OfflineRecognizer,
}

impl ParakeetTranscriber {
    pub fn create(model: &ResolvedModel) -> Result<Self, TranscribeError> {
        let path = |role: Role| model.path(role).map(|p| p.to_string_lossy().into_owned());

        let mut config = OfflineRecognizerConfig::default();
        config.model_config.transducer = OfflineTransducerModelConfig {
            encoder: path(Role::Encoder),
            decoder: path(Role::Decoder),
            joiner: path(Role::Joiner),
        };
        config.model_config.tokens = path(Role::Tokens);
        config.model_config.provider = Some("cpu".into());
        // Stated rather than left to sherpa-onnx's model-metadata sniffing. The
        // cost of it guessing wrong is not an error — it is a plausible-looking
        // transcript of nothing, which is the worst failure mode available here.
        config.model_config.model_type = Some("nemo_transducer".into());
        config.model_config.num_threads = threads();

        // A domain dictionary goes here: `config.hotwords_file` and
        // `config.hotwords_score`. See the module doc.

        let recognizer =
            OfflineRecognizer::create(&config).ok_or(TranscribeError::Engine("recogniser"))?;
        Ok(Self { recognizer })
    }
}

impl Transcriber for ParakeetTranscriber {
    fn transcribe(&self, samples: &[f32]) -> Option<String> {
        let stream = self.recognizer.create_stream();
        stream.accept_waveform(ENGINE_RATE, samples);
        self.recognizer.decode(&stream);

        let text = stream.get_result()?.text.trim().to_string();
        (!text.is_empty()).then_some(text)
    }
}

/// Threads for the recogniser.
///
/// Half the machine, at least one, capped at four. This runs after a meeting on
/// the user's own laptop, very likely while they are doing something else, so
/// taking every core would be rude — and past four the encoder stops scaling
/// anyway.
pub fn threads() -> i32 {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    (cores / 2).clamp(1, 4) as i32
}

/// How far through the audio the pass is, as a fraction.
///
/// Measured in seconds of audio consumed rather than segments decoded, because
/// the segment count is not known until the detector has seen the whole track —
/// and a bar that cannot appear until the work is nearly done is not a bar.
pub type ProgressFn<'a> = &'a mut dyn FnMut(f32);

/// Runs the pass over a recording directory.
///
/// Takes a path and opens its own files, holding no cpal types, so it is `Send`
/// and can run on any thread — or in another process from the one that
/// recorded.
pub fn run(dir: &Path, options: TranscribeOptions) -> Result<TranscriptReport, TranscribeError> {
    run_with_progress(dir, options, &mut |_| {})
}

/// [`run`], reporting progress as it goes.
pub fn run_with_progress(
    dir: &Path,
    options: TranscribeOptions,
    progress: ProgressFn<'_>,
) -> Result<TranscriptReport, TranscribeError> {
    let started = Instant::now();
    let meta_path = dir.join("meta.json");
    let meta = Meta::read(&meta_path)?;
    let model = model_for(options.model.as_deref());

    let mut report = TranscriptReport {
        model_id: model.id,
        engine: model.engine,
        segments: 0,
        mic_segments: 0,
        system_segments: 0,
        words: 0,
        speech_secs: 0.0,
        audio_secs: 0.0,
        elapsed_secs: 0.0,
        decline: None,
        output: None,
    };

    // Every check that can be answered without reading audio runs first, so a
    // recording this cannot handle costs no I/O — the same ordering as
    // `process::check_alignable`. Working out the sources is part of that:
    // `meta.json` already says how long each track is, so the figure the report
    // leads with is known before any decline is taken, rather than every
    // early return claiming the recording was empty.
    let sources = track_sources(&meta, dir, options.tracks);
    report.audio_secs = sources.iter().map(|s| s.secs).sum();

    let output_path = dir.join(OUTPUT_NAME);
    if output_path.exists() && !options.force && !options.dry_run && Transcribe.is_current(&meta) {
        return finish(
            dir,
            &meta_path,
            meta,
            report,
            Some(TranscribeDecline::AlreadyTranscribed),
            options,
            started,
        );
    }

    if sources.is_empty() {
        return finish(
            dir,
            &meta_path,
            meta,
            report,
            Some(TranscribeDecline::NoAudio),
            options,
            started,
        );
    }

    let resolved = match resolve(model) {
        Ok(resolved) => resolved,
        Err(files) => {
            return finish(
                dir,
                &meta_path,
                meta,
                report,
                Some(TranscribeDecline::ModelMissing {
                    model_id: model.id,
                    files,
                }),
                options,
                started,
            );
        }
    };

    // A dry run stops here, having established everything answerable without
    // running the model: which tracks there are, how long they are, and whether
    // the model is ready. That is what a dry run is for.
    if options.dry_run {
        return finish(dir, &meta_path, meta, report, None, options, started);
    }

    let transcriber = ParakeetTranscriber::create(&resolved.recognizer)?;
    let vad_model = resolved.vad_model();

    // The system stream starts at its own instant, so its timestamps have to be
    // moved onto the mic track's before the two can be interleaved. `None` — a
    // track that never produced a callback — means there is nothing to correct
    // for, not that the correction is zero by measurement.
    let offset = meta.track_offset_secs().unwrap_or(0.0);

    let total_secs = report.audio_secs.max(f32::MIN_POSITIVE);
    let mut done_secs = 0.0f32;

    let mut mic = Vec::new();
    let mut system = Vec::new();

    for source in &sources {
        let found = transcribe_track(
            &source.path,
            source.track,
            &transcriber,
            vad_model.clone(),
            &mut |consumed| progress((done_secs + consumed) / total_secs),
        )?;

        report.speech_secs += found.speech_secs;
        done_secs += source.secs;
        progress(done_secs / total_secs);

        match source.track {
            Track::Mic => mic = found.segments,
            Track::System => {
                system = found
                    .segments
                    .into_iter()
                    .map(|s| s.shifted(offset))
                    .collect()
            }
        }
    }

    let transcript = Transcript::new(model.id, transcript::merge(mic, system));
    if transcript.segments.is_empty() {
        return finish(
            dir,
            &meta_path,
            meta,
            report,
            Some(TranscribeDecline::NoSpeech),
            options,
            started,
        );
    }

    report.segments = transcript.segments.len() as u32;
    report.mic_segments = transcript.segments_from(Track::Mic);
    report.system_segments = transcript.segments_from(Track::System);
    report.words = transcript.words();

    transcript.write(&output_path)?;
    report.output = Some(output_path);

    finish(dir, &meta_path, meta, report, None, options, started)
}

/// A track worth reading, with everything needed to read it.
struct Source {
    path: PathBuf,
    track: Track,
    secs: f32,
}

/// The tracks that exist, have audio in them, and were asked for.
///
/// The mic side comes from [`Meta::preferred_mic_path`] rather than `mic.wav`
/// directly, which is what makes the echo pass' verdict count: it hands back the
/// cancelled track only when that pass's own numbers cleared the bar.
fn track_sources(meta: &Meta, dir: &Path, wanted: Tracks) -> Vec<Source> {
    let mut sources = Vec::new();

    let secs =
        |info: &crate::audio::meta::TrackInfo| info.frames as f32 / info.sample_rate.max(1) as f32;

    if wanted.wants(Track::Mic)
        && let Some(info) = meta.mic.as_ref().filter(|t| t.frames > 0)
        && let Some(path) = meta.preferred_mic_path(dir)
    {
        sources.push(Source {
            path,
            track: Track::Mic,
            secs: secs(info),
        });
    }

    if wanted.wants(Track::System)
        && let Some(info) = meta.system.as_ref().filter(|t| t.frames > 0)
    {
        sources.push(Source {
            path: info.resolve(dir),
            track: Track::System,
            secs: secs(info),
        });
    }

    sources
}

/// The catalogue entry for a model id, or the default one.
///
/// An id the catalogue does not know falls back to the default rather than
/// failing: the id reaches here from settings and flags, and a transcript from
/// the default model beats none. Shared with live transcription so the two
/// cannot pick different models for the same choice.
pub(crate) fn model_for(id: Option<&str>) -> &'static Model {
    id.and_then(models::find)
        .unwrap_or(models::DEFAULT_TRANSCRIPTION_MODEL)
}

/// Both models a run needs, resolved together.
pub(crate) struct Resolved {
    pub(crate) recognizer: ResolvedModel,
    pub(crate) vad: ResolvedModel,
}

impl Resolved {
    /// The detector's model file, in the form sherpa-onnx's config takes.
    pub(crate) fn vad_model(&self) -> Option<String> {
        self.vad
            .path(Role::Vad)
            .map(|p| p.to_string_lossy().into_owned())
    }
}

/// Check both models are on disk before any audio is read. `Err` is how many
/// files are missing between them.
///
/// Together, because they are equally fatal and a user who is missing both
/// should be told once. The counts are summed for the same reason
/// `MissingAssets` lists every file: "5 files missing" is one `jotter models
/// pull` away, and reporting them one at a time is not.
pub(crate) fn resolve(model: &'static Model) -> Result<Resolved, usize> {
    let recognizer = model.resolve();
    let vad = models::SILERO_VAD.resolve();

    match (recognizer, vad) {
        (Ok(recognizer), Ok(vad)) => Ok(Resolved { recognizer, vad }),
        (recognizer, vad) => Err(recognizer.as_ref().err().map_or(0, |e| e.problems.len())
            + vad.as_ref().err().map_or(0, |e| e.problems.len())),
    }
}

/// What one track yielded.
///
/// `pub` because [`transcribe_track`] has a second caller outside this module —
/// see the note there.
pub struct TrackResult {
    pub segments: Vec<Segment>,
    pub speech_secs: f32,
}

/// Read one track, cut it at the pauses, and decode each piece.
///
/// The track is read whole — the established pattern, and what `stage::wav`
/// exists for — but handed to the [`Segmenter`] a chunk at a time, and each
/// speech segment is decoded and dropped as it appears rather than collected
/// first. That keeps the peak cost of an hour-long meeting the `i16` track
/// plus a working set, instead of three copies of it at three sample rates.
///
/// `pub` for the `bench` binary, which measures word error rate over a corpus
/// and has to run *this* function rather than a copy of it: a benchmark of a
/// reimplementation of the pipeline measures the reimplementation. The same
/// reasoning that moved the shared stage mechanics into [`crate::audio::stage`]
/// rather than duplicating them into the transcription pass.
pub fn transcribe_track(
    path: &Path,
    track: Track,
    transcriber: &dyn Transcriber,
    vad_model: Option<String>,
    progress: &mut dyn FnMut(f32),
) -> Result<TrackResult, TranscribeError> {
    let audio = wav::read_track(path)?;
    let source_rate = audio.sample_rate.max(1);

    let mut segmenter = Segmenter::new(track, source_rate, vad_model)?;
    let mut segments = Vec::new();

    for (index, chunk) in audio.samples.chunks(CHUNK).enumerate() {
        segmenter.push(chunk, transcriber, &mut segments);

        // Cheap, and it does not have to be exact: this is what moves the bar.
        progress((index * CHUNK) as f32 / source_rate as f32);
    }
    segmenter.finish(transcriber, &mut segments);

    Ok(TrackResult {
        segments,
        speech_secs: segmenter.speech_secs(),
    })
}

/// One track on its way from audio to text: resampled to [`ENGINE_RATE`], cut
/// at the pauses, and each piece decoded as the detector lets go of it.
///
/// The offline pass and live transcription both run through this, and that is
/// why it exists: the two must build the detector and cut the audio the same
/// way, or the live transcript and the final one would disagree for reasons
/// nobody could see. Either way it takes audio a buffer at a time — the
/// offline pass in [`CHUNK`]-sized pieces of a finished file, the live one in
/// whatever the audio callback delivered.
pub(crate) struct Segmenter {
    track: Track,
    resampler: LinearResampler,
    vad: VoiceActivityDetector,
    /// The detector wants whole windows, and a resampled buffer is not a whole
    /// number of them. Whatever is left over waits here for the next one.
    pending: Vec<f32>,
    /// Samples handed to the detector so far, at [`ENGINE_RATE`].
    fed: u64,
    settled: f64,
    speech_secs: f32,
}

impl Segmenter {
    pub(crate) fn new(
        track: Track,
        source_rate: u32,
        vad_model: Option<String>,
    ) -> Result<Self, TranscribeError> {
        let resampler = LinearResampler::create(source_rate.max(1) as i32, ENGINE_RATE)
            .ok_or(TranscribeError::Engine("resampler"))?;

        let config = VadModelConfig {
            silero_vad: SileroVadModelConfig {
                model: vad_model,
                threshold: VAD_THRESHOLD,
                min_silence_duration: MIN_SILENCE_SECS,
                min_speech_duration: MIN_SPEECH_SECS,
                window_size: VAD_WINDOW as i32,
                max_speech_duration: MAX_SPEECH_SECS,
            },
            ten_vad: Default::default(),
            sample_rate: ENGINE_RATE,
            num_threads: 1,
            provider: Some("cpu".into()),
            debug: false,
        };
        let vad = VoiceActivityDetector::create(&config, VAD_BUFFER_SECS)
            .ok_or(TranscribeError::Engine("voice activity detector"))?;

        Ok(Self {
            track,
            resampler,
            vad,
            pending: Vec::with_capacity(VAD_WINDOW * 2),
            fed: 0,
            settled: 0.0,
            speech_secs: 0.0,
        })
    }

    /// Take the next stretch of the track, at its recorded rate, and decode
    /// every segment the detector finishes into `out`.
    pub(crate) fn push(
        &mut self,
        samples: &[i16],
        transcriber: &dyn Transcriber,
        out: &mut Vec<Segment>,
    ) {
        let float: Vec<f32> = samples.iter().map(|&s| s as f32 / 32_768.0).collect();
        self.pending.extend(self.resampler.resample(&float, false));
        self.feed(transcriber, out);
    }

    /// The track has ended: decode whatever is still held back.
    pub(crate) fn finish(&mut self, transcriber: &dyn Transcriber, out: &mut Vec<Segment>) {
        // The resampler holds a tail, the detector holds an unfinished segment,
        // and a partial window is still audio. Dropping any of the three
        // silently loses the end of the recording — which, in a meeting, is
        // where the actions are.
        self.pending.extend(self.resampler.resample(&[], true));
        self.feed(transcriber, out);
        if !self.pending.is_empty() {
            self.pending.resize(VAD_WINDOW, 0.0);
            self.feed(transcriber, out);
        }
        self.vad.flush();
        self.drain(transcriber, out);
    }

    /// Which track this is cutting.
    pub(crate) fn track(&self) -> Track {
        self.track
    }

    /// Seconds of the track the detector has seen, on the track's own
    /// timeline.
    pub(crate) fn fed_secs(&self) -> f64 {
        self.fed as f64 / ENGINE_RATE as f64
    }

    /// Every segment starting before this time, on the track's own timeline,
    /// has already been handed out.
    ///
    /// Stalls while the detector is inside a segment, because that segment's
    /// start is not known until it ends. Live transcription orders the two
    /// tracks by this; the offline pass has no use for it.
    pub(crate) fn settled_secs(&self) -> f64 {
        self.settled
    }

    /// Whether the detector is inside a segment it has not finished.
    pub(crate) fn in_speech(&self) -> bool {
        self.vad.detected()
    }

    /// Seconds the detector called speech so far.
    pub(crate) fn speech_secs(&self) -> f32 {
        self.speech_secs
    }

    /// Push whole windows into the detector and decode whatever comes out.
    fn feed(&mut self, transcriber: &dyn Transcriber, out: &mut Vec<Segment>) {
        let whole = self.pending.len() / VAD_WINDOW * VAD_WINDOW;
        for window in self.pending[..whole].chunks(VAD_WINDOW) {
            self.vad.accept_waveform(window);
        }
        self.pending.drain(..whole);
        self.fed += whole as u64;
        self.drain(transcriber, out);

        if !self.vad.detected() {
            self.settled = self.settled.max(self.fed_secs() - DETECTION_LAG_SECS);
        }
    }

    /// Decode every finished segment the detector is holding.
    ///
    /// Decoded here, as they appear, rather than collected and decoded
    /// afterwards: a segment's samples are the largest thing in flight, and
    /// keeping an hour of them alive to save a few function calls is the wrong
    /// trade.
    fn drain(&mut self, transcriber: &dyn Transcriber, out: &mut Vec<Segment>) {
        while let Some(speech) = self.vad.front() {
            let samples = speech.samples();
            // `start` counts samples fed to the detector, at the engine's rate,
            // so this is time from the beginning of *this* track. Putting it on
            // a shared timeline is the caller's job.
            let start = speech.start().max(0) as f64 / ENGINE_RATE as f64;
            let end = start + samples.len() as f64 / ENGINE_RATE as f64;
            self.speech_secs += (end - start) as f32;

            if let Some(text) = transcriber.transcribe(samples) {
                out.push(Segment {
                    start,
                    end,
                    track: self.track,
                    speaker: None,
                    text,
                });
            }
            self.vad.pop();
        }
    }
}

/// Record the outcome in `meta.json` — including, and especially, a decline.
///
/// The twin of `process::finish`, and the same contract: a pass that produced
/// nothing has to leave a reason behind, or it is indistinguishable from one
/// that crashed.
#[allow(clippy::too_many_arguments)]
fn finish(
    dir: &Path,
    meta_path: &Path,
    mut meta: Meta,
    mut report: TranscriptReport,
    decline: Option<TranscribeDecline>,
    options: TranscribeOptions,
    started: Instant,
) -> Result<TranscriptReport, TranscribeError> {
    report.decline = decline;
    report.elapsed_secs = started.elapsed().as_secs_f32();

    if options.dry_run {
        return Ok(report);
    }

    meta.transcript = Some(TranscriptInfo {
        // Relative to `dir`, the convention every path in `meta.json` follows.
        path: report
            .output
            .as_ref()
            .and_then(|p| p.strip_prefix(dir).ok())
            .map(|p| p.to_string_lossy().into_owned()),
        version: TRANSCRIBE_VERSION,
        model: report.model_id.to_string(),
        engine: report.engine.to_string(),
        segments: report.segments,
        mic_segments: report.mic_segments,
        system_segments: report.system_segments,
        words: report.words,
        speech_secs: report.speech_secs,
        audio_secs: report.audio_secs,
        elapsed_secs: report.elapsed_secs,
        declined: Transcribe.declined_kind(report.decline.as_ref()),
    });

    // A new transcript is a new set of segments, so any speaker labels a
    // diarization pass left behind referred to text that no longer exists —
    // `Transcript::write` has already overwritten them. Clearing the block is
    // what stops `meta.json` claiming otherwise, and what makes the next
    // diarization run rather than decline as already current.
    //
    // Only when a transcript was actually written: a decline changed nothing,
    // and dropping the record then would throw away a good pass because a later
    // one found no speech.
    if report.output.is_some() {
        meta.diarization = None;
    }

    meta.write(meta_path)?;

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::meta::{AecInfo, TrackInfo};

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

    fn meta(mic: Option<TrackInfo>, system: Option<TrackInfo>) -> Meta {
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
    }

    fn scratch(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("jotter-transcribe-{name}-{unique}"));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn report(output: Option<PathBuf>) -> TranscriptReport {
        TranscriptReport {
            model_id: "m",
            engine: "e",
            segments: 1,
            mic_segments: 1,
            system_segments: 0,
            words: 2,
            speech_secs: 1.0,
            audio_secs: 2.0,
            elapsed_secs: 0.0,
            decline: None,
            output,
        }
    }

    /// Re-transcribing rewrites `transcript.json` from scratch, so any speaker
    /// labels a diarization pass had written are gone — `Transcript::write` has
    /// already overwritten them. If `meta.diarization` survived that, the file
    /// would claim labels that no longer exist and the next diarization run
    /// would decline as already current, leaving the transcript permanently
    /// unattributed.
    #[test]
    fn a_new_transcript_clears_the_diarization_record() {
        let dir = scratch("clears-diarization");
        let meta_path = dir.join("meta.json");

        let mut m = meta(Some(track(48_000)), None);
        m.diarization = Some(crate::audio::meta::DiarizationInfo {
            path: Some(OUTPUT_NAME.into()),
            version: 1,
            speakers: 3,
            ..Default::default()
        });

        finish(
            &dir,
            &meta_path,
            m,
            report(Some(dir.join(OUTPUT_NAME))),
            None,
            TranscribeOptions::default(),
            Instant::now(),
        )
        .expect("finish");

        let written = Meta::read(&meta_path).expect("read");
        assert!(
            written.diarization.is_none(),
            "stale speaker labels were left claimed in meta.json"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A decline changed nothing, so the labels it did not touch are still
    /// good. Dropping the record here would throw away a finished pass because
    /// a later one found no speech.
    #[test]
    fn a_declined_transcription_leaves_existing_labels_alone() {
        let dir = scratch("keeps-diarization");
        let meta_path = dir.join("meta.json");

        let mut m = meta(Some(track(48_000)), None);
        m.diarization = Some(crate::audio::meta::DiarizationInfo {
            path: Some(OUTPUT_NAME.into()),
            version: 1,
            speakers: 3,
            ..Default::default()
        });

        finish(
            &dir,
            &meta_path,
            m,
            report(None),
            Some(TranscribeDecline::NoSpeech),
            TranscribeOptions::default(),
            Instant::now(),
        )
        .expect("finish");

        let written = Meta::read(&meta_path).expect("read");
        assert_eq!(written.diarization.expect("kept").speakers, 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A decline is reconsidered on every run, and `model_missing` is the one
    /// that matters most: the user fixes it with one command, and a block that
    /// read as finished work would mean the recording was never revisited.
    #[test]
    fn a_declined_pass_is_never_current() {
        let mut m = meta(Some(track(48_000)), None);
        m.transcript = Some(TranscriptInfo {
            path: None,
            version: TRANSCRIBE_VERSION,
            declined: Some("model_missing".into()),
            ..TranscriptInfo::default()
        });
        assert!(!Transcribe.is_current(&m));

        // A real transcript from this version is current; one from another
        // version is not, in either direction.
        m.transcript = Some(TranscriptInfo {
            path: Some(OUTPUT_NAME.into()),
            version: TRANSCRIBE_VERSION,
            ..TranscriptInfo::default()
        });
        assert!(Transcribe.is_current(&m));

        m.transcript = Some(TranscriptInfo {
            path: Some(OUTPUT_NAME.into()),
            version: TRANSCRIBE_VERSION + 1,
            ..TranscriptInfo::default()
        });
        assert!(!Transcribe.is_current(&m));
    }

    #[test]
    fn declines_are_recorded_by_kind_not_by_their_message() {
        assert_eq!(
            TranscribeDecline::ModelMissing {
                model_id: "parakeet-tdt-0.6b-v2-int8",
                files: 4,
            }
            .kind(),
            "model_missing"
        );
        assert_eq!(TranscribeDecline::NoSpeech.kind(), "no_speech");

        // The sentence, unlike the kind, has to tell the user what to do.
        let message = TranscribeDecline::ModelMissing {
            model_id: "parakeet-tdt-0.6b-v2-int8",
            files: 4,
        }
        .to_string();
        assert!(message.contains("jotter models pull"), "{message}");
    }

    /// A track that opened but captured nothing is the signature of a denied
    /// permission prompt. Handing an empty file to the recogniser would burn a
    /// model load to produce an empty transcript.
    #[test]
    fn a_silent_track_is_not_a_source() {
        let dir = Path::new("/recordings/x");

        assert!(track_sources(&meta(None, None), dir, Tracks::Both).is_empty());
        assert!(
            track_sources(&meta(Some(track(0)), Some(track(0))), dir, Tracks::Both).is_empty(),
            "zero-frame tracks must not be read"
        );

        let one = track_sources(
            &meta(Some(track(48_000)), Some(track(0))),
            dir,
            Tracks::Both,
        );
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].track, Track::Mic);
        assert!((one[0].secs - 1.0).abs() < 1e-6);
    }

    /// The whole argument for two tracks. A recording with both must produce two
    /// sources, and `--tracks` must be able to narrow it.
    #[test]
    fn both_tracks_are_read_by_default() {
        let dir = Path::new("/recordings/x");
        let m = meta(Some(track(48_000)), Some(track(96_000)));

        let both = track_sources(&m, dir, Tracks::Both);
        assert_eq!(both.len(), 2);
        assert_eq!(both[0].track, Track::Mic);
        assert_eq!(both[1].track, Track::System);
        assert!((both.iter().map(|s| s.secs).sum::<f32>() - 3.0).abs() < 1e-6);

        assert_eq!(track_sources(&m, dir, Tracks::Mic).len(), 1);
        assert_eq!(
            track_sources(&m, dir, Tracks::System)[0].track,
            Track::System
        );
    }

    /// The echo pass' verdict has to count, and it is not this stage's to
    /// second-guess: `preferred_mic_path` hands back the cancelled track only
    /// when that pass's own numbers cleared the bar.
    #[test]
    fn the_mic_source_honours_the_echo_passs_verdict() {
        let dir = Path::new("/recordings/x");
        let good = AecInfo {
            path: Some("mic_aec.wav".into()),
            version: 2,
            erle_db: Some(12.4),
            near_gain_db: Some(-0.2),
            ..AecInfo::default()
        };

        let mut m = meta(Some(track(48_000)), None);
        m.aec = Some(good.clone());
        assert_eq!(
            track_sources(&m, dir, Tracks::Both)[0].path,
            dir.join("mic_aec.wav")
        );

        // Cancelled plenty, but ate the speaker's voice doing it.
        m.aec = Some(AecInfo {
            near_gain_db: Some(-3.5),
            ..good
        });
        assert_eq!(
            track_sources(&m, dir, Tracks::Both)[0].path,
            dir.join("mic.wav"),
            "a damaging pass must not be fed to transcription"
        );
    }

    /// Enough to be useful, not so many that a laptop stops responding while it
    /// runs. The clamp is the point.
    #[test]
    fn thread_count_stays_within_its_bounds() {
        let n = threads();
        assert!((1..=4).contains(&n), "{n} threads");
    }
}
