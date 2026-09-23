//! `jotter start`, `jotter status` and `jotter stop`: a recording that runs in
//! the background while the command that started it returns.
//!
//! The recorder is a separate process (`jotter __session-run`, hidden from
//! help), because a recording outlives the command that asked for it and must
//! survive the terminal closing. The two talk through one session file — see
//! [`state`] — which is also how the recorder reports that it started, since on
//! macOS it is launched through LaunchServices and there is no pipe back.

mod launch;
pub(crate) mod recorder;
mod state;

use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use clap::Args;
use serde::Serialize;

use jotter::audio::{self, FinishOptions};
use jotter::config::{self, Settings};
use jotter::telemetry::{Surface, Telemetry, events};

use self::state::{Lookup, Phase, SessionFile, SessionState, StopLock};
use crate::cli::SourcesArg;
use crate::output::{CliError, ErrorKind, Output};
use crate::report::RecordingJson;

/// How long `jotter start` waits for the recorder to report that it is
/// capturing, and how long `jotter stop` waits for it to finish.
///
/// Long on purpose: opening a device can wait on a permission prompt, and a
/// machine under load can take a while to start a process at all.
const START_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(60);

/// How often the launcher checks the session file while it waits.
const POLL: Duration = Duration::from_millis(100);

/// The current moment, as the session file stores it.
pub fn now() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[derive(Args)]
pub struct StartArgs {
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

    /// Output directory (default: ~/Documents/Jotter/<timestamp>)
    #[arg(long, value_name = "DIR")]
    out: Option<PathBuf>,

    /// Do not transcribe while recording. Live transcription is not yet
    /// available, so every session records without it.
    #[arg(long)]
    no_live: bool,
}

#[derive(Args)]
pub struct StopArgs {
    /// Stop the recorder and leave the offline passes for later, rather than
    /// running them before returning.
    #[arg(long)]
    no_finish: bool,
}

/// `jotter start`: launch a recorder and return once it is capturing.
pub fn start(args: StartArgs, out: Output) -> Result<(), CliError> {
    let file = SessionFile::default_location();
    let (pid, exe) = state::current_process()?;

    // Absolute before it is handed to another process: a macOS bundle's
    // working directory is `/`, so a relative path would land there.
    let dir = args
        .out
        .map(absolute)
        .unwrap_or_else(|| config::recordings_root().join(audio::meta::timestamp_dir_name()));

    let session = SessionState {
        session_id: format!(
            "{}-{}",
            Utc::now().format("%Y%m%dT%H%M%S%.3f"),
            std::process::id()
        ),
        state: Phase::Starting,
        pid,
        exe,
        dir,
        started_at: now(),
        sources: args.only,
        mic: args.mic,
        system: args.system,
        // Always off until the library grows live transcription; the flag is
        // accepted now so a script written against it does not break then.
        live: false,
        log: state::log_path(),
        error: None,
    };
    let _ = args.no_live;

    file.create(&session)?;
    let child = match launch::launch(file.path(), &session.session_id, &session.log) {
        Ok(child) => child,
        Err(e) => {
            file.remove(&session.session_id)?;
            return Err(e);
        }
    };

    let started = match wait_for_recording(&file, &session, child) {
        Ok(started) => started,
        Err(e) => {
            // The recorder did not start, so the session must not linger to
            // block the next one.
            file.remove(&session.session_id)?;
            return Err(e);
        }
    };

    let result = StartJson {
        session_id: started.session_id,
        dir: started.dir,
        pid: started.pid,
        started_at: started.started_at,
        live: started.live,
    };
    out.emit(&result, |r| {
        println!("recording to {}", r.dir.display());
        println!("  session  {}", r.session_id);
        println!("  recorder pid {}", r.pid);
        println!("\n`jotter stop` ends it");
    });
    Ok(())
}

/// A path another process can open, even when its working directory is `/`.
fn absolute(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or_else(|_| {
        std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .unwrap_or(path)
    })
}

