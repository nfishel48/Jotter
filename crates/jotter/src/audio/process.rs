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
//! functions over frame energies. The mechanics underneath them are not
//! AEC-specific and live in [`crate::audio::stage`]: the crash-safe write, the
//! WAV plumbing, and the already-processed check that every later pass needs in
//! exactly the same shape.

use std::path::{Path, PathBuf};

use crate::audio::aec;
use crate::audio::aec::delay;
use crate::audio::meta::{AecInfo, Meta};
use crate::audio::stage::{DeclineReason, Stage, StageRecord, wav};

/// Bumped whenever a change would give a different result for the same input,
/// so a `mic_aec.wav` left over from an older build is detectable rather than
/// being mistaken for a current one.
///
/// 2: WebRTC AEC3, replacing a hand-rolled canceller over Speex MDF. Not a
/// tuning change — on the reference recording it moved echo removal from 1.8 dB
/// to 20 dB, so every `mic_aec.wav` from version 1 is worth redoing.
pub const AEC_VERSION: u32 = 2;

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

impl DeclineReason for AecBypass {
    fn kind(&self) -> &'static str {
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

/// The echo-cancellation stage.
///
/// A unit struct: the pass keeps nothing between runs, and this exists only to
/// hang the [`Stage`] implementation on, which is what tells the shared
/// mechanics where in `meta.json` to look for AEC's own record.
pub struct Aec;

impl Stage for Aec {
    type Decline = AecBypass;

    fn name(&self) -> &'static str {
        "aec"
    }

    fn version(&self) -> u32 {
        AEC_VERSION
    }

    fn record<'m>(&self, meta: &'m Meta) -> Option<StageRecord<'m>> {
        meta.aec.as_ref().map(|info| StageRecord {
            version: info.version,
            output: info.path.as_deref(),
            declined: info.bypassed.as_deref(),
        })
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

/// How long the far end keeps counting as active after it falls quiet.
///
/// Its echo is still arriving. Without this, a frame during a pause between two
/// remote words is labelled near-only while the echo tail is still decaying —
/// and the canceller correctly removing that tail then reads as damage to the
/// user's voice. Measured on the reference recording: 200 ms of hangover moved
/// the reported near-end loss from -1.0 dB to -0.3 dB, and the -1.0 dB was
/// entirely this artifact.
///
/// 200 ms comfortably exceeds the ~100 ms reverb tail of a laptop-speaker path,
/// and it is short enough not to swallow genuine near-only speech, which in a
/// meeting arrives in bursts far longer than that.
pub const FAR_HANGOVER_MS: usize = 200;

/// Labels each frame from the two tracks' per-frame RMS.
///
/// Thresholds come from [`aec::active_threshold`], derived per track, so the
/// same rule governs the delay estimator and the metrics.
///
/// `frame_ms` is the length of one RMS frame, needed to turn
/// [`FAR_HANGOVER_MS`] into a frame count.
pub fn classify(mic_rms: &[f32], far_rms: &[f32], frame_ms: usize) -> Vec<Activity> {
    let near_threshold = aec::active_threshold(mic_rms);
    let far_threshold = aec::active_threshold(far_rms);
    let hangover = FAR_HANGOVER_MS / frame_ms.max(1);

    let frames = mic_rms.len().min(far_rms.len());
    let mut since_far = usize::MAX;
    let mut out = Vec::with_capacity(frames);

    for i in 0..frames {
        if far_rms[i] > far_threshold {
            since_far = 0;
        } else {
            since_far = since_far.saturating_add(1);
        }
        let far_on = since_far <= hangover;

        out.push(match (mic_rms[i] > near_threshold, far_on) {
            (false, false) => Activity::Silence,
            (true, false) => Activity::NearOnly,
            (false, true) => Activity::FarOnly,
            (true, true) => Activity::DoubleTalk,
        });
    }
    out
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

// ---------------------------------------------------------------------------
// The file-level pass.
// ---------------------------------------------------------------------------

/// RMS frame length for the activity pass, in milliseconds. 100 ms matches what
/// the reference activity census was measured at.
const ENVELOPE_FRAME_MS: usize = 100;

/// Longest run of far-only audio a segment needs before a delay is measured
/// from it.
const MIN_SEGMENT_MS: usize = 400;

/// Audio read per delay-measurement segment.
const SEGMENT_MS: usize = 500;

/// Stretches of the recording to sample for delay measurement. Spread across
/// the file rather than clustered, so the drift fit has a lever arm.
const DELAY_STRATA: usize = 12;

#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessOptions {
    /// Report what would happen and write nothing.
    pub dry_run: bool,
    /// Overwrite an existing `mic_aec.wav`.
    pub force: bool,
    /// Skip measurement and use this delay. For debugging; `None` measures.
    pub delay_ms: Option<f32>,
}

