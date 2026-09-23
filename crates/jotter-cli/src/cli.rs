//! The subcommands.
//!
//!   jotter devices
//!   jotter record --duration 10
//!   jotter record --system <id> --mic <id> --duration 600
//!   jotter process ~/Documents/Jotter/<dir>
//!
//! Printing and argument handling only. Every decision about audio — what to
//! capture, which passes to run and in what order — is the library's, so the
//! command and a program embedding the library cannot drift apart. The two
//! macOS permissions are granted separately, and a terminal session that prints
//! device ids and frame counts is what makes each of them debuggable.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Subcommand, ValueEnum};

use jotter::audio::{
    self, FinishOptions, FinishReport, FinishStage, RecordConfig, Sources, devices::DeviceChoice,
};
use jotter::config::{self, Settings};
use jotter::telemetry::{Prop, Surface, Telemetry, events};

#[derive(Subcommand)]
pub enum Command {
    /// Capture mic + system audio to two WAV tracks
    Record(RecordArgs),
    /// List audio devices and show which ones can be tapped for system audio
    #[command(alias = "list")]
    Devices,
    /// Remove speaker echo from the mic track of a finished recording
    #[cfg(feature = "aec")]
    Process(ProcessArgs),
    /// Show or change whether anonymous usage data is sent
    Telemetry(TelemetryArgs),
    /// Download and inspect the speech models transcription needs
    #[cfg(feature = "transcribe")]
    Models(ModelsArgs),
    /// Turn a finished recording into a transcript
    #[cfg(feature = "transcribe")]
    Transcribe(TranscribeArgs),
    /// Work out who said what, in a recording that already has a transcript
    #[cfg(feature = "diarize")]
    Diarize(DiarizeArgs),
}

/// `jotter diarize <dir>`.
///
/// Separate from `transcribe` rather than a flag on it, because the two have
/// very different costs: re-running this to try a different speaker count is
/// seconds of clustering, and re-running transcription is the whole recogniser
/// over the whole meeting. Anyone tuning the first should not have to pay the
/// second.
#[cfg(feature = "diarize")]
#[derive(Args)]
pub struct DiarizeArgs {
    /// Recording directory, containing meta.json and a transcript.json
    dir: PathBuf,

    /// Report what would happen and write nothing
    #[arg(long)]
    dry_run: bool,

    /// Re-label even if the transcript already carries current speakers
    #[arg(long)]
    force: bool,

    /// How many people were on the call. Required, unless `diarize_speakers`
    /// is set in settings.json — the pass declines without it
    #[arg(long, value_name = "N")]
    speakers: Option<u8>,
}

/// `jotter transcribe <dir>`.
///
/// The twin of `process`: touches no audio devices, so it needs no permissions
/// and no macOS bundle, and it works on any recording directory — including
/// ones made before transcription existed.
#[cfg(feature = "transcribe")]
#[derive(Args)]
pub struct TranscribeArgs {
    /// Recording directory, containing meta.json and at least one track
    dir: PathBuf,

    /// Report what would happen and write nothing
    #[arg(long)]
    dry_run: bool,

    /// Re-transcribe even if a current transcript.json already exists
    #[arg(long)]
    force: bool,

    /// Model id, from `jotter models list`
    #[arg(long, value_name = "ID")]
    model: Option<String>,

    /// Which tracks to read. Both is the point of recording two.
    #[arg(long, value_enum, default_value_t = TracksArg::Both)]
    tracks: TracksArg,
}

/// Mirrors `audio::transcribe::Tracks` rather than deriving `ValueEnum` on it,
/// for the reason `SourcesArg` exists: `audio` compiles in a CLI-free build and
/// must not depend on clap.
#[cfg(feature = "transcribe")]
#[derive(Clone, Copy, ValueEnum)]
enum TracksArg {
    Mic,
    System,
    Both,
}

#[cfg(feature = "transcribe")]
impl From<TracksArg> for audio::transcribe::Tracks {
    fn from(arg: TracksArg) -> Self {
        match arg {
            TracksArg::Mic => Self::Mic,
            TracksArg::System => Self::System,
            TracksArg::Both => Self::Both,
        }
    }
}

/// `jotter models …`.
///
/// Its own subcommand rather than a flag on `transcribe`, because the point is
/// that fetching a model is a separate, deliberate act. Transcription declines
/// when a model is missing and says to run this; it never downloads 660 MB
/// because a meeting ended.
#[cfg(feature = "transcribe")]
#[derive(Args)]
pub struct ModelsArgs {
    #[command(subcommand)]
    command: ModelsCommand,
}

#[cfg(feature = "transcribe")]
#[derive(Subcommand)]
pub enum ModelsCommand {
    /// Show every known model and whether it is ready to use
    List,
    /// Print where models are kept
    Path,
    /// Download a model. Files already present are left alone.
    Pull(PullArgs),
}

