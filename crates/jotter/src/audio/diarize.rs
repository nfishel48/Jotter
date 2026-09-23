//! The diarization pass: which of several people said each system segment.
//!
//! The third offline stage, after echo cancellation and transcription, and built
//! on the same mechanics: it reads a recording directory, records in `meta.json`
//! what it did or why it declined, and leaves the shared half of that to
//! [`crate::audio::stage`].
//!
//! # Only the system track
//!
//! `mic.wav` is you. The operating system separated the two signals when they
//! were captured, so "was this me?" was answered before any model ran — see the
//! two-track argument in `docs/ARCHITECTURE.md`. What is left is telling apart
//! the several people inside `system.wav`, and that is all this stage attempts.
//! Mic segments are never labelled, whatever the clustering thinks it heard:
//! running a speaker model over your own track can only split you in two.
//!
//! That halves the work and removes the hardest case. Diarizing a mixed file
//! means recovering a fact the recorder already had.
//!
//! # It labels, it does not re-cut
//!
//! The transcription pass has already cut the track at the pauses and written
//! timed segments. This pass only has to *attribute* them, so it never slices
//! audio, never re-runs the recogniser, and never merges adjacent turns — the
//! segment boundaries in `transcript.json` are the ones a reader already has.
//!
//! Each system segment takes the label of the speaker turn it overlaps most. A
//! segment overlapping no turn at all keeps no label: the field is omitted while
//! unset precisely so that "nobody attributed this" stays distinguishable from a
//! guess.
//!
//! # Two costs worth stating
//!
//! - **The whole waveform is resident.** `OfflineSpeakerDiarization::process`
//!   takes one slice and offers no streaming entry point, so an hour of system
//!   audio is ~230 MB of `f32` at 16 kHz. That is the opposite of the chunked
//!   discipline [`crate::audio::transcribe`] documents, and it is not a choice
//!   this module gets to make. It is affordable — less than the 346 MB the
//!   `i16` track already costs — and the `i16` track is dropped before the model
//!   runs so the two peaks do not add.
//! - **The model run reports no progress.** sherpa-onnx's C API has a callback
//!   form; the Rust wrapper at 1.13 exposes only the plain one. Progress is
//!   therefore reported across the decode and resample and then stops, which is
//!   honest about what is measurable rather than inventing a bar. The echo pass
//!   is the precedent for a stage that cannot narrate itself.

use std::path::{Path, PathBuf};
use std::time::Instant;

use sherpa_onnx::{
    FastClusteringConfig, LinearResampler, OfflineSpeakerDiarization,
    OfflineSpeakerDiarizationConfig, OfflineSpeakerSegmentationModelConfig,
    OfflineSpeakerSegmentationPyannoteModelConfig, SpeakerEmbeddingExtractorConfig,
};

use crate::audio::meta::{DiarizationInfo, Meta};
use crate::audio::stage::{DeclineReason, Stage, StageRecord, wav};
use crate::audio::transcribe;
use crate::audio::transcript::{Segment, Track, Transcript};
use crate::models::{self, ResolvedModel, Role};

/// Bumped whenever a change would attribute the same recording differently, so
/// labels left by an older build are detectable rather than trusted. The model
/// pair is recorded separately in `meta.diarization`, for the reason
/// [`transcribe::TRANSCRIBE_VERSION`] gives: which model ran is a user's choice,
/// not a property of the build.
pub const DIARIZE_VERSION: u32 = 1;

/// The file this stage fills in. Not one it creates — see the module doc.
pub const OUTPUT_NAME: &str = transcribe::OUTPUT_NAME;

/// Samples per resampler call, matching [`transcribe`]'s for the same reason:
/// converting an hour of 48 kHz mono to `f32` in one go would cost 690 MB, and
/// only the 16 kHz result needs to survive the loop.
const CHUNK: usize = 48_000;

/// The share of the progress bar the decode and resample get.
///
/// Everything after it is one opaque call into ONNX. Deliberately small and
/// deliberately not 1.0: a bar that fills and then waits claims the work is
/// done, and a bar that stops a fifth of the way across at least stops where
/// the measurable part ended.
const RESAMPLE_SHARE: f32 = 0.2;

/// Why a pass declined to diarize.
#[derive(Debug, Clone, PartialEq)]
pub enum DiarizeDecline {
    /// One or both models are not on disk. The likeliest reason on a first run,
    /// and the one with a one-line fix.
    ModelsMissing { files: usize },
    /// No transcript to label. This stage attributes segments someone else cut;
    /// it does not make them.
    NoTranscript,
    /// Nobody said how many people were on the call. See the note on
    /// [`DiarizeOptions::speakers`] for why that is fatal rather than a default.
    NoSpeakerCount,
    /// A transcript, but nothing on the system track — the ordinary shape of a
    /// recording with nobody else in it, not a failure.
    NoSystemSpeech,
    /// The transcript has system segments but `system.wav` is gone or empty, so
    /// there is no audio left to cluster.
    NoSystemAudio,
    /// The transcript already carries current labels. Re-run with `--force`.
    AlreadyDiarized,
}

