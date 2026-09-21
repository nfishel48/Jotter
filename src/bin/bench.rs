//! `jotter-bench` — transcribe a corpus, for `benchmarks/`.
//!
//! Developer tooling behind the non-default `bench` feature, and never shipped.
//! It exists because `jotter transcribe` is the wrong shape for a benchmark:
//! that command takes a recording directory and builds a recogniser per call,
//! and a corpus is thousands of short clips. Reloading 660 MB of model for each
//! one measures process startup, not accuracy.
//!
//! So this loads the model once and walks a manifest. What it deliberately does
//! *not* do is reimplement the pipeline — it calls
//! [`transcribe::transcribe_track`], the same function the real pass calls, so a
//! word error rate measured here is a word error rate of the shipped code.
//! `benchmarks/bench verify` re-runs a handful of items through the real
//! `jotter transcribe` and asserts the text matches, which is what keeps that
//! claim true as both sides change.
//!
//! # Two segmentations, and why both are reported
//!
//! Corpus utterances arrive pre-cut at sentence boundaries; Jotter's own path
//! cuts them itself with Silero VAD. Those measure different things, and
//! averaging them would hide which one owns an error:
//!
//! - `--segmentation none` feeds the whole clip straight to the recogniser.
//!   That is what published leaderboard figures do, so it is the number that is
//!   comparable to other people's — it measures the *model*.
//! - `--segmentation vad` runs the detector first, exactly as a real recording
//!   would. It measures the *pipeline*.
//!
//! The gap between the two is the cost of our segmentation, and is a result
//! worth having rather than an inconvenience.
//!
//! # Interface
//!
//! Line-delimited JSON in, line-delimited JSON out, because a corpus run is
//! long and a format that can be streamed can also be resumed and inspected
//! while it is still going.
//!
//! ```text
//! jotter-bench --manifest items.jsonl --out hyps.jsonl --segmentation vad
//! ```
//!
//! Input, one object per line:
//!
//! ```json
//! { "id": "1089-134686-0000", "audio": "/path/to/clip.wav" }
//! ```
//!
//! Output: one `"record": "provenance"` object first, then one
//! `"record": "item"` per clip. The provenance line is not decoration — a WER
//! without the model, the build and the detector settings that produced it
//! cannot be reproduced or compared against the next run.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};

use jotter::audio::stage::wav;
use jotter::audio::transcribe::{
    self, ENGINE_RATE, MAX_SPEECH_SECS, MIN_SILENCE_SECS, MIN_SPEECH_SECS, ParakeetTranscriber,
    Transcriber, VAD_THRESHOLD,
};
use jotter::audio::transcript::Track;
use jotter::models::{self, Role};

#[derive(Parser)]
#[command(
    name = "jotter-bench",
    version,
    about = "Transcribe a manifest of audio files, for benchmarking"
)]
struct Args {
    /// JSONL manifest: one {"id", "audio"} object per line
    #[arg(long, value_name = "PATH")]
    manifest: PathBuf,

    /// Where to write the JSONL hypotheses
    #[arg(long, value_name = "PATH")]
    out: PathBuf,

    /// How to cut the audio before decoding it
    #[arg(long, value_enum, default_value_t = Segmentation::Vad)]
    segmentation: Segmentation,

    /// Model id, from `jotter models list`
    #[arg(long, value_name = "ID")]
    model: Option<String>,

    /// Stop after this many items. For smoke runs.
    #[arg(long, value_name = "N")]
    limit: Option<usize>,

    /// Report progress to stderr as items finish
    #[arg(long)]
    progress: bool,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
enum Segmentation {
    /// Silero VAD first — the production path.
    Vad,
    /// The whole clip in one call — what leaderboard figures measure.
    None,
}

impl Segmentation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Vad => "vad",
            Self::None => "none",
        }
    }
}

/// One line of the input manifest.
#[derive(Deserialize)]
struct Item {
    id: String,
    audio: PathBuf,
}