#[cfg(feature = "transcribe")]
#[derive(Args)]
pub struct PullArgs {
    /// Model id, from `jotter models list`. Defaults to everything
    /// transcription needs.
    #[arg(long, value_name = "ID")]
    model: Option<String>,
}

/// `jotter process <dir>`.
///
/// Touches no audio devices, so unlike `record` it needs no permissions and no
/// macOS bundle. That is what makes it usable for iterating on the canceller —
/// and it works on recordings made before echo cancellation existed.
#[cfg(feature = "aec")]
#[derive(Args)]
pub struct ProcessArgs {
    /// Recording directory, containing mic.wav, system.wav and meta.json
    dir: PathBuf,

    /// Measure and report, but write nothing
    #[arg(long)]
    dry_run: bool,

    /// Reprocess even if a current mic_aec.wav already exists
    #[arg(long)]
    force: bool,

    /// Skip delay measurement and use this value. For debugging a recording
    /// whose delay the estimator gets wrong.
    #[arg(long, value_name = "MS")]
    delay_ms: Option<f32>,
}

#[derive(Args)]
pub struct TelemetryArgs {
    /// Start sending anonymous usage and crash reports
    #[arg(long, conflicts_with = "disable")]
    enable: bool,

    /// Stop sending anonymous usage and crash reports
    #[arg(long)]
    disable: bool,
}

#[derive(Args)]
pub struct RecordArgs {
    /// Which sources to record. The two macOS permissions are granted
    /// separately, so use this to test them one at a time.
    #[arg(long, value_enum, default_value_t = SourcesArg::Both)]
    only: SourcesArg,

    /// Microphone device id (default: built-in mic)
    #[arg(long, value_name = "ID")]
    mic: Option<String>,

    /// Device to tap for system audio (default: default output)
    #[arg(long, value_name = "ID")]
    system: Option<String>,

    /// Stop after N seconds (default: until Enter is pressed)
    #[arg(long, value_name = "SECS")]
    duration: Option<u64>,

    /// Output directory (default: ~/Documents/Jotter/<timestamp>)
    #[arg(long, value_name = "DIR")]
    out: Option<PathBuf>,

    /// Tap a duplex device anyway. Diagnostic only: cpal will record its
    /// microphone, not system audio.
    #[arg(long)]
    force_system_on_duplex: bool,

    /// Remove speaker echo from the mic track when the recording ends.
    /// Defaults to the stored setting; `--no-aec` forces it off.
    #[cfg(feature = "aec")]
    #[arg(long, overrides_with = "no_aec")]
    aec: bool,

    #[cfg(feature = "aec")]
    #[arg(long, overrides_with = "aec")]
    no_aec: bool,

    /// Transcribe the recording when it ends. Defaults to the stored setting;
    /// `--no-transcribe` forces it off.
    #[cfg(feature = "transcribe")]
    #[arg(long, overrides_with = "no_transcribe")]
    transcribe: bool,

    #[cfg(feature = "transcribe")]
    #[arg(long, overrides_with = "transcribe")]
    no_transcribe: bool,

    /// Work out who said what once the transcript exists. Defaults to the
    /// stored setting; `--no-diarize` forces it off.
    #[cfg(feature = "diarize")]
    #[arg(long, overrides_with = "no_diarize")]
    diarize: bool,

    #[cfg(feature = "diarize")]
    #[arg(long, overrides_with = "diarize")]
    no_diarize: bool,
}

/// Mirrors `audio::Sources` rather than deriving `ValueEnum` on it directly:
/// `audio` compiles in a CLI-free build, and should not depend on clap.
#[derive(Clone, Copy, ValueEnum)]
enum SourcesArg {
    Mic,
    System,
    Both,
}

impl SourcesArg {
    fn telemetry_name(self) -> &'static str {
        match self {
            Self::Mic => "mic",
            Self::System => "system",
            Self::Both => "both",
        }
    }
}

impl From<SourcesArg> for Sources {
    fn from(arg: SourcesArg) -> Self {
        match arg {
            SourcesArg::Mic => Sources::MicOnly,
            SourcesArg::System => Sources::SystemOnly,
            SourcesArg::Both => Sources::Both,
        }
    }
}