impl DeclineReason for DiarizeDecline {
    fn kind(&self) -> &'static str {
        match self {
            Self::ModelsMissing { .. } => "models_missing",
            Self::NoTranscript => "no_transcript",
            Self::NoSpeakerCount => "no_speaker_count",
            Self::NoSystemSpeech => "no_system_speech",
            Self::NoSystemAudio => "no_system_audio",
            Self::AlreadyDiarized => "already_diarized",
        }
    }
}

impl std::fmt::Display for DiarizeDecline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ModelsMissing { files } => write!(
                f,
                "the speaker models are not downloaded ({files} file(s) missing) — \
                 run `jotter models pull`"
            ),
            Self::NoTranscript => write!(
                f,
                "there is no transcript to label — run `jotter transcribe <dir>` first"
            ),
            Self::NoSpeakerCount => write!(
                f,
                "nobody said how many people were on the call — pass `--speakers N`, \
                 or set `diarize_speakers` in settings.json"
            ),
            Self::NoSystemSpeech => write!(
                f,
                "nobody else was recorded, so there are no speakers to tell apart"
            ),
            Self::NoSystemAudio => write!(f, "system.wav is missing or empty"),
            Self::AlreadyDiarized => {
                write!(f, "already diarized — pass --force to redo it")
            }
        }
    }
}

/// The diarization stage.
///
/// A unit struct for the same reason [`transcribe::Transcribe`] is: the pass
/// keeps nothing between runs, and this exists to hang the [`Stage`]
/// implementation on.
pub struct Diarize;

impl Stage for Diarize {
    type Decline = DiarizeDecline;