/// Wait until the recorder reports it is capturing, or explain why it is not.
///
/// `child` is the recorder when it was spawned directly, so an early exit is
/// visible at once; through LaunchServices there is no child to watch and the
/// session file is the only signal.
fn wait_for_recording(
    file: &SessionFile,
    session: &SessionState,
    mut child: Option<Child>,
) -> Result<SessionState, CliError> {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Some(state) = file.read_session(&session.session_id)? {
            match state.state {
                Phase::Starting => {}
                Phase::Failed => return Err(state.error.unwrap_or_else(unexplained_failure)),
                // Anything past starting means capture began, even if a stop
                // already followed it.
                _ => return Ok(state),
            }
        }

        if let Some(child) = &mut child
            && let Some(status) = child.try_wait()?
        {
            // One last read: it may have written its failure just before
            // exiting, which is a better explanation than the exit code.
            if let Some(state) = file.read_session(&session.session_id)?
                && state.state == Phase::Failed
            {
                return Err(state.error.unwrap_or_else(unexplained_failure));
            }
            return Err(died_before_starting(&session.log, status));
        }

        if Instant::now() >= deadline {
            return Err(timed_out(file, session));
        }
        std::thread::sleep(POLL);
    }
}

fn unexplained_failure() -> CliError {
    CliError::new(ErrorKind::SessionFailed, "the recorder failed to start")
}

fn died_before_starting(log: &Path, status: ExitStatus) -> CliError {
    CliError::new(
        ErrorKind::SessionFailed,
        format!(
            "the recorder exited before it started recording ({status}); see {}",
            log.display()
        ),
    )
}

/// The recorder never reported in. Stop it if it is ours, so a slow start does
/// not leave a process recording into a session nobody is tracking.
fn timed_out(file: &SessionFile, session: &SessionState) -> CliError {
    if let Ok(Some(state)) = file.read_session(&session.session_id) {
        state::terminate(&state);
    }
    CliError::new(
        ErrorKind::SessionTimeout,
        format!(
            "the recorder did not start within {}s; see {}",
            START_TIMEOUT.as_secs(),
            session.log.display()
        ),
    )
}

#[derive(Serialize)]
struct StartJson {
    session_id: String,
    dir: PathBuf,
    pid: u32,
    started_at: String,
    live: bool,
}

/// `jotter status`.
pub fn status(out: Output) -> Result<(), CliError> {
    let file = SessionFile::default_location();
    let result = match file.lookup()? {
        Lookup::Idle => StatusJson {
            active: false,
            session: None,
            stale: None,
        },
        Lookup::Active(s) => StatusJson {
            active: true,
            session: Some(SessionJson::new(&s)),
            stale: None,
        },
        Lookup::Stale(s) => StatusJson {
            active: false,
            session: None,
            stale: Some(StaleJson::new(&s)),
        },
    };

    out.emit(&result, |r| match (&r.session, &r.stale) {
        (Some(s), _) => {
            println!("recording into {}", s.dir.display());
            println!(
                "  {} since {}, {:.0}s elapsed",
                s.state, s.started_at, s.elapsed_secs
            );
            println!("  session {} (pid {})", s.session_id, s.pid);
        }
        (_, Some(s)) => println!(
            "no session running; the last one ({}, pid {}) died without stopping and is stale",
            s.session_id, s.pid
        ),
        _ => println!("no session running"),
    });
    Ok(())
}

#[derive(Serialize)]
struct StatusJson {
    active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<SessionJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stale: Option<StaleJson>,
}

#[derive(Serialize)]
struct SessionJson {
    session_id: String,
    dir: PathBuf,
    pid: u32,
    started_at: String,
    elapsed_secs: f64,
    state: String,
    /// Live transcription status. `null` until a session transcribes as it
    /// records, which none yet does.
    live: Option<()>,
}

impl SessionJson {
    fn new(s: &SessionState) -> Self {
        Self {
            session_id: s.session_id.clone(),
            dir: s.dir.clone(),
            pid: s.pid,
            started_at: s.started_at.clone(),
            elapsed_secs: elapsed(&s.started_at),
            state: s.state.as_str().to_string(),
            live: None,
        }
    }
}

#[derive(Serialize)]
struct StaleJson {
    session_id: String,
    dir: PathBuf,
    pid: u32,
    state: String,
}

impl StaleJson {
    fn new(s: &SessionState) -> Self {
        Self {
            session_id: s.session_id.clone(),
            dir: s.dir.clone(),
            pid: s.pid,
            state: s.state.as_str().to_string(),
        }
    }
}

fn elapsed(started_at: &str) -> f64 {
    DateTime::parse_from_rfc3339(started_at)
        .map(|t| (Utc::now() - t.with_timezone(&Utc)).num_milliseconds() as f64 / 1_000.0)
        .unwrap_or(0.0)
        .max(0.0)
}

