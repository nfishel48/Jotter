//! The `jotter` command.
//!
//! A thin shell over the `jotter` library: parse the arguments, hand them to
//! `cli::run`, and turn an error into a message and an exit status. Everything
//! that records or processes audio lives in the library, so that a program
//! embedding it gets the same behaviour this command has.

mod cli;
mod output;
mod report;
mod session;
mod settings;

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::Parser;

use output::{CliError, ErrorKind, Output};

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
    /// Print exactly one JSON object on stdout instead of text. Errors are
    /// JSON too, on stdout, with a non-zero exit.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: cli::Command,
}

fn main() -> ExitCode {
    // Finder and LaunchServices may append a process-serial-number argument
    // (`-psn_0_12345`) when opening the macOS bundle, which is how this binary
    // gets its audio permissions. clap would reject it as unknown and the run
    // would die with the error only on a stderr nobody sees, so drop it before
    // parsing.
    let args: Vec<String> = std::env::args()
        .filter(|a| !a.starts_with("-psn_"))
        .collect();

    // Known before parsing, because a command line that does not parse still
    // has to fail in the form its reader expects.
    let json = args.iter().any(|a| a == "--json");
    let out = Output::new(json);

    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            if json && e.use_stderr() {
                out.error(&CliError::new(ErrorKind::Usage, e.to_string()));
                return ExitCode::from(e.exit_code() as u8);
            }
            // Help and version are output, not failure, and keep their own
            // formatting.
            let _ = e.print();
            return ExitCode::from(e.exit_code() as u8);
        }
    };

    match cli::run(cli.command, out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            out.error(&e);
            ExitCode::FAILURE
        }
    }
}

/// Whether a prompt could be answered. Shared by every command that would
/// otherwise wait for input: under `--json`, or with stdin closed, there is
/// nobody to answer, and waiting would hang a program driving the command.
pub fn can_prompt() -> bool {
    std::io::stdin().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, FromArgMatches};

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::from_arg_matches(
            &Cli::command()
                .try_get_matches_from(std::iter::once("jotter").chain(args.iter().copied()))?,
        )
    }
    #[test]
    fn json_is_global_and_does_not_disturb_the_subcommand() {
        let cli = parse(&["--json", "status"]).unwrap();
        assert!(cli.json);
        assert!(matches!(cli.command, cli::Command::Status));

        let cli = parse(&["status", "--json"]).unwrap();
        assert!(cli.json);
    }

    #[test]
    fn the_recorder_command_parses_and_stays_out_of_help() {
        let cli = parse(&["__session-run", "--state", "s", "--session", "id"]).unwrap();
        assert!(matches!(cli.command, cli::Command::SessionRun(_)));

        let help = Cli::command().render_help().to_string();
        assert!(
            !help.contains("session-run"),
            "the recorder's command is an implementation detail"
        );
    }
}