    fn name(&self) -> &'static str {
        "diarize"
    }

    fn version(&self) -> u32 {
        DIARIZE_VERSION
    }

    fn record<'m>(&self, meta: &'m Meta) -> Option<StageRecord<'m>> {
        meta.diarization.as_ref().map(|info| StageRecord {
            version: info.version,
            output: info.path.as_deref(),
            declined: info.declined.as_deref(),
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct DiarizeOptions {
    /// Report what would happen and write nothing.
    pub dry_run: bool,
    /// Re-label a transcript that already carries current labels.
    pub force: bool,
    /// How many people to expect on the system track.
    ///
    /// Required. `None` is a decline, not a default, and that is the one design
    /// decision here worth arguing for.
    ///
    /// sherpa-onnx will happily infer the count — pass `-1` and a distance
    /// threshold decides. Measured, that inference is not safe to ship. On
    /// clean audio it is exactly right; on a recording where people talk over
    /// each other it degrades without warning, and on a 36-minute meeting of
    /// three it returned **208 speakers**. There is no threshold that fixes
    /// that: sweeping it moves the answer from "fragmented" to "everyone is one
    /// person" without passing through the truth.
    ///
    /// A transcript labelled with 208 names is worse than an unlabelled one. It
    /// is confidently wrong, it is wrong in a way a reader cannot detect from
    /// the file, and it poisons whatever resolves those labels to real names
    /// later. A stated count cannot fail that way: the clustering is *told* how
    /// many groups to form, so the worst case is that it puts the right number
    /// of people in the wrong groups — recoverable, and visible.
    ///
    /// So the count comes from the person who was in the meeting, and a pass
    /// without one declines and says so.
    ///
    /// No `model` field, unlike [`transcribe::TranscribeOptions`]: diarization
    /// takes a *pair* of models, and an override that could name only one of
    /// them would let someone pair a segmenter with embeddings it was never
    /// matched against. If a second pair is ever added, this grows one option
    /// naming the pair, not two naming halves.
    pub speakers: Option<u8>,
}

/// What the pass did. `decline` set means nothing was written.
#[derive(Debug, Clone)]
pub struct DiarizeReport {
    /// Catalogue data, so `&'static str` — see the note on
    /// [`transcribe::TranscriptReport`].
    pub segmentation_model_id: &'static str,
    pub embedding_model_id: &'static str,
    pub engine: &'static str,
    /// Distinct speakers found.
    pub speakers: u32,
    /// System segments in the transcript, and how many got a label. The gap is
    /// the measure worth having: segments the clustering could not place are
    /// left unlabelled rather than guessed at, so this is how often that
    /// happened.
    pub system_segments: u32,
    pub attributed_segments: u32,
    pub audio_secs: f32,
    pub elapsed_secs: f32,
    pub decline: Option<DiarizeDecline>,
    pub output: Option<PathBuf>,
}

#[derive(Debug)]
pub enum DiarizeError {
    Io(std::io::Error),
    Wav(hound::Error),
    /// sherpa-onnx refused to build the diarizer or a resampler, or the model
    /// run itself returned nothing. It reports failure as a null pointer and
    /// logs the reason to stderr, so there is nothing more specific to pass on
    /// than which step failed.
    Engine(&'static str),
}

impl std::fmt::Display for DiarizeError {
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

impl std::error::Error for DiarizeError {}

impl From<std::io::Error> for DiarizeError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<hound::Error> for DiarizeError {
    fn from(e: hound::Error) -> Self {
        Self::Wav(e)
    }
}

/// One stretch of one voice, as the clustering saw it.
///
/// `speaker` is the raw cluster index, which is *not* what ends up in the file:
/// see [`renumber`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Turn {
    pub start: f64,
    pub end: f64,
    pub speaker: i32,
}

impl Turn {
    /// The same turn, moved onto another timeline. The twin of
    /// [`Segment::shifted`], and load-bearing for the same reason.
    fn shifted(mut self, by_secs: f64) -> Self {
        self.start += by_secs;
        self.end += by_secs;
        self
    }

    /// Seconds this turn and `segment` have in common. Zero when they do not
    /// touch — never negative, which is what makes it safe to compare.
    fn overlap(&self, segment: &Segment) -> f64 {
        (self.end.min(segment.end) - self.start.max(segment.start)).max(0.0)
    }
}

/// Turns audio into speaker turns. The seam a second backend slots into, for
/// the same reason [`transcribe::Transcriber`] is one.
pub trait Diarizer {
    /// What the segmentation model was trained at. Asked rather than assumed:
    /// the resample has to target this, and a mismatch would not fail, it would
    /// cluster nonsense.
    fn sample_rate(&self) -> i32;

    /// `None` when the model run failed outright. An empty `Vec` is different
    /// and ordinary: audio with no speech in it has no turns.
    fn turns(&self, samples: &[f32]) -> Option<Vec<Turn>>;
}

/// Pyannote segmentation plus WeSpeaker embeddings, clustered.
pub struct PyannoteDiarizer {
    inner: OfflineSpeakerDiarization,
}

impl PyannoteDiarizer {
    pub fn create(
        segmentation: &ResolvedModel,
        embedding: &ResolvedModel,
        speakers: u8,
    ) -> Result<Self, DiarizeError> {
        let path = |model: &ResolvedModel, role: Role| {
            model.path(role).map(|p| p.to_string_lossy().into_owned())
        };

        let config = OfflineSpeakerDiarizationConfig {
            segmentation: OfflineSpeakerSegmentationModelConfig {
                pyannote: OfflineSpeakerSegmentationPyannoteModelConfig {
                    model: path(segmentation, Role::Segmentation),
                    ..Default::default()
                },
                num_threads: transcribe::threads(),
                provider: Some("cpu".into()),
                debug: false,
            },
            embedding: SpeakerEmbeddingExtractorConfig {
                model: path(embedding, Role::SpeakerEmbedding),
                num_threads: transcribe::threads(),
                provider: Some("cpu".into()),
                debug: false,
            },
            clustering: FastClusteringConfig {
                // Always a real count, never sherpa-onnx's `-1`. With a count
                // set, `threshold` is not consulted at all — which is the
                // point: see [`DiarizeOptions::speakers`] for the measurements
                // that ruled the inferred form out.
                num_clusters: i32::from(speakers),
                ..Default::default()
            },
            // sherpa-onnx's own defaults, kept. 0.3 s of speech to open a turn
            // and 0.5 s of silence to close one is the same turn-boundary
            // judgement `transcribe::MIN_SILENCE_SECS` makes, which is what
            // keeps the turns and the segments roughly commensurate.
            ..Default::default()
        };

        let inner =
            OfflineSpeakerDiarization::create(&config).ok_or(DiarizeError::Engine("diarizer"))?;
        Ok(Self { inner })
    }
}

impl Diarizer for PyannoteDiarizer {
    fn sample_rate(&self) -> i32 {
        self.inner.sample_rate()
    }

    fn turns(&self, samples: &[f32]) -> Option<Vec<Turn>> {
        let result = self.inner.process(samples)?;
        Some(
            result
                .sort_by_start_time()
                .into_iter()
                .map(|s| Turn {
                    start: s.start as f64,
                    end: s.end as f64,
                    speaker: s.speaker,
                })
                .collect(),
        )
    }
}

/// How far through the measurable part the pass is. See [`RESAMPLE_SHARE`].
pub type ProgressFn<'a> = &'a mut dyn FnMut(f32);

/// Runs the pass over a recording directory.
///
/// Takes a path and opens its own files, holding no cpal types, so it is `Send`
/// and can run on any thread — or in another process from the one that
/// recorded.
pub fn run(dir: &Path, options: DiarizeOptions) -> Result<DiarizeReport, DiarizeError> {
    run_with_progress(dir, options, &mut |_| {})
}

pub fn run_with_progress(
    dir: &Path,
    options: DiarizeOptions,
    progress: ProgressFn<'_>,
) -> Result<DiarizeReport, DiarizeError> {
    let started = Instant::now();
    let meta_path = dir.join("meta.json");
    let meta = Meta::read(&meta_path)?;

    let mut report = DiarizeReport {
        segmentation_model_id: models::DEFAULT_SEGMENTATION_MODEL.id,
        embedding_model_id: models::DEFAULT_EMBEDDING_MODEL.id,
        engine: models::DEFAULT_SEGMENTATION_MODEL.engine,
        speakers: 0,
        system_segments: 0,
        attributed_segments: 0,
        audio_secs: 0.0,
        elapsed_secs: 0.0,
        decline: None,
        output: None,
    };

    // Read out of `meta.json` before anything can decline, and copied out
    // rather than borrowed because `meta` is moved into `finish`. The copy is
    // the point: `meta.json` already says how long the system track is, so
    // every early return can report the real figure instead of claiming the
    // recording was empty. Same reasoning as `transcribe::run` working out its
    // sources up front.
    let system = meta.system.as_ref().filter(|t| t.frames > 0);
    report.audio_secs = system.map_or(0.0, |t| t.frames as f32 / t.sample_rate.max(1) as f32);
    let system_path = system.map(|t| t.resolve(dir));

    // Every check answerable without reading audio runs first, so a recording
    // this cannot handle costs no I/O — the ordering `transcribe::run` and
    // `process::check_alignable` both use.
    let Some(transcript_path) = meta
        .transcript
        .as_ref()
        .and_then(|t| t.path.as_deref())
        .map(|p| dir.join(p))
    else {
        return finish(
            dir,
            &meta_path,
            meta,
            report,
            Some(DiarizeDecline::NoTranscript),
            options,
            started,
        );
    };

    // Before the re-run check and before any audio: a pass with no count cannot
    // proceed however fresh the recording is, and saying so costs nothing.
    let Some(speakers) = options.speakers.filter(|&n| n > 0) else {
        return finish(
            dir,
            &meta_path,
            meta,
            report,
            Some(DiarizeDecline::NoSpeakerCount),
            options,
            started,
        );
    };

    if !options.force && !options.dry_run && Diarize.is_current(&meta) {
        return finish(
            dir,
            &meta_path,
            meta,
            report,
            Some(DiarizeDecline::AlreadyDiarized),
            options,
            started,
        );
    }

    let mut transcript = Transcript::read(&transcript_path)?;
    report.system_segments = transcript.segments_from(Track::System);
    if report.system_segments == 0 {
        return finish(
            dir,
            &meta_path,
            meta,
            report,
            Some(DiarizeDecline::NoSystemSpeech),
            options,
            started,
        );
    }

    // The transcript says there was system speech, so the track it was
    // transcribed from should still be here. Reached when it is not — deleted,
    // or a `meta.json` copied away from its audio.
    let Some(system_path) = system_path else {
        return finish(
            dir,
            &meta_path,
            meta,
            report,
            Some(DiarizeDecline::NoSystemAudio),
            options,
            started,
        );
    };

    let resolved = match resolve() {
        Ok(resolved) => resolved,
        Err(decline) => {
            return finish(
                dir,
                &meta_path,
                meta,
                report,
                Some(decline),
                options,
                started,
            );
        }
    };

    // A dry run stops here, having established everything answerable without
    // running the models: that there is a transcript, that it has system speech,
    // how long the track is, and whether the models are ready.
    if options.dry_run {
        return finish(dir, &meta_path, meta, report, None, options, started);
    }

    let diarizer = PyannoteDiarizer::create(&resolved.segmentation, &resolved.embedding, speakers)?;

    let samples = read_resampled(&system_path, diarizer.sample_rate(), progress)?;
    let Some(turns) = diarizer.turns(&samples) else {
        return Err(DiarizeError::Engine("speaker clustering"));
    };
    // Held only until here, and dropped explicitly rather than at end of scope:
    // the transcript rewrite below has no use for 230 MB of audio.
    drop(samples);

    // The clustering read `system.wav`, so its times are on that file's own
    // timeline. The transcript's are on the mic's. Skip this and every label
    // lands on the neighbouring segment — see `Segment::shifted`.
    let offset = meta.track_offset_secs().unwrap_or(0.0);
    let mut turns: Vec<Turn> = turns.into_iter().map(|t| t.shifted(offset)).collect();
    renumber(&mut turns);

    report.speakers = distinct_speakers(&turns);
    report.attributed_segments = label(&mut transcript.segments, &turns);

    transcript.write(&transcript_path)?;
    report.output = Some(transcript_path);

    finish(dir, &meta_path, meta, report, None, options, started)
}

/// Both models a run needs, resolved together.
struct Resolved {
    segmentation: ResolvedModel,
    embedding: ResolvedModel,
}

/// Check both models are on disk before any audio is read.
///
/// Together, and with the counts summed, for the reason `transcribe::resolve`
/// gives: they are equally fatal, a user missing both should be told once, and
/// "5 files missing" is one `jotter models pull` away whereas five separate
/// complaints are not.
fn resolve() -> Result<Resolved, DiarizeDecline> {
    let segmentation = models::DEFAULT_SEGMENTATION_MODEL.resolve();
    let embedding = models::DEFAULT_EMBEDDING_MODEL.resolve();

    match (segmentation, embedding) {
        (Ok(segmentation), Ok(embedding)) => Ok(Resolved {
            segmentation,
            embedding,
        }),
        (segmentation, embedding) => {
            let files = segmentation.as_ref().err().map_or(0, |e| e.problems.len())
                + embedding.as_ref().err().map_or(0, |e| e.problems.len());
            Err(DiarizeDecline::ModelsMissing { files })
        }
    }
}

/// Read a track and hand back the whole thing at `target_rate`.
///
/// Whole, because [`OfflineSpeakerDiarization::process`] takes one slice — see
/// the module doc. The conversion and resample are still chunked, so the peak is
/// the `i16` track plus the 16 kHz result rather than also the 48 kHz `f32` in
/// between, and the `i16` track is dropped before returning so it is not still
/// resident when the model runs.
fn read_resampled(
    path: &Path,
    target_rate: i32,
    progress: ProgressFn<'_>,
) -> Result<Vec<f32>, DiarizeError> {
    let audio = wav::read_track(path)?;
    let source_rate = audio.sample_rate.max(1);

    // A track already at the model's rate is converted and handed straight
    // over. Not only to save the work: a resampler asked for a ratio of 1 is
    // still a filter, and running the audio through one for no reason is a
    // change to the samples the embeddings are drawn from.
    if source_rate as i32 == target_rate {
        let out: Vec<f32> = audio.samples.iter().map(|&s| s as f32 / 32_768.0).collect();
        progress(RESAMPLE_SHARE);
        return Ok(out);
    }

    let resampler = LinearResampler::create(source_rate as i32, target_rate)
        .ok_or(DiarizeError::Engine("resampler"))?;

    let total = audio.samples.len().max(1);
    let mut out: Vec<f32> = Vec::with_capacity(
        (audio.samples.len() as f64 * target_rate as f64 / source_rate as f64) as usize + 1,
    );

    for (index, chunk) in audio.samples.chunks(CHUNK).enumerate() {
        let float: Vec<f32> = chunk.iter().map(|&s| s as f32 / 32_768.0).collect();
        out.extend(resampler.resample(&float, false));
        progress((index * CHUNK) as f32 / total as f32 * RESAMPLE_SHARE);
    }
    // The resampler holds a tail. Dropping it loses the end of the recording,
    // which in a meeting is where the actions are.
    out.extend(resampler.resample(&[], true));
    progress(RESAMPLE_SHARE);

    Ok(out)
}

/// How many distinct voices the turns name.
///
/// Counted from the turns rather than taken from the result object's own
/// `num_speakers`, so the figure always describes the turns actually used. It
/// can therefore come out *below* the requested count, and that is worth
/// seeing: asking for five speakers and getting four back means one of the five
/// was never heard, which is a fact about the meeting rather than an error.
fn distinct_speakers(turns: &[Turn]) -> u32 {
    let mut seen: Vec<i32> = Vec::new();
    for turn in turns {
        if !seen.contains(&turn.speaker) {
            seen.push(turn.speaker);
        }
    }
    seen.len() as u32
}

/// Give every system segment the label of the speaker it overlaps most.
///
/// Returns how many were labelled. Segments overlapping no turn keep `None`:
/// the field is omitted while unset precisely so an unattributed segment can say
/// so, and a nearest-neighbour guess would spend that distinction on a label
/// nobody can check.
///
/// Mic segments are skipped unconditionally. The clustering only ever saw
/// `system.wav`, so a turn that appears to cover a mic segment covers it by
/// coincidence of timing — that is you talking over someone, and labelling it
/// with their name would be exactly wrong.
///
/// Separate from the stage, and `pub(crate)` for its tests, because this is
/// where the attribution actually happens: everything around it is file
/// handling, and this is the part with a right answer.
pub(crate) fn label(segments: &mut [Segment], turns: &[Turn]) -> u32 {
    let mut attributed = 0;

    for segment in segments.iter_mut() {
        if segment.track != Track::System {
            continue;
        }

        // Totalled per speaker, not per turn. A segment usually sits inside one
        // turn, but it does not have to: the VAD cut it at a pause in the audio
        // and the segmenter cut turns at changes of voice, and neither consulted
        // the other. When a segment spans several turns, the speaker who held it
        // longest is the answer — which is not the same as the single longest
        // turn touching it, and picking that instead hands a segment to whoever
        // happened to have one uninterrupted stretch inside it.
        let mut totals: Vec<(i32, f64)> = Vec::new();
        for turn in turns {
            let overlap = turn.overlap(segment);
            if overlap <= 0.0 {
                continue;
            }
            match totals
                .iter_mut()
                .find(|(speaker, _)| *speaker == turn.speaker)
            {
                Some((_, total)) => *total += overlap,
                None => totals.push((turn.speaker, overlap)),
            }
        }

        // `total_cmp` rather than `partial_cmp().unwrap()`, the same reasoning
        // as `transcript::merge`: the times come from sample counts so they are
        // never NaN, and reaching for the panicking form here would be waiting
        // for the one recording that is. Ties go to the lower speaker index, so
        // re-running over one recording produces one file.
        let best = totals
            .into_iter()
            .max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0)));

        if let Some((speaker, _)) = best {
            segment.speaker = Some(speaker_label(speaker));
            attributed += 1;
        }
    }

    attributed
}