pub fn run(command: Command) -> Result<(), Box<dyn std::error::Error>> {
    // `telemetry` is handled before the worker starts: it only edits the
    // settings file, and starting a reporting client in order to turn reporting
    // off would be a strange thing to do.
    if let Command::Telemetry(args) = command {
        return telemetry_command(args);
    }

    let mut settings = Settings::load();
    let telemetry = Telemetry::init(Surface::Cli, &mut settings);
    let first_run = !settings.telemetry_notice_seen;
    if first_run && telemetry.is_active() {
        telemetry_notice(&mut settings);
    }
    telemetry.track(events::APP_STARTED, &[("is_first_run", first_run.into())]);

    let result = match command {
        Command::Devices => list_devices(&telemetry),
        Command::Record(args) => record(args, &settings, &telemetry),
        #[cfg(feature = "aec")]
        Command::Process(args) => process(args, &telemetry),
        #[cfg(feature = "transcribe")]
        Command::Models(args) => models(args),
        #[cfg(feature = "transcribe")]
        Command::Transcribe(args) => transcribe(args, &telemetry),
        #[cfg(feature = "diarize")]
        Command::Diarize(args) => diarize(args, &telemetry),
        Command::Telemetry(_) => unreachable!("handled above"),
    };

    // Explicit rather than relying on `Drop`: this is the one place a CLI run
    // can lose its whole queue, since the process exits immediately after.
    telemetry.track(events::APP_EXITED, &[("reason", "cli_done".into())]);
    telemetry.shutdown();

    result
}

/// Say, once, that this build reports anonymous usage and how to stop it.
///
/// Telemetry is on by default, and this is the only place a user of the
/// command is told so. Shown only when something will actually be sent — a
/// build without an API key, or a user who has already opted out, has nothing
/// to give notice of — and on stderr, so it never lands in output someone is
/// piping into another program.
fn telemetry_notice(settings: &mut Settings) {
    eprintln!(
        "Jotter sends anonymous usage and crash reports, which is how recording failures\n\
         get found and fixed. Never audio, transcripts, file names or device names.\n\
         Turn it off with `jotter telemetry --disable` or DO_NOT_TRACK=1; see\n\
         docs/TELEMETRY.md for exactly what is sent. This notice is shown once.\n"
    );
    settings.telemetry_notice_seen = true;
    // Best effort, like the install id: an unwritable config directory means
    // the notice appears again next run, which is the safe way to fail.
    let _ = settings.save();
}

/// `jotter telemetry [--enable|--disable]`, and with neither, a status report.
///
/// The persistent way to opt out; the environment variables are the per-run
/// one. Works the same on a server or over SSH as anywhere else.
fn telemetry_command(args: TelemetryArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut settings = Settings::load();

    if args.enable || args.disable {
        settings.telemetry_enabled = args.enable;
        settings.telemetry_notice_seen = true;
        settings.save()?;
    }

    let stored = if settings.telemetry_enabled {
        "enabled"
    } else {
        "disabled"
    };
    println!("telemetry: {stored}");
    println!("  config:  {}", config::path().display());

    if !cfg!(feature = "telemetry") {
        println!("  note:    this build has telemetry compiled out and sends nothing");
    }

    match config::env_override() {
        config::EnvOverride::ForceOff => {
            println!("  note:    overridden to OFF by DO_NOT_TRACK / JOTTER_TELEMETRY");
        }
        config::EnvOverride::ForceOn => {
            println!("  note:    overridden to ON by JOTTER_TELEMETRY");
        }
        config::EnvOverride::Unset => {}
    }

    println!("\nSee docs/TELEMETRY.md for exactly what is collected.");
    Ok(())
}

fn record(
    args: RecordArgs,
    settings: &Settings,
    telemetry: &Telemetry,
) -> Result<(), Box<dyn std::error::Error>> {
    // Read before `args` is taken apart below. None of these identify a
    // device: they are shapes of the request.
    let options = finish_options(&args, settings);
    let sources = args.only.telemetry_name();
    let mic_is_default = args.mic.is_none();
    let system_is_default = args.system.is_none();

    // The same place every other recording goes, so `jotter record` output is
    // found where the user looks for meetings. It also has to be absolute: run
    // through the macOS bundle — the only way to capture system audio there —
    // the working directory is `/`.
    let out_dir = args
        .out
        .unwrap_or_else(|| config::recordings_root().join(audio::meta::timestamp_dir_name()));

    let config = RecordConfig {
        sources: args.only.into(),
        mic: args.mic.map_or(DeviceChoice::Default, DeviceChoice::Id),
        system: args.system.map_or(DeviceChoice::Default, DeviceChoice::Id),
        out_dir,
        allow_duplex_system: args.force_system_on_duplex,
    };

    let handle = match audio::start(config) {
        Ok(handle) => handle,
        Err(e) => {
            report_failure(telemetry, "start", &e);
            return Err(e.into());
        }
    };
    telemetry.track(
        events::RECORDING_STARTED,
        &[
            ("sources", sources.into()),
            ("mic_is_default", mic_is_default.into()),
            ("system_is_default", system_is_default.into()),
            ("force_system_on_duplex", args.force_system_on_duplex.into()),
            ("fixed_duration", args.duration.is_some().into()),
        ],
    );
    let dir = handle.out_dir().to_path_buf();
    println!("recording to {}", dir.display());

    match args.duration {
        Some(secs) => {
            println!("stopping after {secs}s");
            std::thread::sleep(Duration::from_secs(secs));
        }
        None => {
            println!("press Enter to stop");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
        }
    }

    let meta = match handle.stop() {
        Ok(meta) => meta,
        Err(e) => {
            report_failure(telemetry, "stop", &e);
            return Err(e.into());
        }
    };
    telemetry.track(events::RECORDING_COMPLETED, &events::recording_props(&meta));

    println!("\nwrote {:.1}s", meta.duration_secs());
    if let Some(mic) = &meta.mic {
        report_track("mic   ", mic);
    }
    if let Some(system) = &meta.system {
        report_track("system", system);
    }
    if let Some(offset) = meta.track_offset_secs() {
        println!("track offset: {:+.3}s (system relative to mic)", offset);
    }

    // After the track report, not instead of it: the recording is the result,
    // and what the offline passes then did to it comes second. Nothing they do
    // can fail this command — the audio is on disk, and `jotter process`,
    // `transcribe` and `diarize` can redo any of them — so a failed pass is
    // printed and the exit status stays zero.
    let report = finish_recording(&dir, &options);
    track_finish(telemetry, &report);
    print_finish(&report);

    Ok(())
}

