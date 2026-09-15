//! The one entry point.
//!
//! With no subcommand `jotter` launches the tray app; `jotter record` and
//! `jotter devices` are the command-line front end. Bare-invocation-is-the-GUI
//! is not a style choice: LaunchServices starts a .app with no arguments, and
//! the bundle is the only way macOS grants the process audio permissions.
//!
//! Which halves exist is a build-time choice — see the `gui` and `cli` features
//! in Cargo.toml.

#[cfg(not(any(feature = "gui", feature = "cli")))]
compile_error!("jotter needs at least one of the `gui` and `cli` features enabled");

use std::process::ExitCode;

fn main() -> ExitCode {
    match dispatch() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "cli")]
use clap::Parser;

#[cfg(feature = "cli")]
#[derive(Parser)]
#[command(
    name = "jotter",
    version,
    about = "Capture a meeting to two WAV tracks",
    long_about = "Capture a meeting to two WAV tracks.\n\nRun with no subcommand to open the tray app."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<jotter::cli::Command>,
}

#[cfg(feature = "cli")]
fn dispatch() -> Result<(), Box<dyn std::error::Error>> {
    // Finder and LaunchServices may append a process-serial-number argument
    // (`-psn_0_12345`) when opening a bundle. clap would reject it as unknown
    // and the app would die on double-click with the error only on a stderr
    // nobody sees, so drop it before parsing.
    let args = std::env::args().filter(|a| !a.starts_with("-psn_"));

    match Cli::parse_from(args).command {
        Some(command) => jotter::cli::run(command),
        None => launch_gui(),
    }
}

#[cfg(not(feature = "cli"))]
fn dispatch() -> Result<(), Box<dyn std::error::Error>> {
    // No parser in a GUI-only build, so any arguments are simply ignored.
    jotter::ui::run()
}

#[cfg(all(feature = "cli", feature = "gui"))]
fn launch_gui() -> Result<(), Box<dyn std::error::Error>> {
    jotter::ui::run()
}

#[cfg(all(feature = "cli", not(feature = "gui")))]
fn launch_gui() -> Result<(), Box<dyn std::error::Error>> {
    use clap::CommandFactory;

    // Built without the tray app, so there is nothing to fall back to — show
    // the subcommands rather than exiting silently.
    Cli::command().print_help()?;
    Err("this build has no GUI (built without the `gui` feature)".into())
}