/// Renumber cluster indices by order of first appearance.
///
/// The clustering's indices are sparse: on a three-speaker recording it will
/// happily return clusters 0, 3 and 6, because the numbers are positions in an
/// internal table and nothing ever promised otherwise. Written through
/// unchanged, that is a transcript with a `speaker_07` in a meeting of three
/// people — which reads as a bug in the attribution rather than as the
/// meaningless label it is, and invites exactly the wrong conclusion about how
/// many people were there.
///
/// Renumbering by first appearance costs one pass and makes `speaker_01` the
/// first person to say anything, which is both dense and useful. Turns arrive
/// sorted by start time, so "first appearance" is just the order seen.
fn renumber(turns: &mut [Turn]) {
    let mut order: Vec<i32> = Vec::new();
    for turn in turns.iter_mut() {
        let position = match order.iter().position(|&s| s == turn.speaker) {
            Some(position) => position,
            None => {
                order.push(turn.speaker);
                order.len() - 1
            }
        };
        turn.speaker = position as i32;
    }
}

/// The name a cluster index goes into the transcript under.
///
/// `speaker_01` rather than `speaker_0`: these are read by people and, one step
/// later, by a model resolving them to real names, and both find one-based
/// labels less surprising. Zero-padded so a meeting with ten people sorts.
fn speaker_label(index: i32) -> String {
    format!("speaker_{:02}", index.saturating_add(1))
}

