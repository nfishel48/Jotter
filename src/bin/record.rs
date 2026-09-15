//! Capture a meeting to two WAV tracks.
//!
//!   cargo run --bin record -- --list
//!   cargo run --bin record -- --duration 10
//!   cargo run --bin record -- --system <id> --mic <id> --duration 600

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use jotter::audio::{self, RecordConfig, Sources, devices::DeviceChoice};

const USAGE: &str = "\
record — capture mic + system audio to two WAV tracks

USAGE:
    record [OPTIONS]

OPTIONS:
    --list                      List audio devices and exit
    --only <mic|system|both>    Which sources to record (default: both). The two
                                macOS permissions are granted separately, so use
                                this to test them one at a time.
    --mic <id>                  Microphone device id (default: built-in mic)
    --system <id>               Device to tap for system audio (default: default output)
    --duration <secs>           Stop after N seconds (default: until Enter is pressed)
    --out <dir>                 Output directory (default: ./recordings/<timestamp>)
    --force-system-on-duplex    Tap a duplex device anyway. Diagnostic only: cpal
                                will record its microphone, not system audio.
    -h, --help                  Show this help
";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

struct Args {
    list: bool,
    sources: Sources,
    mic: Option<String>,
    system: Option<String>,
    duration: Option<u64>,
    out: Option<PathBuf>,
    force_duplex: bool,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut args = Args {
        list: false,
        sources: Sources::Both,
        mic: None,
        system: None,
        duration: None,
        out: None,
        force_duplex: false,
    };

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} requires a value"));
        match arg.as_str() {
            "--list" => args.list = true,
            "--force-system-on-duplex" => args.force_duplex = true,
            "--only" => {
                let raw = value("--only")?;
                args.sources = match raw.as_str() {
                    "mic" => Sources::MicOnly,
                    "system" => Sources::SystemOnly,
                    "both" => Sources::Both,
                    other => return Err(format!("invalid --only: {other} (mic|system|both)")),
                };
            }
            "--mic" => args.mic = Some(value("--mic")?),
            "--system" => args.system = Some(value("--system")?),
            "--out" => args.out = Some(PathBuf::from(value("--out")?)),
            "--duration" => {
                let raw = value("--duration")?;
                args.duration = Some(
                    raw.parse()
                        .map_err(|_| format!("invalid --duration: {raw}"))?,
                );
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            other => return Err(format!("unknown argument: {other}\n\n{USAGE}")),
        }
    }
    Ok(Some(args))
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let Some(args) = parse_args()? else {
        return Ok(());
    };

    if args.list {
        return list_devices();
    }

    let out_dir = args
        .out
        .unwrap_or_else(|| PathBuf::from("recordings").join(timestamp_dir()));

    let config = RecordConfig {
        sources: args.sources,
        mic: args.mic.map_or(DeviceChoice::Default, DeviceChoice::Id),
        system: args.system.map_or(DeviceChoice::Default, DeviceChoice::Id),
        out_dir,
        allow_duplex_system: args.force_duplex,
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

fn report_track(label: &str, track: &jotter::audio::meta::TrackInfo) {
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

fn timestamp_dir() -> String {
    // Same format the GUI uses. Local time, zero-padded so lexical order
    // matches chronological order.
    chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string()
}