/// What the pass did. `bypass` set means nothing was written.
#[derive(Debug, Clone)]
pub struct AecReport {
    pub delay: delay::DelayEstimate,
    pub census: Census,
    pub stats: aec::AecStats,
    pub config: aec::AecConfig,
    pub far_gap_secs: f32,
    pub bypass: Option<AecBypass>,
    pub output: Option<PathBuf>,
}

#[derive(Debug)]
pub enum ProcessError {
    Io(std::io::Error),
    Wav(hound::Error),
    Aec(aec::AecError),
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Wav(e) => write!(f, "{e}"),
            Self::Aec(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ProcessError {}

impl From<std::io::Error> for ProcessError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<hound::Error> for ProcessError {
    fn from(e: hound::Error) -> Self {
        Self::Wav(e)
    }
}

impl From<aec::AecError> for ProcessError {
    fn from(e: aec::AecError) -> Self {
        Self::Aec(e)
    }
}

/// Runs the pass over a recording directory.
///
/// Takes a path and opens its own files, holding no cpal types, so it is
/// `Send` and can run on any thread — or in another process from the one that
/// recorded. `RecordingHandle::stop` never waits on it.
pub fn run(dir: &Path, options: ProcessOptions) -> Result<AecReport, ProcessError> {
    let meta_path = dir.join("meta.json");
    let meta = Meta::read(&meta_path)?;

    let output_path = dir.join(OUTPUT_NAME);
    let mut report = AecReport {
        delay: delay::DelayEstimate::unaligned(None),
        census: Census::default(),
        stats: aec::AecStats::default(),
        config: aec::AecConfig::default(),
        far_gap_secs: 0.0,
        bypass: None,
        output: None,
    };

    if let (Some(mic), Some(system)) = (meta.mic.as_ref(), meta.system.as_ref()) {
        report.far_gap_secs = far_gap_secs(mic.frames, system.frames, mic.sample_rate);
    }

    if let Err(bypass) = check_alignable(&meta) {
        return finish(dir, &meta_path, meta, report, Some(bypass), options);
    }
    if output_path.exists() && !options.force && !options.dry_run && Aec.is_current(&meta) {
        return finish(
            dir,
            &meta_path,
            meta,
            report,
            Some(AecBypass::AlreadyProcessed),
            options,
        );
    }

    let tracks = meta
        .mic
        .as_ref()
        .zip(meta.system.as_ref())
        .map(|(mic, system)| (mic.resolve(dir), system.resolve(dir)));
    // `check_alignable` above already turned a missing track into a bypass, so
    // this arm is unreachable; it exists so a future reordering cannot make it
    // a panic.
    let Some((mic_path, far_path)) = tracks else {
        return finish(
            dir,
            &meta_path,
            meta,
            report,
            Some(AecBypass::NoNearEnd),
            options,
        );
    };

    let mic = wav::read_track(&mic_path)?;
    let far = wav::read_track(&far_path)?;
    let sample_rate = mic.sample_rate;

    let frame = sample_rate as usize * ENVELOPE_FRAME_MS / 1_000;
    let mic_rms = delay::frame_rms(&mic.samples, frame);
    let far_rms = delay::frame_rms(&far.samples, frame);
    let activity = classify(&mic_rms, &far_rms, ENVELOPE_FRAME_MS);
    report.census = census(&activity, ENVELOPE_FRAME_MS as f32 / 1_000.0);

    if report.census.far_only <= 0.0 {
        return finish(
            dir,
            &meta_path,
            meta,
            report,
            Some(AecBypass::NoFarOnlyWindows),
            options,
        );
    }

    let meta_offset_frames = meta
        .track_offset_secs()
        .map(|secs| (secs * sample_rate as f64) as i64);

    report.delay = match options.delay_ms {
        Some(ms) => delay::DelayEstimate {
            frames: (ms * sample_rate as f32 / 1_000.0) as i64,
            source: delay::DelaySource::Measured,
            confidence: f32::INFINITY,
            segments_used: 0,
            spread_ms: 0.0,
            drift_ppm: 0.0,
        },
        None => {
            match estimate_delay(&mic, &far, &mic_rms, &far_rms, frame, meta_offset_frames) {
                Ok(estimate) => estimate,
                // Drift and the sanity gates are positive evidence that no single
                // delay works, so they stop the pass. "Could not find a clear
                // peak" is not that, and falls back to an unaligned run — a
                // 150 ms filter still covers a 30 ms delay out of its own tail.
                Err(delay::DelayError::Drifting { ppm }) => {
                    return finish(
                        dir,
                        &meta_path,
                        meta,
                        report,
                        Some(AecBypass::ClockDrift { ppm }),
                        options,
                    );
                }
                Err(delay::DelayError::PrecedesPlayback)
                | Err(delay::DelayError::Implausible)
                | Err(delay::DelayError::Unstable { .. }) => {
                    return finish(
                        dir,
                        &meta_path,
                        meta,
                        report,
                        Some(AecBypass::DelayUnmeasurable),
                        options,
                    );
                }
                Err(delay::DelayError::Inconclusive) => {
                    delay::DelayEstimate::unaligned(meta_offset_frames)
                }
            }
        }
    };

    report.config = aec::AecConfig { sample_rate };

    if options.dry_run {
        return finish(dir, &meta_path, meta, report, None, options);
    }

    // The tracks are fed **unaligned**, at their recorded offsets. AEC3 runs its
    // own delay estimator, so pre-shifting the audio would only put our estimate
    // in the way of a better one — and an alignment that is slightly too *large*
    // is unrecoverable, because it asks the filter to model an echo arriving
    // before its cause. `report.delay` is measured for the record and for the
    // drift and gap guards above, not applied here.
    //
    // Pass 1 converges the filter and its output is discarded; pass 2 re-runs
    // from the start with the filter already trained, so the opening of the
    // recording — the greeting, in a meeting — is cancelled as well as the rest.
    let mut canceller = aec::EchoCanceller::new(report.config)?;
    cancel_stream(&mut canceller, &mic.samples, &far.samples, None)?;
    let mut out = vec![0i16; mic.samples.len()];
    report.stats = cancel_stream(&mut canceller, &mic.samples, &far.samples, Some(&mut out))?;
    report.stats.reported_delay_ms = canceller.reported_delay_ms();

    debug_assert_eq!(out.len(), mic.samples.len());
    wav::write_track(&output_path, sample_rate, &out)?;
    report.output = Some(output_path);

    finish(dir, &meta_path, meta, report, None, options)
}

/// Measures the bulk delay from far-only stretches spread across the file.
fn estimate_delay(
    mic: &wav::Track,
    far: &wav::Track,
    mic_rms: &[f32],
    far_rms: &[f32],
    frame: usize,
    meta_offset_frames: Option<i64>,
) -> Result<delay::DelayEstimate, delay::DelayError> {
    let sample_rate = mic.sample_rate;
    let headroom = delay::search_headroom(sample_rate);
    let segment_len = sample_rate as usize * SEGMENT_MS / 1_000;
    let min_run = MIN_SEGMENT_MS / ENVELOPE_FRAME_MS;

    let far_only = delay::far_only_frames(mic_rms, far_rms);
    let runs = contiguous_runs(&far_only, min_run);

    let mut segments = Vec::new();
    let stride = runs.len().div_ceil(DELAY_STRATA).max(1);
    for run in runs.iter().step_by(stride) {
        let start = run.0 * frame;
        // The near window starts at the same file position as the reference and
        // runs on past it, so the whole plausible lag range is inside it.
        if start + segment_len > far.samples.len()
            || start + segment_len + headroom > mic.samples.len()
        {
            continue;
        }

        let Some((lag, confidence)) = delay::segment_lag(
            &mic.samples[start..start + segment_len + headroom],
            &far.samples[start..start + segment_len],
            sample_rate,
        ) else {
            continue;
        };

        segments.push(delay::SegmentLag {
            at_secs: start as f32 / sample_rate as f32,
            frames: lag as i64,
            confidence,
        });
    }

    delay::combine(&segments, sample_rate, meta_offset_frames)
}

/// Groups frame indices into runs of at least `min_run` consecutive frames,
/// returning `(start_frame, length)` for each.
fn contiguous_runs(frames: &[usize], min_run: usize) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut start = None;
    let mut previous = None;

    for &index in frames {
        match previous {
            Some(p) if index == p + 1 => {}
            _ => {
                if let (Some(s), Some(p)) = (start, previous)
                    && p + 1 - s >= min_run
                {
                    runs.push((s, p + 1 - s));
                }
                start = Some(index);
            }
        }
        previous = Some(index);
    }
    if let (Some(s), Some(p)) = (start, previous)
        && p + 1 - s >= min_run
    {
        runs.push((s, p + 1 - s));
    }
    runs
}

/// Feeds both tracks through the canceller a frame at a time.
///
/// With `out` as `None` this is the converging pass and the result is thrown
/// away; with `Some` it also writes the cancelled audio.
fn cancel_stream(
    canceller: &mut aec::EchoCanceller,
    near: &[i16],
    far: &[i16],
    mut out: Option<&mut [i16]>,
) -> Result<aec::AecStats, ProcessError> {
    let n = canceller.frame_size();
    let mut near_frame = vec![0i16; n];
    let mut far_frame = vec![0i16; n];
    let mut out_frame = vec![0i16; n];

    // Classified at the canceller's own frame rate, so each frame's energy lands
    // in the right bucket.
    //
    // ERLE is measured over *far-only* frames, not every frame where the far end
    // is active. That distinction is the whole measurement: in a double-talk
    // frame the mic holds the user's voice as well as the echo, and the voice is
    // not removable, so total energy barely drops however well the echo is
    // cancelled. Averaging those in reported 2.2 dB on a recording whose
    // far-only frames were doing far better — a metric that made a working
    // canceller look broken.
    let near_rms = delay::frame_rms(near, n);
    let far_rms = delay::frame_rms(far, n);
    // AEC3's frame is 10 ms at whatever rate it was given — that is what
    // `num_samples_per_frame` means — so the classification frame is 10 ms too.
    let activity = classify(&near_rms, &far_rms, 10);

    let mut stats = aec::AecStats::default();
    let (mut echo_in, mut echo_out) = (0.0f64, 0.0f64);
    let (mut quiet_in, mut quiet_out) = (0.0f64, 0.0f64);
    let (mut both_in, mut both_out) = (0.0f64, 0.0f64);

    for (index, start) in (0..near.len()).step_by(n).enumerate() {
        let end = (start + n).min(near.len());
        let len = end - start;
        near_frame[..len].copy_from_slice(&near[start..end]);
        near_frame[len..].fill(0);

        // Both ends clamped, not just the upper one. The mic track is routinely
        // *longer* than the system track — 1024 frames on the reference
        // recording, a callback buffer at each end — so once the loop passes the
        // end of the far track, an unclamped start would be greater than `end`
        // and slicing would panic rather than yielding an empty range.
        let far_start = start.min(far.len());
        let far_end = (start + n).min(far.len());
        let far_len = far_end - far_start;
        far_frame[..far_len].copy_from_slice(&far[far_start..far_end]);
        far_frame[far_len..].fill(0);

        canceller.cancel_frame(&near_frame, &far_frame, &mut out_frame)?;

        if let Some(out) = out.as_deref_mut() {
            out[start..end].copy_from_slice(&out_frame[..len]);
        }

        let energy = |s: &[i16]| -> f64 { s.iter().map(|&v| f64::from(v) * f64::from(v)).sum() };
        let (a, b) = (energy(&near_frame[..len]), energy(&out_frame[..len]));
        match activity.get(index) {
            Some(Activity::FarOnly) => {
                echo_in += a;
                echo_out += b;
            }
            Some(Activity::NearOnly) => {
                quiet_in += a;
                quiet_out += b;
            }
            Some(Activity::DoubleTalk) => {
                both_in += a;
                both_out += b;
            }
            // Silence carries no signal either way, and a frame past the end of
            // the classification is the zero-padded tail.
            _ => {}
        }
        stats.frames += 1;
    }

    let db = |a: f64, b: f64| -> Option<f32> {
        (a > 0.0 && b > 0.0).then(|| (10.0 * (a / b).log10()) as f32)
    };
    stats.erle_db = db(echo_in, echo_out);
    stats.near_gain_db = db(quiet_out, quiet_in);
    stats.double_talk_gain_db = db(both_out, both_in);
    Ok(stats)
}

/// Records the outcome in `meta.json` and returns the report.
///
/// Always called, including on every bypass path, so a recording can always say
/// what the pass decided and why.
fn finish(
    dir: &Path,
    meta_path: &Path,
    mut meta: Meta,
    mut report: AecReport,
    bypass: Option<AecBypass>,
    options: ProcessOptions,
) -> Result<AecReport, ProcessError> {
    report.bypass = bypass;

    if options.dry_run {
        return Ok(report);
    }

    meta.aec = Some(AecInfo {
        // Relative to `dir`, which is the convention every path in `meta.json`
        // follows — see `meta::resolve_track_path`.
        path: report
            .output
            .as_ref()
            .and_then(|p| p.strip_prefix(dir).ok())
            .map(|p| p.to_string_lossy().into_owned()),
        version: AEC_VERSION,
        delay_frames: report.delay.frames,
        delay_source: report.delay.source.as_str().to_string(),
        delay_confidence: report.delay.confidence,
        delay_spread_ms: report.delay.spread_ms,
        drift_ppm: report.delay.drift_ppm,
        reported_delay_ms: report.stats.reported_delay_ms,
        erle_db: report.stats.erle_db,
        near_gain_db: report.stats.near_gain_db,
        silence_secs: report.census.silence,
        near_only_secs: report.census.near_only,
        far_only_secs: report.census.far_only,
        double_talk_secs: report.census.double_talk,
        far_gap_secs: report.far_gap_secs,
        bypassed: Aec.declined_kind(report.bypass.as_ref()),
    });
    meta.write(meta_path)?;

    Ok(report)
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
            transcript: None,
            diarization: None,
            live: None,
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

        let activity = classify(&mic, &far, 100);
        assert_eq!(activity[0], Activity::Silence);
        assert_eq!(activity[1], Activity::NearOnly);
        assert_eq!(activity[2], Activity::FarOnly);
        assert_eq!(activity[3], Activity::DoubleTalk);
    }