/// Which passes `record` runs: the stored settings, with this run's flags on
/// top. `--aec`/`--no-aec` and friends exist so a one-off run can opt in or out
/// without editing the config file — which is also the only way to test both
/// paths from a single build.
// Nothing to override in a build without any pass.
#[cfg_attr(
    not(any(feature = "aec", feature = "transcribe")),
    allow(unused_variables)
)]
fn finish_options(args: &RecordArgs, settings: &Settings) -> FinishOptions {
    let stored = FinishOptions::from_settings(settings);
    FinishOptions {
        #[cfg(feature = "aec")]
        aec: flag_or(args.aec, args.no_aec, stored.aec),
        #[cfg(feature = "transcribe")]
        transcribe: flag_or(args.transcribe, args.no_transcribe, stored.transcribe),
        #[cfg(feature = "diarize")]
        diarize: flag_or(args.diarize, args.no_diarize, stored.diarize),
        ..stored
    }
}

/// An `--x`/`--no-x` pair over a stored default. clap's `overrides_with` means
/// at most one of the two is set, whichever came last on the command line.
#[cfg(any(feature = "aec", feature = "transcribe"))]
fn flag_or(on: bool, off: bool, stored: bool) -> bool {
    if on {
        true
    } else if off {
        false
    } else {
        stored
    }
}

/// Run `audio::finish` over a just-stopped recording, with a progress line.
///
/// The line is rewritten in place and wiped at the end, and only on a
/// terminal, for the reason `models pull` gives: piped, carriage returns are
/// not rewrites but ordinary bytes.
fn finish_recording(dir: &Path, options: &FinishOptions) -> FinishReport {
    use std::io::{IsTerminal, Write};

    let interactive = std::io::stdout().is_terminal();
    let mut last: Option<(FinishStage, u8)> = None;
    let report = audio::finish_with_progress(dir, options, &mut |stage, fraction| {
        if !interactive {
            return;
        }
        let percent = (fraction.clamp(0.0, 1.0) * 100.0) as u8;
        if last == Some((stage, percent)) {
            return;
        }
        last = Some((stage, percent));
        let label = match stage {
            FinishStage::Aec => "removing speaker echo",
            FinishStage::Transcribe => "transcribing",
            FinishStage::Diarize => "identifying speakers",
        };
        print!("\r  {:<40}", format!("{label}… {percent}%"));
        let _ = std::io::stdout().flush();
    });
    if last.is_some() {
        print!("\r{:44}\r", "");
    }
    report
}

/// Report what the passes did. Separate from printing so that every way of
/// finishing a recording reports it the same way, whatever it shows the user.
///
/// Declines are reported like successes — the stage ran and decided — and
/// skips and failures are not, the same as the standalone commands, which
/// report nothing when a pass errors out.
#[cfg_attr(
    not(any(feature = "aec", feature = "transcribe")),
    allow(unused_variables)
)]
pub(crate) fn track_finish(telemetry: &Telemetry, report: &FinishReport) {
    #[cfg(feature = "aec")]
    if let Some(aec) = report.aec.report() {
        telemetry.track(events::RECORDING_PROCESSED, &events::aec_props(aec, false));
    }
    #[cfg(feature = "transcribe")]
    if let Some(transcript) = report.transcribe.report() {
        telemetry.track(
            events::RECORDING_TRANSCRIBED,
            &events::transcript_props(transcript),
        );
    }
    #[cfg(feature = "diarize")]
    if let Some(diarization) = report.diarize.report() {
        telemetry.track(
            events::RECORDING_DIARIZED,
            &events::diarize_props(diarization),
        );
    }
}