/// One line of the output: what the recogniser made of one clip.
#[derive(Serialize)]
struct Hypothesis {
    record: &'static str,
    id: String,
    /// Segments joined with a space. `None` never happens for a clip with
    /// speech in it, but an empty decode is data, not an error: a corpus is
    /// allowed to contain a clip this model has nothing to say about, and
    /// silently dropping it would quietly improve the score.
    text: String,
    /// How many pieces the audio was cut into. Always 1 under
    /// `--segmentation none`, and the interesting number under `vad`.
    segments: usize,
    /// Seconds the detector called speech. Zero under `--segmentation none`,
    /// which runs no detector.
    speech_secs: f32,
    audio_secs: f32,
    elapsed_secs: f32,
}

/// The first line of the output, and the reason a result is reproducible.
#[derive(Serialize)]
struct Provenance {
    record: &'static str,
    jotter_version: &'static str,
    /// Bumped whenever the same audio would give different text, so two runs
    /// carrying different values are not comparable however similar they look.
    transcribe_version: u32,
    model_id: &'static str,
    engine: &'static str,
    segmentation: &'static str,
    /// Thread count changes the numbers, so it is recorded rather than assumed.
    threads: i32,
    engine_rate: i32,
    /// The detector's settings, read from the code that ran rather than
    /// re-declared in the harness, where they could drift.
    vad: VadSettings,
    items: usize,
}

#[derive(Serialize)]
struct VadSettings {
    threshold: f32,
    min_silence_secs: f32,
    min_speech_secs: f32,
    max_speech_secs: f32,
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let items = read_manifest(&args.manifest, args.limit)?;
    if items.is_empty() {
        return Err(format!("{} holds no items", args.manifest.display()).into());
    }

    let model = match args.model.as_deref() {
        Some(id) => models::find(id)
            .ok_or_else(|| format!("no model with id {id} — try `jotter models list`"))?,
        None => models::DEFAULT_TRANSCRIPTION_MODEL,
    };

    // Resolved once, up front, so a missing download fails before the first
    // clip rather than after an hour of them.
    let recogniser_model = model.resolve().map_err(|m| {
        format!(
            "{model_id}: {m} — run `jotter models pull`",
            model_id = model.id
        )
    })?;
    let transcriber = ParakeetTranscriber::create(&recogniser_model)?;

    // The detector is a second model, and only the VAD path needs it.
    let vad_model = match args.segmentation {
        Segmentation::Vad => {
            let resolved = models::SILERO_VAD.resolve().map_err(|m| {
                format!("{}: {m} — run `jotter models pull`", models::SILERO_VAD.id)
            })?;
            Some(
                resolved
                    .path(Role::Vad)
                    .ok_or("the voice activity detector has no model file")?
                    .to_string_lossy()
                    .into_owned(),
            )
        }
        Segmentation::None => None,
    };

    let mut out = BufWriter::new(File::create(&args.out)?);
    writeln!(
        out,
        "{}",
        serde_json::to_string(&Provenance {
            record: "provenance",
            jotter_version: env!("CARGO_PKG_VERSION"),
            transcribe_version: transcribe::TRANSCRIBE_VERSION,
            model_id: model.id,
            engine: model.engine,
            segmentation: args.segmentation.as_str(),
            threads: transcribe::threads(),
            engine_rate: ENGINE_RATE,
            vad: VadSettings {
                threshold: VAD_THRESHOLD,
                min_silence_secs: MIN_SILENCE_SECS,
                min_speech_secs: MIN_SPEECH_SECS,
                max_speech_secs: MAX_SPEECH_SECS,
            },
            items: items.len(),
        })?
    )?;