    /// Classification must not run past the end of the shorter track, which is
    /// the normal case — the two tracks are never exactly the same length.
    #[test]
    fn classify_stops_at_the_shorter_track() {
        assert_eq!(classify(&[100.0; 10], &[100.0; 4], 100).len(), 4);
        assert_eq!(classify(&[100.0; 3], &[100.0; 9], 100).len(), 3);
        assert!(classify(&[], &[100.0; 9], 100).is_empty());
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

    /// Runs shorter than the minimum are unusable for delay measurement — too
    /// little signal to correlate — and including them would fill the segment
    /// list with noise, crowding out the good measurements.
    #[test]
    fn contiguous_runs_groups_and_filters_by_length() {
        // Two runs: 0..3 (length 3) and 10..15 (length 5).
        let frames = [0, 1, 2, 10, 11, 12, 13, 14];
        assert_eq!(contiguous_runs(&frames, 1), vec![(0, 3), (10, 5)]);
        assert_eq!(contiguous_runs(&frames, 4), vec![(10, 5)]);
        assert_eq!(contiguous_runs(&frames, 6), vec![]);
        assert_eq!(contiguous_runs(&[], 1), vec![]);
        assert_eq!(contiguous_runs(&[7], 1), vec![(7, 1)]);
    }

    /// The bug this guards destroys the user's only copy of the audio, so it is
    /// worth asserting rather than assuming: the pass reads `mic.wav` and must
    /// never write to it.
    #[test]
    fn the_output_path_is_never_the_input_path() {
        assert_ne!(OUTPUT_NAME, "mic.wav");
        assert_ne!(OUTPUT_NAME, "system.wav");

        let dir = Path::new("/tmp/recording");
        assert_ne!(dir.join(OUTPUT_NAME), dir.join("mic.wav"));
        // And the temporary file the writer renames from is distinct again, so a
        // killed pass cannot leave something that looks like a finished track.
        let tmp = dir.join(OUTPUT_NAME).with_extension("wav.tmp");
        assert_ne!(tmp, dir.join(OUTPUT_NAME));
        assert!(tmp.to_string_lossy().ends_with(".wav.tmp"));
    }
}