/// `jotter stop`: end the recording, then run the offline passes.
pub fn stop(args: StopArgs, out: Output) -> Result<(), CliError> {
    // Held for the whole command, so a second `jotter stop` cannot start
    // finishing the same recording while this one is still on it.
    let _lock = StopLock::try_acquire()?.ok_or_else(|| {
        CliError::new(
            ErrorKind::SessionBusy,
            "another `jotter stop` is already finishing this session",
        )
    })?;

    let file = SessionFile::default_location();
    let session = match file.lookup()? {
        Lookup::Active(s) => s,
        Lookup::Stale(s) => {
            file.remove(&s.session_id)?;
            return Err(stale_session(&s));
        }
        Lookup::Idle => return Err(no_session()),
    };

    let session = match session.state {
        Phase::Starting => wait_for_recording(&file, &session, None)?,
        Phase::Failed => {
            file.remove(&session.session_id)?;
            return Err(session.error.unwrap_or_else(unexplained_failure));
        }
        _ => session,
    };

    if matches!(session.state, Phase::Recording | Phase::Stopping) {
        file.update(&session.session_id, |s| s.state = Phase::Stopping)?;
        state::terminate(&session);
        wait_until_stopped(&file, &session)?;
    }

    finish_session(&file, &session, args.no_finish, out)
}

fn no_session() -> CliError {
    CliError::new(ErrorKind::NoSession, "no session is recording")
}

fn stale_session(s: &SessionState) -> CliError {
    let detail = if matches!(s.state, Phase::Stopped | Phase::Finishing) {
        "its recording was saved, but the offline passes did not finish — \
         `jotter transcribe` and `jotter diarize` can redo them"
    } else {
        "its recording may be incomplete"
    };
    CliError::new(
        ErrorKind::NoSession,
        format!(
            "no session is recording; the last one died without stopping ({detail}), into {}",
            s.dir.display()
        ),
    )
}

/// Wait until the recorder reports it has finalised the tracks.
fn wait_until_stopped(file: &SessionFile, session: &SessionState) -> Result<(), CliError> {
    let deadline = Instant::now() + STOP_TIMEOUT;
    loop {
        let Some(state) = file.read_session(&session.session_id)? else {
            return Err(CliError::new(
                ErrorKind::SessionFailed,
                "the session disappeared while stopping",
            ));
        };
        match state.state {
            Phase::Stopped => return Ok(()),
            Phase::Failed => {
                file.remove(&session.session_id)?;
                return Err(state.error.unwrap_or_else(|| {
                    CliError::new(ErrorKind::SessionFailed, "the recorder failed to stop")
                }));
            }
            _ if !state.is_alive() => {
                file.remove(&session.session_id)?;
                return Err(CliError::new(
                    ErrorKind::SessionFailed,
                    format!(
                        "the recorder exited without finalising the recording; see {}",
                        session.log.display()
                    ),
                ));
            }
            _ if Instant::now() >= deadline => {
                return Err(CliError::new(
                    ErrorKind::SessionTimeout,
                    format!(
                        "the recorder did not stop within {}s; it is still running as pid {}",
                        STOP_TIMEOUT.as_secs(),
                        state.pid
                    ),
                ));
            }
            _ => std::thread::sleep(POLL),
        }
    }
}

/// Run the offline passes, unless asked not to, and report the recording.
fn finish_session(
    file: &SessionFile,
    session: &SessionState,
    no_finish: bool,
    out: Output,
) -> Result<(), CliError> {
    let meta = audio::meta::Meta::read(&session.dir.join("meta.json"))?;
    let mut result = RecordingJson::new(&session.dir, &meta);

    if !no_finish {
        let (pid, exe) = state::current_process()?;
        file.update(&session.session_id, |s| {
            s.state = Phase::Finishing;
            s.pid = pid;
            s.exe = exe;
        })?;

        // Same reporting as `jotter record`: the passes are the same passes,
        // and should be counted the same way.
        let mut settings = Settings::load();
        let telemetry = Telemetry::init(Surface::Cli, &mut settings);
        let report = crate::cli::finish_recording(
            &session.dir,
            &FinishOptions::from_settings(&settings),
            out,
        );
        crate::cli::track_finish(&telemetry, &report);
        telemetry.track(events::APP_EXITED, &[("reason", "cli_done".into())]);
        telemetry.shutdown();

        result = result.with_finish(&report);
        file.remove(&session.session_id)?;
        out.emit(&result, |_| {
            crate::cli::print_recording(&meta, Some(&report))
        });
        return Ok(());
    }

    file.remove(&session.session_id)?;
    out.emit(&result, |_| crate::cli::print_recording(&meta, None));
    Ok(())
}