/// Print each pass that was attempted, in the shape the standalone commands
/// use. A pass that was switched off prints nothing, as it did not happen; one
/// that was enabled and found nothing to work on says so, since the user asked
/// for it. The echo pass is the exception: it is on by default, so a
/// single-track recording would otherwise announce, every time, that it did
/// not cancel echo it could never have had.
#[cfg_attr(
    not(any(feature = "aec", feature = "transcribe")),
    allow(unused_variables)
)]
fn print_finish(report: &FinishReport) {
    #[cfg(feature = "transcribe")]
    use audio::Skip;
    #[cfg(any(feature = "aec", feature = "transcribe"))]
    use audio::StageOutcome;

    #[cfg(feature = "aec")]
    match &report.aec {
        StageOutcome::Ran(aec) => {
            println!();
            report_aec(aec, false);
        }
        StageOutcome::Failed(e) => println!("\n  echo cancellation failed: {e}"),
        StageOutcome::Skipped(_) => {}
    }

    #[cfg(feature = "transcribe")]
    match &report.transcribe {
        StageOutcome::Ran(transcript) => {
            println!();
            report_transcript(transcript, false);
        }
        StageOutcome::Failed(e) => println!("\n  transcription failed: {e}"),
        StageOutcome::Skipped(Skip::Disabled) => {}
        StageOutcome::Skipped(skip) => println!("\n  transcription skipped: {skip}"),
    }

    #[cfg(feature = "diarize")]
    match &report.diarize {
        StageOutcome::Ran(diarization) => {
            println!();
            report_diarization(diarization, false);
        }
        StageOutcome::Failed(e) => println!("\n  speaker identification failed: {e}"),
        StageOutcome::Skipped(Skip::Disabled) => {}
        StageOutcome::Skipped(skip) => println!("\n  speaker identification skipped: {skip}"),
    }
}

/// Report a capture failure as both an event and an exception.
///
/// Note what is *not* passed: `e.to_string()`. The `Display` impl embeds the
/// device name and is written for the terminal; `kind` and `cpal_kind` are the
/// `&'static str` classifications meant to leave the machine.
fn report_failure(telemetry: &Telemetry, phase: &'static str, e: &audio::capture::CaptureError) {
    let props: Vec<Prop> = vec![
        ("phase", phase.into()),
        ("error_kind", e.kind().into()),
        ("cpal_kind", e.cpal_kind().into()),
        ("permission_shaped", e.is_permission_shaped().into()),
    ];
    telemetry.track(events::RECORDING_FAILED, &props);
    telemetry.report_error(e.kind(), e.cpal_kind(), &props);
}

/// `jotter process <dir>` — offline echo cancellation.
#[cfg(feature = "aec")]
fn process(args: ProcessArgs, telemetry: &Telemetry) -> Result<(), Box<dyn std::error::Error>> {
    use audio::process::{ProcessOptions, run};

    let options = ProcessOptions {
        dry_run: args.dry_run,
        force: args.force,
        delay_ms: args.delay_ms,
    };

    println!("processing {}", args.dir.display());
    let report = run(&args.dir, options)?;
    report_aec(&report, args.dry_run);

    telemetry.track(
        events::RECORDING_PROCESSED,
        &events::aec_props(&report, args.dry_run),
    );
    Ok(())
}

/// Prints what the pass decided, in the shape of [`report_track`].
#[cfg(feature = "aec")]
fn report_aec(report: &audio::process::AecReport, dry_run: bool) {
    let census = &report.census;
    let total = census.silence + census.near_only + census.far_only + census.double_talk;
    if total > 0.0 {
        println!(
            "  activity   silence {:.0}s  you {:.0}s  them {:.0}s  both {:.0}s",
            census.silence, census.near_only, census.far_only, census.double_talk
        );
    }

    if let Some(bypass) = report.bypass {
        println!("  SKIPPED    {bypass}");
        return;
    }

    let delay_ms = report.delay.frames as f32 * 1_000.0 / report.config.sample_rate.max(1) as f32;
    print!(
        "  delay      {:.1}ms ({}",
        delay_ms,
        report.delay.source.as_str()
    );
    if report.delay.segments_used > 0 {
        print!(
            ", {} segments, spread {:.1}ms, confidence {:.1}",
            report.delay.segments_used, report.delay.spread_ms, report.delay.confidence
        );
    }
    println!(")");
    if let Some(ms) = report.stats.reported_delay_ms {
        println!("  AEC3 delay {ms}ms (its own estimate, as a cross-check)");
    }

    // A dry run stops before the canceller, so there are no figures yet — and
    // saying "not measurable" there would blame the recording for something
    // that simply did not run.
    if dry_run {
        println!("  echo       not measured (dry run)");
    } else {
        match report.stats.erle_db {
            Some(erle) => {
                println!("  echo       {erle:.1}dB removed where system audio was playing")
            }
            None => println!("  echo       not measurable — no echo-only passages to compare"),
        }
        // The figure an ERLE number cannot show: whether the user's own voice
        // survived. Printed even when it is fine, because "fine" is the result.
        match report.stats.near_gain_db {
            Some(gain) if gain < -1.0 => println!(
                "  your voice {gain:.1}dB — the filter is cutting into it; \
                 mic.wav is unchanged and still the safe choice"
            ),
            Some(gain) => println!("  your voice {gain:+.1}dB (unchanged, as it should be)"),
            // No stretch of the user talking alone, so nothing was verified.
            // Say so: this is the check that matters, and its absence is why
            // `Meta::preferred_mic_path` will not hand the cancelled track on.
            None => println!(
                "  your voice not verified — no passage of you talking alone to check against"
            ),
        }
    }

    match &report.output {
        Some(path) => println!("  wrote      {}", path.display()),
        None => println!("  wrote      nothing (dry run)"),
    }
}

