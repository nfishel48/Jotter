//! The command-line front end.
//!
//!   jotter devices
//!   jotter record --duration 10
//!   jotter record --system <id> --mic <id> --duration 600
//!
//! This exists to exercise the capture path without the tray app in the way:
//! the two macOS permissions are granted separately, and a terminal session
//! that prints device ids and frame counts is far easier to debug against than
//! a settings pane.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Subcommand, ValueEnum};

use crate::audio::{self, RecordConfig, Sources, devices::DeviceChoice};

#[derive(Subcommand)]
pub enum Command {
    /// Capture mic + system audio to two WAV tracks
    Record(RecordArgs),
    /// List audio devices and show which ones can be tapped for system audio
    #[command(alias = "list")]
    Devices,
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
}

/// Mirrors `audio::Sources` rather than deriving `ValueEnum` on it directly:
/// `audio` compiles in a CLI-free build, and should not depend on clap.
#[derive(Clone, Copy, ValueEnum)]
enum SourcesArg {
    Mic,
    System,
    Both,
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
    match command {
        Command::Devices => list_devices(),
        Command::Record(args) => record(args),
    }
}

fn record(args: RecordArgs) -> Result<(), Box<dyn std::error::Error>> {
    // Relative, and deliberately not the GUI's ~/Documents/Jotter: a debugging
    // run should land next to the checkout, not in with real recordings.
    let out_dir = args
        .out
        .unwrap_or_else(|| PathBuf::from("recordings").join(audio::meta::timestamp_dir_name()));

    let config = RecordConfig {
        sources: args.only.into(),
        mic: args.mic.map_or(DeviceChoice::Default, DeviceChoice::Id),
        system: args.system.map_or(DeviceChoice::Default, DeviceChoice::Id),
        out_dir,
        allow_duplex_system: args.force_system_on_duplex,
    };

    let handle = audio::start(config)?;
    println!("recording to {}", handle.out_dir().display());

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

    let meta = handle.stop()?;

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

    Ok(())
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

fn list_devices() -> Result<(), Box<dyn std::error::Error>> {
    let devices = audio::devices::list_devices()?;

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
