//! The `jotter` command.
//!
//! A thin shell over the `jotter` library: parse the arguments, hand them to
//! `cli::run`, and turn an error into a message and an exit status. Everything
//! that records or processes audio lives in the library, so that a program
//! embedding it gets the same behaviour this command has.

mod cli;

use std::process::ExitCode;

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "jotter",
    version,
    about = "Capture a meeting to two WAV tracks, then clean, transcribe and label it offline",
    // A bare `jotter` has nothing sensible to do, and saying what it can do is
    // more use than an error naming a missing subcommand.
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: cli::Command,
}

fn main() -> ExitCode {
    // Finder and LaunchServices may append a process-serial-number argument
    // (`-psn_0_12345`) when opening the macOS bundle, which is how this binary
    // gets its audio permissions. clap would reject it as unknown and the run
    // would die with the error only on a stderr nobody sees, so drop it before
    // parsing.
    let args = std::env::args().filter(|a| !a.starts_with("-psn_"));

    match cli::run(Cli::parse_from(args).command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
