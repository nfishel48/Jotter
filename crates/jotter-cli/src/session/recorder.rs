//! The background recorder: `jotter __session-run`, the process `jotter start`
//! launches and `jotter stop` signals.
//!
//! It holds the capture for the length of the meeting and does nothing else.
//! Stopping is a signal — SIGTERM from `jotter stop`, or SIGINT/SIGHUP from
//! anyone — handled by finishing the recording properly, because a recorder
//! that simply died would leave WAV headers that still claim zero samples. The
//! offline passes are not run here: `jotter stop` runs them itself, so the
//! command that asked for the result is the one that waits for it and reports
//! it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::Args;

use jotter::audio::{self, RecordConfig, devices::DeviceChoice};
use jotter::config::Settings;
use jotter::telemetry::{Surface, Telemetry, events};

use super::state::{self, Phase, SessionFile, SessionState};
use crate::output::{CliError, ErrorKind};

#[derive(Args)]
pub struct RecorderArgs {
    /// The session file to read the configuration from and report through
    #[arg(long, value_name = "PATH")]
    state: PathBuf,

    /// The session this process was launched for
    #[arg(long, value_name = "ID")]
    session: String,
}

/// How often the capture loop looks for a stop request. Short enough that
/// `jotter stop` feels immediate; the loop does nothing else, so the cost is a
/// few wake-ups a second.
const POLL: Duration = Duration::from_millis(100);

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn request_stop(_signal: libc::c_int) {
    // An atomic store is async-signal-safe; nothing else here would be.
    STOP.store(true, Ordering::SeqCst);
}

/// What a background session captures, from its session file.
///
/// The only place a session's `RecordConfig` is built, so what a session
/// records is decided here and nowhere else.
fn record_config(session: &SessionState) -> RecordConfig {
    RecordConfig {
        sources: session.sources.into(),
        mic: session
            .mic
            .clone()
            .map_or(DeviceChoice::Default, DeviceChoice::Id),
        system: session
            .system
            .clone()
            .map_or(DeviceChoice::Default, DeviceChoice::Id),
        out_dir: session.dir.clone(),
        allow_duplex_system: false,
        // Wired in by the context command, not here.
        live: None,
    }
}

pub fn run(args: RecorderArgs) -> Result<(), CliError> {
    // First, before anything slow: a stop that arrives while the devices are
    // still opening must end the recording cleanly, not kill the process.
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        let handler = request_stop as extern "C" fn(libc::c_int);
        // SAFETY: the handler only performs an atomic store, which is
        // async-signal-safe.
        unsafe { libc::signal(signal, handler as libc::sighandler_t) };
    }

    let file = SessionFile::at(args.state);
    let session = file
        .read_session(&args.session)?
        .filter(|s| s.state == Phase::Starting)
        .ok_or_else(|| {
            // `jotter start` gave up waiting, or the file was cleared: there is
            // nobody to record for.
            CliError::new(
                ErrorKind::SessionFailed,
                format!("session {} is not waiting for a recorder", args.session),
            )
        })?;

    // Take the session over from the `jotter start` that launched us: from
    // here on, this process being alive is what makes the session active.
    let (pid, exe) = state::current_process()?;
    let Some(session) = file.update(&session.session_id, |s| {
        s.pid = pid;
        s.exe = exe;
    })?
    else {
        return Err(CliError::new(
            ErrorKind::SessionFailed,
            format!("session {} was abandoned while starting", args.session),
        ));
    };

    // Started here without the first-run notice or `app_started`: this process
    // has no terminal to show a notice on, and the `jotter start` that
    // launched it already counted the invocation.
    let mut settings = Settings::load();
    let telemetry = Telemetry::init(Surface::Cli, &mut settings);
    let result = record(&file, &session, &telemetry);
    telemetry.shutdown();
    result
}

fn record(
    file: &SessionFile,
    session: &SessionState,
    telemetry: &Telemetry,
) -> Result<(), CliError> {
    let handle = match audio::start(record_config(session)) {
        Ok(handle) => handle,
        Err(e) => {
            crate::cli::report_failure(telemetry, "start", &e);
            return Err(fail(file, session, e.into()));
        }
    };
    telemetry.track(
        events::RECORDING_STARTED,
        &[
            ("sources", session.sources.telemetry_name().into()),
            ("mic_is_default", session.mic.is_none().into()),
            ("system_is_default", session.system.is_none().into()),
            ("force_system_on_duplex", false.into()),
            ("fixed_duration", false.into()),
        ],
    );

    let started_at = super::now();
    file.update(&session.session_id, |s| {
        // Only forward: a stop that already arrived has moved the session on,
        // and must not be overwritten back to "recording".
        if s.state == Phase::Starting {
            s.state = Phase::Recording;
            s.started_at = started_at;
        }
    })?;

    while !STOP.load(Ordering::SeqCst) {
        std::thread::sleep(POLL);
    }

    match handle.stop() {
        Ok(meta) => {
            telemetry.track(events::RECORDING_COMPLETED, &events::recording_props(&meta));
            file.update(&session.session_id, |s| s.state = Phase::Stopped)?;
            Ok(())
        }
        Err(e) => {
            crate::cli::report_failure(telemetry, "stop", &e);
            Err(fail(file, session, e.into()))
        }
    }
}

/// Leave `error` in the session file for whoever is waiting on it, and hand it
/// back to be printed into the recorder's log as well.
fn fail(file: &SessionFile, session: &SessionState, error: CliError) -> CliError {
    // Best effort: if even this write fails, the launcher still sees the
    // process exit and points at the log, which will hold the error.
    let _ = file.update(&session.session_id, |s| {
        s.state = Phase::Failed;
        s.error = Some(error.clone());
    });
    error
}