/// `jotter transcribe <dir>` — offline transcription.
#[cfg(feature = "transcribe")]
fn transcribe(
    args: TranscribeArgs,
    _telemetry: &Telemetry,
) -> Result<(), Box<dyn std::error::Error>> {
    use audio::transcribe::{TranscribeOptions, run_with_progress};
    use std::io::{IsTerminal, Write};

    // An unknown id is rejected here rather than silently falling back to the
    // default inside the stage: someone who asked for a particular model and
    // got another one has been lied to about what produced the transcript.
    if let Some(id) = args.model.as_deref()
        && jotter::models::find(id).is_none()
    {
        return Err(format!("unknown model {id:?} — see `jotter models list`").into());
    }

    let options = TranscribeOptions {
        dry_run: args.dry_run,
        force: args.force,
        model: args.model,
        tracks: args.tracks.into(),
    };

    println!("transcribing {}", args.dir.display());

    // Same rule as `models pull`: a percentage that rewrites itself is for a
    // terminal, and is line noise in a log.
    let interactive = std::io::stdout().is_terminal();
    let mut last = u8::MAX;
    let report = run_with_progress(&args.dir, options, &mut |fraction| {
        if !interactive {
            return;
        }
        let percent = (fraction.clamp(0.0, 1.0) * 100.0) as u8;
        if percent != last {
            last = percent;
            print!("\r  {percent}%");
            let _ = std::io::stdout().flush();
        }
    })?;
    if interactive && last != u8::MAX {
        print!("\r");
    }

    report_transcript(&report, args.dry_run);
    Ok(())
}

/// Prints what the pass decided, in the shape of [`report_aec`].
#[cfg(feature = "transcribe")]
fn report_transcript(report: &audio::transcribe::TranscriptReport, dry_run: bool) {
    println!("  model      {} ({})", report.model_id, report.engine);
    println!("  audio      {:.1}s across both tracks", report.audio_secs);

    if let Some(decline) = &report.decline {
        println!("  SKIPPED    {decline}");
        return;
    }

    if dry_run {
        println!("  would      transcribe (dry run — nothing was decoded)");
        return;
    }

    // Both counts, always, even when one is zero: a meeting that transcribed
    // only your own voice is a specific, recognisable failure — the system tap
    // was idle — and a single total would hide it.
    println!(
        "  speech     {:.1}s in {} segment(s) — you {}, everyone else {}",
        report.speech_secs, report.segments, report.mic_segments, report.system_segments
    );
    println!("  words      {}", report.words);

    // The number that decides whether this is usable on a given machine.
    let rtf = report.elapsed_secs / report.audio_secs.max(f32::MIN_POSITIVE);
    println!(
        "  took       {:.1}s ({rtf:.2}x realtime)",
        report.elapsed_secs
    );

    match &report.output {
        Some(path) => println!("  wrote      {}", path.display()),
        None => println!("  wrote      nothing"),
    }
}

/// `jotter diarize <dir>` — who said what.
#[cfg(feature = "diarize")]
fn diarize(args: DiarizeArgs, telemetry: &Telemetry) -> Result<(), Box<dyn std::error::Error>> {
    use audio::diarize::{DiarizeOptions, run_with_progress};
    use std::io::{IsTerminal, Write};

    // A meeting with nobody in it is a typo, not a request. Rejected here rather
    // than left to the stage so the message can name the flag.
    if args.speakers == Some(0) {
        return Err("--speakers must be at least 1".into());
    }

    let options = DiarizeOptions {
        dry_run: args.dry_run,
        force: args.force,
        // The flag wins; the stored setting is the fallback for someone who
        // always meets the same people. Neither is a decline the stage reports,
        // rather than an error here, because `--dry-run` should still be able to
        // say what else is or is not ready.
        speakers: args.speakers.or_else(|| Settings::load().speaker_count()),
    };

    println!("identifying speakers in {}", args.dir.display());

    // Same rule as `transcribe`, with one difference worth knowing about: the
    // bar stops a fifth of the way across and stays there. Everything after the
    // resample is a single call into ONNX that cannot report progress — see the
    // note in `audio::diarize`.
    let interactive = std::io::stdout().is_terminal();
    let mut last = u8::MAX;
    let report = run_with_progress(&args.dir, options, &mut |fraction| {
        if !interactive {
            return;
        }
        let percent = (fraction.clamp(0.0, 1.0) * 100.0) as u8;
        if percent != last {
            last = percent;
            print!("\r  reading audio… {percent}%");
            let _ = std::io::stdout().flush();
        }
    })?;
    if interactive && last != u8::MAX {
        print!("\r{:30}\r", "");
    }

    report_diarization(&report, args.dry_run);
    telemetry.track(events::RECORDING_DIARIZED, &events::diarize_props(&report));
    Ok(())
}