/// Record the outcome in `meta.json` — including, and especially, a decline.
///
/// The twin of `transcribe::finish`, and the same contract: a pass that produced
/// nothing has to leave a reason behind, or it is indistinguishable from one
/// that crashed.
fn finish(
    dir: &Path,
    meta_path: &Path,
    mut meta: Meta,
    mut report: DiarizeReport,
    decline: Option<DiarizeDecline>,
    options: DiarizeOptions,
    started: Instant,
) -> Result<DiarizeReport, DiarizeError> {
    report.decline = decline;
    report.elapsed_secs = started.elapsed().as_secs_f32();

    if options.dry_run {
        return Ok(report);
    }

    meta.diarization = Some(DiarizationInfo {
        // Relative to `dir`, the convention every path in `meta.json` follows.
        path: report
            .output
            .as_ref()
            .and_then(|p| p.strip_prefix(dir).ok())
            .map(|p| p.to_string_lossy().into_owned()),
        version: DIARIZE_VERSION,
        segmentation_model: report.segmentation_model_id.to_string(),
        embedding_model: report.embedding_model_id.to_string(),
        speakers: report.speakers,
        system_segments: report.system_segments,
        attributed_segments: report.attributed_segments,
        audio_secs: report.audio_secs,
        elapsed_secs: report.elapsed_secs,
        declined: Diarize.declined_kind(report.decline.as_ref()),
    });
    meta.write(meta_path)?;

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(start: f64, end: f64, track: Track) -> Segment {
        Segment {
            start,
            end,
            track,
            speaker: None,
            text: "words".into(),
        }
    }

    fn turn(start: f64, end: f64, speaker: i32) -> Turn {
        Turn {
            start,
            end,
            speaker,
        }
    }

    #[test]
    fn a_segment_takes_the_label_of_the_turn_it_overlaps_most() {
        // The segment straddles a speaker change, but spends 0.8 s with the
        // second voice against 0.2 s with the first.
        let mut segments = vec![segment(1.0, 2.0, Track::System)];
        let turns = [turn(0.0, 1.2, 0), turn(1.2, 3.0, 1)];

        assert_eq!(label(&mut segments, &turns), 1);
        assert_eq!(segments[0].speaker.as_deref(), Some("speaker_02"));
    }

    /// The bug this guards, found on a three-speaker fixture where every
    /// segment came back with the same label: a segment spanning several turns
    /// must go to the speaker who held it *longest in total*, not to whoever
    /// owns the single longest turn inside it.
    ///
    /// Here speaker 1 holds two stretches of 3 s each and speaker 0 holds one
    /// of 4 s. The longest single turn is speaker 0's; the speaker who actually
    /// dominates the segment is speaker 1.
    #[test]
    fn a_segment_goes_to_the_speaker_it_overlaps_most_in_total() {
        let mut segments = vec![segment(0.0, 10.0, Track::System)];
        let turns = [turn(0.0, 3.0, 1), turn(3.0, 7.0, 0), turn(7.0, 10.0, 1)];

        assert_eq!(label(&mut segments, &turns), 1);
        assert_eq!(
            segments[0].speaker.as_deref(),
            Some("speaker_02"),
            "6s across two turns must beat one 4s turn"
        );
    }

    /// Equal totals have to resolve the same way every run, or re-diarizing one
    /// recording produces two different files.
    #[test]
    fn an_exact_tie_goes_to_the_lower_speaker_index() {
        let turns = [turn(0.0, 5.0, 3), turn(5.0, 10.0, 1)];
        for _ in 0..8 {
            let mut segments = vec![segment(0.0, 10.0, Track::System)];
            label(&mut segments, &turns);
            assert_eq!(segments[0].speaker.as_deref(), Some("speaker_02"));
        }
    }

    /// The distinction the format's `skip_serializing_if` exists to preserve.
    /// A nearest-turn guess would be unfalsifiable; absence is honest.
    #[test]
    fn a_segment_overlapping_no_turn_keeps_no_speaker() {
        let mut segments = vec![segment(10.0, 11.0, Track::System)];
        let turns = [turn(0.0, 2.0, 0)];

        assert_eq!(label(&mut segments, &turns), 0);
        assert_eq!(segments[0].speaker, None);

        // And it really is absent from the file, not null.
        let transcript = Transcript::new("m", segments);
        let json = serde_json::to_string(&transcript).expect("serialize");
        assert!(
            !json.contains("speaker"),
            "unexpected speaker key in {json}"
        );
    }

    /// The clustering only ever saw `system.wav`. A turn covering a mic segment
    /// covers it because you were talking at the same time as someone else, so
    /// taking the label would attribute your words to them.
    #[test]
    fn mic_segments_are_never_labelled() {
        let mut segments = vec![
            segment(1.0, 2.0, Track::Mic),
            segment(1.0, 2.0, Track::System),
        ];
        let turns = [turn(0.0, 5.0, 3)];

        assert_eq!(label(&mut segments, &turns), 1);
        assert_eq!(segments[0].speaker, None, "the mic track is you");
        assert_eq!(segments[1].speaker.as_deref(), Some("speaker_04"));
    }

    /// The bug `Turn::shifted` exists for, asserted from both sides: the two
    /// cpal streams start at different instants, so a time read out of
    /// `system.wav` is not comparable with one in the transcript. Without the
    /// shift the label lands on the neighbouring segment.
    #[test]
    fn turns_are_moved_onto_the_transcript_timeline_before_labelling() {
        // The system stream started 500 ms after the mic stream, so a turn the
        // clustering saw at t=0 really happened at t=0.5. Two people speak back
        // to back: speaker 0 holds [0.5, 1.5] in transcript time, speaker 1
        // holds [1.5, 2.5].
        let offset = 0.5;
        let raw = [turn(0.0, 1.0, 0), turn(1.0, 2.0, 1)];

        // A segment sitting just inside the first speaker's real turn — and,
        // unshifted, just inside the *second* speaker's. Which is the point:
        // the offset is the only thing that decides this one.
        let mut segments = vec![segment(1.2, 1.4, Track::System)];

        let shifted: Vec<Turn> = raw.iter().map(|t| t.shifted(offset)).collect();
        assert_eq!(label(&mut segments, &shifted), 1);
        assert_eq!(segments[0].speaker.as_deref(), Some("speaker_01"));

        // Without the shift the same segment is attributed to the other person
        // — this is the failure, asserted so a regression cannot pass quietly.
        let mut unshifted = vec![segment(1.2, 1.4, Track::System)];
        label(&mut unshifted, &raw);
        assert_eq!(unshifted[0].speaker.as_deref(), Some("speaker_02"));
    }

    #[test]
    fn speakers_are_counted_from_the_turns_that_exist() {
        let turns = [
            turn(0.0, 1.0, 0),
            turn(1.0, 2.0, 1),
            turn(2.0, 3.0, 0),
            turn(3.0, 4.0, 2),
        ];
        assert_eq!(distinct_speakers(&turns), 3);
        assert_eq!(distinct_speakers(&[]), 0);
    }

    /// Observed on a three-speaker control: the clustering returned indices 0,
    /// 3 and 6, which without this becomes a `speaker_07` in a meeting of
    /// three. Dense, first-appearance numbering is what makes the label count
    /// match the speaker count.
    #[test]
    fn sparse_cluster_indices_are_renumbered_by_first_appearance() {
        let mut turns = vec![
            turn(0.0, 1.0, 3),
            turn(1.0, 2.0, 6),
            turn(2.0, 3.0, 3),
            turn(3.0, 4.0, 0),
        ];
        renumber(&mut turns);

        let ids: Vec<i32> = turns.iter().map(|t| t.speaker).collect();
        assert_eq!(ids, [0, 1, 0, 2], "first heard must become speaker 0");
        assert_eq!(distinct_speakers(&turns), 3);

        // And the labels that reach the file are dense and one-based.
        let mut segments = vec![
            segment(0.0, 1.0, Track::System),
            segment(1.0, 2.0, Track::System),
            segment(3.0, 4.0, Track::System),
        ];
        label(&mut segments, &turns);
        let labels: Vec<_> = segments.iter().map(|s| s.speaker.as_deref()).collect();
        assert_eq!(
            labels,
            [Some("speaker_01"), Some("speaker_02"), Some("speaker_03")]
        );
    }

    /// One-based and zero-padded, so a ten-person meeting sorts and nobody has
    /// to explain why the first speaker is numbered zero.
    #[test]
    fn labels_are_one_based_and_padded() {
        assert_eq!(speaker_label(0), "speaker_01");
        assert_eq!(speaker_label(9), "speaker_10");
    }

    /// The kind is a contract that aggregate telemetry groups by; the sentence
    /// is written for a human and may be reworded freely. This pins the first.
    #[test]
    fn declines_are_recorded_by_kind_not_by_their_message() {
        let kinds = [
            (DiarizeDecline::ModelsMissing { files: 2 }, "models_missing"),
            (DiarizeDecline::NoTranscript, "no_transcript"),
            (DiarizeDecline::NoSpeakerCount, "no_speaker_count"),
            (DiarizeDecline::NoSystemSpeech, "no_system_speech"),
            (DiarizeDecline::NoSystemAudio, "no_system_audio"),
            (DiarizeDecline::AlreadyDiarized, "already_diarized"),
        ];
        for (decline, kind) in kinds {
            assert_eq!(decline.kind(), kind);
            assert!(!decline.to_string().is_empty());
        }
    }

    /// A decline wrote no labels, so there is nothing to keep and the reason it
    /// declined may since have been fixed. Same rule as every other stage.
    #[test]
    fn a_declined_pass_is_never_current() {
        let mut meta = Meta {
            started_at: 0.0,
            ended_at: 10.0,
            mic: None,
            system: None,
            aec: None,
            transcript: None,
            diarization: Some(DiarizationInfo {
                path: None,
                version: DIARIZE_VERSION,
                declined: Some("models_missing".into()),
                ..Default::default()
            }),
            live: None,
        };
        assert!(!Diarize.is_current(&meta));

        // And an output from an older version is redone rather than trusted.
        meta.diarization = Some(DiarizationInfo {
            path: Some(OUTPUT_NAME.into()),
            version: DIARIZE_VERSION - 1,
            ..Default::default()
        });
        assert!(!Diarize.is_current(&meta));

        meta.diarization = Some(DiarizationInfo {
            path: Some(OUTPUT_NAME.into()),
            version: DIARIZE_VERSION,
            ..Default::default()
        });
        assert!(Diarize.is_current(&meta));
    }
}
