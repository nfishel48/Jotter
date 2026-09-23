//! How a run talks to whoever started it: a person at a terminal, or a program
//! reading `--json`.
//!
//! The two audiences want different things from the same command. A person
//! wants a progress line, a first-run notice and a sentence per result; a
//! program wants exactly one JSON object on stdout and nothing else there, and
//! an error it can branch on without parsing English. Every command builds its
//! result as a serde struct and hands it to [`Output::emit`] with a closure that
//! prints the human rendering, so the two can never report different things.

use std::io::IsTerminal;

use serde::{Deserialize, Serialize};

/// Where this run's output goes and what it may print besides its result.
#[derive(Debug, Clone, Copy)]
pub struct Output {
    json: bool,
}

impl Output {
    pub fn new(json: bool) -> Self {
        Self { json }
    }

    pub fn is_json(self) -> bool {
        self.json
    }

    /// Print the command's result: one JSON line under `--json`, otherwise
    /// whatever `human` prints.
    pub fn emit<T: Serialize>(self, value: &T, human: impl FnOnce(&T)) {
        if self.json {
            print_json(value);
        } else {
            human(value);
        }
    }

    /// Report a failed command in the form this run's reader expects.
    ///
    /// On stdout under `--json`, because that is the stream a program is
    /// parsing and it must not have to read a second one to learn the command
    /// failed; the exit status says so as well.
    pub fn error(self, error: &CliError) {
        if self.json {
            print_json(&ErrorEnvelope { error });
        } else {
            eprintln!("error: {}", error.message);
        }
    }

    /// Whether a person is reading: notices such as the first-run telemetry
    /// line are for them, and are noise — or worse, corruption — to a program
    /// consuming stdout.
    pub fn notices(self) -> bool {
        !self.json && std::io::stdout().is_terminal()
    }

    /// Whether to draw a progress line on stderr. Additionally needs stderr to
    /// be a terminal: the line is redrawn with carriage returns, which in a log
    /// file are not rewrites but ordinary bytes, and a long transcription would
    /// leave one unreadable line a hundred fragments long.
    pub fn progress(self) -> bool {
        self.notices() && std::io::stderr().is_terminal()
    }
}

fn print_json<T: Serialize + ?Sized>(value: &T) {
    // Every type printed here is a plain derived struct with string keys, which
    // serde_json cannot fail to serialise: non-finite floats become `null`
    // rather than an error. A failure would be a programming error in this
    // crate, not something a user could cause or act on.
    let line = serde_json::to_string(value).expect("output types always serialise");
    println!("{line}");
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    error: &'a CliError,
}

/// Why a command failed, as a stable machine-readable name.
///
/// These strings are an interface: an agent driving `jotter --json` branches on
/// them, and a background recorder writes them into the session file for the
/// `jotter start` that launched it to read back. Add variants freely; never
/// rename one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The command line did not parse.
    Usage,
    /// It parsed, but a value was not acceptable: a speaker count of zero, an
    /// unknown settings key, `record` with nobody to press Enter.
    InvalidArgument,
    /// A model id that is not in the catalogue.
    UnknownModel,
    /// The requested audio device does not exist, or there is none at all.
    DeviceNotFound,
    /// The audio device was found but capture could not start or stop — the
    /// usual shape of a denied permission.
    CaptureFailed,
    /// Echo cancellation, transcription or diarization hit an error.
    StageFailed,
    /// A model download failed or did not verify.
    DownloadFailed,
    /// Reading or writing a file failed.
    Io,
    /// `jotter start` while a session is already recording.
    SessionActive,
    /// `jotter stop` while another `jotter stop` is already finishing the
    /// session.
    SessionBusy,
    /// `jotter stop` with no session recording.
    NoSession,
    /// The background recorder exited without starting, or without
    /// finalising its recording.
    SessionFailed,
    /// The background recorder did not report in, or did not stop, in time.
    SessionTimeout,
    /// macOS only: no `Jotter.app` to run the recorder in. See
    /// `session::launch`.
    BundleNotFound,
    /// The recorder process could not be launched at all.
    LaunchFailed,
}

/// A failed command: what kind of failure, and a sentence for a person.
///
/// Also the shape a background recorder leaves in the session file when it
/// fails to start, which is why it round-trips through serde.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliError {
    pub kind: ErrorKind,
    pub message: String,
}

impl CliError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        Self::new(ErrorKind::Io, e.to_string())
    }
}

impl From<jotter::audio::capture::CaptureError> for CliError {
    fn from(e: jotter::audio::capture::CaptureError) -> Self {
        use jotter::audio::capture::CaptureError;

        // Split out because the remedy differs: a missing device is fixed with
        // `jotter devices` and a different id, anything else is a permission
        // or a backend to go and look at.
        let kind = match &e {
            CaptureError::NoSuchDevice(_)
            | CaptureError::NoInputDevice
            | CaptureError::NoOutputDevice => ErrorKind::DeviceNotFound,
            _ => ErrorKind::CaptureFailed,
        };
        Self::new(kind, e.to_string())
    }
}

#[cfg(feature = "aec")]
impl From<jotter::audio::process::ProcessError> for CliError {
    fn from(e: jotter::audio::process::ProcessError) -> Self {
        Self::new(ErrorKind::StageFailed, e.to_string())
    }
}

#[cfg(feature = "transcribe")]
impl From<jotter::audio::transcribe::TranscribeError> for CliError {
    fn from(e: jotter::audio::transcribe::TranscribeError) -> Self {
        Self::new(ErrorKind::StageFailed, e.to_string())
    }
}

#[cfg(feature = "diarize")]
impl From<jotter::audio::diarize::DiarizeError> for CliError {
    fn from(e: jotter::audio::diarize::DiarizeError) -> Self {
        Self::new(ErrorKind::StageFailed, e.to_string())
    }
}

#[cfg(feature = "transcribe")]
impl From<jotter::models::fetch::FetchError> for CliError {
    fn from(e: jotter::models::fetch::FetchError) -> Self {
        Self::new(ErrorKind::DownloadFailed, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The error a failed `--json` command prints is the documented envelope,
    /// and a kind written by one process reads back as the same kind in
    /// another — the recorder-to-`start` hand-off depends on both.
    #[test]
    fn error_envelope_shape_and_round_trip() {
        let error = CliError::new(ErrorKind::SessionActive, "already recording");
        let json = serde_json::to_value(ErrorEnvelope { error: &error }).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "error": { "kind": "session_active", "message": "already recording" }
            })
        );

        let back: CliError = serde_json::from_value(json["error"].clone()).unwrap();
        assert_eq!(back.kind, ErrorKind::SessionActive);
    }
}