/// Prints what the pass decided, in the shape of [`report_transcript`].
#[cfg(feature = "diarize")]
fn report_diarization(report: &audio::diarize::DiarizeReport, dry_run: bool) {
    println!(
        "  models     {} + {} ({})",
        report.segmentation_model_id, report.embedding_model_id, report.engine
    );
    println!("  audio      {:.1}s of system track", report.audio_secs);

    if let Some(decline) = &report.decline {
        println!("  SKIPPED    {decline}");
        return;
    }

    if dry_run {
        println!("  would      identify speakers (dry run — no model was run)");
        return;
    }

    println!("  speakers   {}", report.speakers);

    // Both figures, always. A pass that found three speakers but could only
    // place half the segments is a specific, recognisable failure — the turns
    // and the transcript's segments disagree about where the pauses are — and a
    // bare speaker count would hide it.
    let missed = report
        .system_segments
        .saturating_sub(report.attributed_segments);
    println!(
        "  labelled   {} of {} segment(s){}",
        report.attributed_segments,
        report.system_segments,
        if missed > 0 {
            format!(" — {missed} left unattributed")
        } else {
            String::new()
        }
    );

    let rtf = report.elapsed_secs / report.audio_secs.max(f32::MIN_POSITIVE);
    println!(
        "  took       {:.1}s ({rtf:.2}x realtime)",
        report.elapsed_secs
    );

    match &report.output {
        // "updated", not "wrote": this pass fills in a file the transcription
        // pass created, and saying otherwise would suggest a second artifact.
        Some(path) => println!("  updated    {}", path.display()),
        None => println!("  updated    nothing"),
    }
}

/// `jotter models list | path | pull`.
///
/// Takes no `Telemetry`: which models someone has on disk is a statement about
/// what they transcribe, and there is no aggregate worth that.
#[cfg(feature = "transcribe")]
fn models(args: ModelsArgs) -> Result<(), Box<dyn std::error::Error>> {
    use jotter::models;

    match args.command {
        ModelsCommand::Path => {
            println!("{}", models::models_root().display());
        }

        ModelsCommand::List => {
            println!("{:<28} {:>8}  {:<10} MODEL", "ID", "SIZE", "STATE");
            for model in models::CATALOGUE {
                // Every problem, not just the first, so "3 files missing" does
                // not read the same as "one truncated file".
                let state = match model.resolve() {
                    Ok(_) => "ready".to_string(),
                    Err(missing) => format!("{} missing", missing.problems.len()),
                };
                println!(
                    "{:<28} {:>8}  {:<10} {}",
                    model.id,
                    human_bytes(model.bytes()),
                    state,
                    model.description
                );
            }
            println!("\nkept in {}", models::models_root().display());
        }

        ModelsCommand::Pull(args) => {
            // No id means "everything the offline passes need": the recogniser,
            // the voice-activity model, and the two diarization models. They are
            // separate catalogue entries — a recogniser alone cannot run the
            // transcription stage — and the diarization pair is included rather
            // than left opt-in because it is 35 MB against the recogniser's 630.
            // Making someone come back for a second deliberate download would
            // cost them more attention than the bytes cost their disk, and would
            // leave `diarize_enabled` dead for everyone who pulled already.
            let wanted: Vec<&'static models::Model> =
                match args.model.as_deref() {
                    Some(id) => vec![models::find(id).ok_or_else(|| {
                        format!("unknown model {id:?} — see `jotter models list`")
                    })?],
                    None => vec![
                        models::DEFAULT_TRANSCRIPTION_MODEL,
                        &models::SILERO_VAD,
                        models::DEFAULT_SEGMENTATION_MODEL,
                        models::DEFAULT_EMBEDDING_MODEL,
                    ],
                };

            let total: u64 = wanted.iter().map(|m| m.bytes()).sum();
            println!(
                "pulling {} model(s), up to {} into {}",
                wanted.len(),
                human_bytes(total),
                models::models_root().display()
            );

            for model in wanted {
                println!("\n{} — {}", model.id, model.description);
                pull_one(model)?;
            }
            println!("\ndone");

            // Transcription defaults off precisely because it cannot work
            // before this command has been run, so the moment it can is the
            // moment to say how to turn it on. Only when it is still off:
            // repeating this at someone who has already enabled it is noise.
            if !Settings::load().transcribe_enabled {
                println!(
                    "\ntranscribe an existing recording with `jotter transcribe <dir>`,\n\
                     or a single run with `jotter record --transcribe`. To do it for\n\
                     every recording, set \"transcribe_enabled\": true in\n  {}",
                    config::path().display()
                );
            }
        }
    }

    Ok(())
}

