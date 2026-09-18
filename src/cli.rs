//! The command-line front end.
//!
//!   jotter devices
//!   jotter record --duration 10
//!   jotter record --system <id> --mic <id> --duration 600
//!   jotter process recordings/<dir>
//!
//! This exists to exercise the capture path without the tray app in the way:
//! the two macOS permissions are granted separately, and a terminal session
//! that prints device ids and frame counts is far easier to debug against than
//! a settings pane.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Subcommand, ValueEnum};

use crate::audio::{self, RecordConfig, Sources, devices::DeviceChoice};
use crate::config::Settings;
use crate::telemetry::{Surface, Telemetry, events};

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

    /// Output directory (default: ./recordings/<timestamp>)
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
    telemetry.track(events::APP_STARTED, &[("is_first_run", false.into())]);

    let result = match command {
        Command::Devices => list_devices(&telemetry),
        Command::Record(args) => record(args, &telemetry),
        #[cfg(feature = "aec")]
        Command::Process(args) => process(args, &telemetry),
        #[cfg(feature = "transcribe")]
        Command::Models(args) => models(args),
        Command::Telemetry(_) => unreachable!("handled above"),
    };

    // Explicit rather than relying on `Drop`: this is the one place a CLI run
    // can lose its whole queue, since the process exits immediately after.
    telemetry.track(events::APP_EXITED, &[("reason", "cli_done".into())]);
    telemetry.shutdown();

    result
}

/// `jotter telemetry [--enable|--disable]`, and with neither, a status report.
///
/// The headless half of the settings-pane checkbox. Someone running the CLI on a
/// server or over SSH should not have to launch a tray app to opt out.
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
    println!("  config:  {}", crate::config::path().display());

    if !cfg!(feature = "telemetry") {
        println!("  note:    this build has telemetry compiled out and sends nothing");
    }

    match crate::config::env_override() {
        crate::config::EnvOverride::ForceOff => {
            println!("  note:    overridden to OFF by DO_NOT_TRACK / JOTTER_TELEMETRY");
        }
        crate::config::EnvOverride::ForceOn => {
            println!("  note:    overridden to ON by JOTTER_TELEMETRY");
        }
        crate::config::EnvOverride::Unset => {}
    }

    println!("\nSee docs/TELEMETRY.md for exactly what is collected.");
    Ok(())
}

fn record(args: RecordArgs, telemetry: &Telemetry) -> Result<(), Box<dyn std::error::Error>> {
    // Relative, and deliberately not the GUI's ~/Documents/Jotter: a debugging
    // run should land next to the checkout, not in with real recordings.
    let out_dir = args
        .out
        .unwrap_or_else(|| PathBuf::from("recordings").join(audio::meta::timestamp_dir_name()));

    // Read before the args are consumed by `RecordConfig`; all three are shapes
    // of the request, not identifiers of a device.
    let sources = args.only.telemetry_name();
    let mic_is_default = args.mic.is_none();
    let system_is_default = args.system.is_none();
    // Read before `args` is consumed. `--aec`/`--no-aec` override the stored
    // setting, so a one-off run can opt in or out without editing the config
    // file — which is the only way to test both paths from a single build.
    #[cfg(feature = "aec")]
    let run_aec = if args.aec {
        true
    } else if args.no_aec {
        false
    } else {
        Settings::load().aec_enabled
    };

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
    // and echo removal is something that then happened to it.
    #[cfg(feature = "aec")]
    if run_aec {
        let both_have_audio = [meta.mic.as_ref(), meta.system.as_ref()]
            .iter()
            .all(|t| t.is_some_and(|t| t.frames > 0));
        if both_have_audio {
            println!();
            let report = audio::process::run(&dir, audio::process::ProcessOptions::default())?;
            report_aec(&report, false);
            telemetry.track(
                events::RECORDING_PROCESSED,
                &events::aec_props(&report, false),
            );
        }
    }

    Ok(())
}

/// Mirror of `ui::App::report_recording_failure`.
///
/// Same rule: `kind` and `cpal_kind`, never `to_string()`. The `Display` impl
/// names the device, which is the one thing that must not be sent.
fn report_failure(telemetry: &Telemetry, phase: &'static str, e: &audio::capture::CaptureError) {
    let props: Vec<crate::telemetry::Prop> = vec![
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

/// `jotter models list | path | pull`.
///
/// Takes no `Telemetry`: which models someone has on disk is a statement about
/// what they transcribe, and there is no aggregate worth that.
#[cfg(feature = "transcribe")]
fn models(args: ModelsArgs) -> Result<(), Box<dyn std::error::Error>> {
    use crate::models;

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
            // No id means "everything transcription needs", which is the
            // recogniser *and* the voice-activity model — they are separate
            // catalogue entries, and a recogniser alone cannot run the stage.
            let wanted: Vec<&'static models::Model> =
                match args.model.as_deref() {
                    Some(id) => vec![models::find(id).ok_or_else(|| {
                        format!("unknown model {id:?} — see `jotter models list`")
                    })?],
                    None => vec![models::DEFAULT_TRANSCRIPTION_MODEL, &models::SILERO_VAD],
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
fn pull_one(model: &'static crate::models::Model) -> Result<(), Box<dyn std::error::Error>> {
    use crate::models::fetch::{self, Progress};
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