    let total = items.len();
    for (index, item) in items.into_iter().enumerate() {
        let started = Instant::now();
        let decoded = match args.segmentation {
            Segmentation::Vad => vad_pass(&item.audio, &transcriber, vad_model.clone())?,
            Segmentation::None => whole_clip(&item.audio, &transcriber)?,
        };

        writeln!(
            out,
            "{}",
            serde_json::to_string(&Hypothesis {
                record: "item",
                id: item.id,
                text: decoded.text,
                segments: decoded.segments,
                speech_secs: decoded.speech_secs,
                audio_secs: decoded.audio_secs,
                elapsed_secs: started.elapsed().as_secs_f32(),
            })?
        )?;
        // Flushed per item so a run that is killed halfway still holds every
        // result it reached, and so progress can be watched from another shell.
        out.flush()?;

        if args.progress {
            eprintln!("{}/{total}", index + 1);
        }
    }

    Ok(())
}

/// What one clip produced, whichever segmentation made it.
struct Decoded {
    text: String,
    segments: usize,
    speech_secs: f32,
    audio_secs: f32,
}

/// The production path: the detector cuts the audio, exactly as it would for a
/// real recording.
fn vad_pass(
    path: &Path,
    transcriber: &dyn Transcriber,
    vad_model: Option<String>,
) -> Result<Decoded, Box<dyn std::error::Error>> {
    let audio_secs = duration_secs(path)?;
    // `Track::Mic` is arbitrary here and does not reach the output: a corpus
    // clip has no second track for it to be distinguished from. It matters in
    // `meeting.py`, which drives the real two-track pass instead.
    let result =
        transcribe::transcribe_track(path, Track::Mic, transcriber, vad_model, &mut |_| {})?;

    Ok(Decoded {
        // Joined with a space because the scorer normalises and splits on
        // whitespace anyway; segment boundaries are not word boundaries and
        // pretending otherwise would invent punctuation.
        text: result
            .segments
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        segments: result.segments.len(),
        speech_secs: result.speech_secs,
        audio_secs,
    })
}

/// The whole clip in one call, which is what published figures measure.
///
/// Resampling is done here rather than left to the recogniser for the same
/// reason the real pass does it up front: one resampler, one definition of what
/// a sample index means.
fn whole_clip(
    path: &Path,
    transcriber: &dyn Transcriber,
) -> Result<Decoded, Box<dyn std::error::Error>> {
    let audio = wav::read_track(path)?;
    let rate = audio.sample_rate.max(1);
    let float: Vec<f32> = audio.samples.iter().map(|&s| s as f32 / 32_768.0).collect();

    let samples = if rate as i32 == ENGINE_RATE {
        float
    } else {
        let resampler = sherpa_onnx::LinearResampler::create(rate as i32, ENGINE_RATE)
            .ok_or("could not create the resampler")?;
        let mut out = resampler.resample(&float, false);
        // The tail the resampler is still holding. Dropping it truncates the
        // end of every clip, which on short corpus utterances is a whole word.
        out.extend(resampler.resample(&[], true));
        out
    };

    let audio_secs = audio.samples.len() as f32 / rate as f32;
    Ok(Decoded {
        text: transcriber.transcribe(&samples).unwrap_or_default(),
        segments: 1,
        speech_secs: 0.0,
        audio_secs,
    })
}

/// Clip length without decoding it, for the VAD path's report.
fn duration_secs(path: &Path) -> Result<f32, hound::Error> {
    let reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    Ok(reader.duration() as f32 / spec.sample_rate.max(1) as f32)
}

fn read_manifest(
    path: &Path,
    limit: Option<usize>,
) -> Result<Vec<Item>, Box<dyn std::error::Error>> {
    let mut items = Vec::new();
    for (number, line) in BufReader::new(File::open(path)?).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        // The line number is in the message because a manifest is generated,
        // and "invalid JSON" without a position sends you to read all 2620.
        let item: Item = serde_json::from_str(&line)
            .map_err(|e| format!("{}:{}: {e}", path.display(), number + 1))?;
        items.push(item);
        if limit.is_some_and(|n| items.len() >= n) {
            break;
        }
    }
    Ok(items)
}