/// Fetch one model, printing a line per asset.
///
/// The running percentage is rewritten in place with `\r`, and only when stdout
/// is a terminal. Piped — a CI log, a `tee`, a file — carriage returns are not
/// rewrites but ordinary bytes, and a 652 MB download would leave one
/// unreadable line a hundred fragments long. There the per-asset summary line
/// is the whole output, which is what a log wants anyway.
#[cfg(feature = "transcribe")]
fn pull_one(model: &'static jotter::models::Model) -> Result<(), Box<dyn std::error::Error>> {
    use jotter::models::fetch::{self, Progress};
    use std::io::{IsTerminal, Write};

    let interactive = std::io::stdout().is_terminal();
    let mut last_percent = u64::MAX;

    fetch::fetch(model, &mut |event| match event {
        Progress::Skipped { asset } => println!("  {:<20} already present", asset.name),
        Progress::Started { asset } => {
            last_percent = u64::MAX;
            if interactive {
                print!("  {:<20} 0%", asset.name);
                let _ = std::io::stdout().flush();
            }
        }
        Progress::Bytes { asset, done } => {
            if !interactive {
                return;
            }
            let percent = done * 100 / asset.bytes.max(1);
            if percent != last_percent {
                last_percent = percent;
                print!("\r  {:<20} {percent}%", asset.name);
                let _ = std::io::stdout().flush();
            }
        }
        Progress::Finished { asset } => {
            let lead = if interactive { "\r" } else { "" };
            println!("{lead}  {:<20} {} ✓", asset.name, human_bytes(asset.bytes));
        }
    })?;
    Ok(())
}

/// Sizes a human can compare at a glance. Powers of 1024, one decimal.
#[cfg(feature = "transcribe")]
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn report_track(label: &str, track: &audio::meta::TrackInfo) {
    let secs = track.frames as f64 / track.sample_rate.max(1) as f64;
    println!(
        "  {label}  {:>7.1}s  {} Hz  {}ch→mono  {}",
        secs, track.sample_rate, track.source_channels, track.device_name
    );
    if track.frames == 0 {
        println!("           ^ no audio captured — check permissions and device choice");
    }
    if track.stream_errors > 0 {
        println!("           ^ {} stream error(s)", track.stream_errors);
    }
}

fn list_devices(telemetry: &Telemetry) -> Result<(), Box<dyn std::error::Error>> {
    let devices = match audio::devices::list_devices() {
        Ok(devices) => devices,
        Err(e) => {
            telemetry.track(
                events::DEVICE_LIST_FAILED,
                &[("error_kind", e.kind().into())],
            );
            return Err(e.into());
        }
    };

    telemetry.track(
        events::DEVICES_REFRESHED,
        &[
            ("total", devices.len().into()),
            (
                "input_capable",
                devices
                    .iter()
                    .filter(|(_, i)| i.supports_input)
                    .count()
                    .into(),
            ),
            (
                "loopback_capable",
                devices
                    .iter()
                    .filter(|(_, i)| i.can_loopback())
                    .count()
                    .into(),
            ),
            (
                "has_default_output",
                devices.iter().any(|(_, i)| i.is_default_output).into(),
            ),
        ],
    );

    println!(
        "{:<38} {:<9} {:<5} {:<5} {:<9} FLAGS",
        "NAME", "DIRECTION", "IN", "OUT", "LOOPBACK"
    );
    for (_, info) in &devices {
        let mut flags = Vec::new();
        if info.is_default_input {
            flags.push("default-in");
        }
        if info.is_default_output {
            flags.push("default-out");
        }
        println!(
            "{:<38} {:<9} {:<5} {:<5} {:<9} {}",
            truncate(&info.name, 38),
            format!("{:?}", info.direction),
            info.supports_input,
            info.supports_output,
            if info.can_loopback() { "yes" } else { "NO" },
            flags.join(", ")
        );
    }

    println!("\nids (pass to --mic / --system):");
    for (_, info) in &devices {
        if let Some(id) = &info.id {
            println!("  {:<38} {}", truncate(&info.name, 38), id);
        }
    }

    // The LOOPBACK column is the whole point of this listing: cpal only taps
    // system audio on a device that reports no input support.
    if !devices.iter().any(|(_, i)| i.can_loopback()) {
        println!(
            "\nWARNING: no output-only device found. Every output here also reports \
             an input, so cpal would record a microphone instead of system audio."
        );
    }

    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max - 1).collect::<String>() + "…"
    }
}
