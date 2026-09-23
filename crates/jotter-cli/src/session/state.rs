//! The session file: which background recording is running, and how far along
//! it is.
//!
//! One small JSON file in a per-user directory, shared by three processes that
//! never talk to each other directly — the `jotter start` that launches a
//! recorder, the recorder itself, and the `jotter stop` that ends it. On macOS
//! the recorder is launched through LaunchServices, so there is no pipe back to
//! the launcher and no child to wait on; the file is the only channel there is,
//! and using it on Linux too keeps the two platforms on one protocol.
//!
//! Every write goes to a temporary sibling and is renamed into place, so a
//! reader never sees half a file. Creating the file is exclusive (a hard link,
//! which fails if the name exists), which is what makes "one session at a time"
//! hold even when two `jotter start`s race.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cli::SourcesArg;
use crate::output::{CliError, ErrorKind};

const FILE_NAME: &str = "session.json";
const LOG_NAME: &str = "session.log";
const STOP_LOCK_NAME: &str = "stop.lock";

/// The per-user directory holding the session file, the recorder's log and the
/// stop lock.
///
/// macOS: beside `settings.json` in Application Support, which is not
/// TCC-gated, so neither the launching terminal nor the bundle needs a grant to
/// use it. Linux: `$XDG_RUNTIME_DIR`, a tmpfs cleared at reboot — exactly the
/// lifetime of a pid, so a session file can never outlive the boot its pid
/// belongs to — falling back to `$XDG_STATE_HOME` where there is no runtime
/// directory (containers, some SSH logins).
pub fn dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        let mut dir = jotter::config::path();
        dir.pop();
        dir
    } else {
        let absolute = |name| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
        };
        absolute("XDG_RUNTIME_DIR")
            .or_else(|| absolute("XDG_STATE_HOME"))
            .unwrap_or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(std::env::temp_dir)
                    .join(".local/state")
            })
            .join("jotter")
    }
}

/// Where the recorder's stdout and stderr go. It prints nothing in normal
/// running — failures reach the launcher through the session file — so this
/// holds only what nobody planned for: a panic, a backend's own complaint.
pub fn log_path() -> PathBuf {
    dir().join(LOG_NAME)
}

/// Held by a `jotter stop` for as long as it runs.
///
/// An OS file lock rather than a field in the session file, because the lock
/// dies with its holder: a `jotter stop` interrupted halfway through a long
/// transcription leaves nothing behind that a later one has to second-guess.
pub struct StopLock(#[expect(dead_code, reason = "held for its Drop")] fs::File);

impl StopLock {
    /// `None` when another `jotter stop` holds it.
    pub fn try_acquire() -> Result<Option<Self>, CliError> {
        let dir = dir();
        fs::create_dir_all(&dir)?;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(STOP_LOCK_NAME))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self(file))),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(e)) => Err(e.into()),
        }
    }
}

/// How far along a session is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// `jotter start` has claimed the session and is launching the recorder.
    Starting,
    /// The recorder is capturing.
    Recording,
    /// `jotter stop` has asked the recorder to stop.
    Stopping,
    /// The recorder finalised the tracks and `meta.json`, and is exiting.
    Stopped,
    /// `jotter stop` is running the offline passes over the recording.
    Finishing,
    /// The recorder could not start or stop; `error` says why.
    Failed,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Recording => "recording",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Finishing => "finishing",
            Self::Failed => "failed",
        }
    }
}

/// The session file's contents.
///
/// Also the recorder's whole configuration: it is launched with nothing but
/// the path to this file and the session id, and reads what to record from
/// here. That keeps the LaunchServices command line trivial, and means the
/// recording `jotter status` describes is by construction the one being made.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub session_id: String,
    pub state: Phase,
    /// The process responsible for the session right now: the `jotter start`
    /// launching it, then the recorder, then the `jotter stop` finishing it.
    /// Liveness of this process is what separates an active session from a
    /// stale one.
    pub pid: u32,
    /// `pid`'s executable. Checked alongside the pid so that a pid the OS has
    /// since handed to some other program — after a reboot, say — is neither
    /// reported as a live session nor sent a signal.
    pub exe: PathBuf,
    pub dir: PathBuf,
    /// RFC 3339. The moment capture started, once it has; before that, the
    /// moment `jotter start` ran.
    pub started_at: String,
    pub sources: SourcesArg,
    pub mic: Option<String>,
    pub system: Option<String>,
    /// Whether the recorder transcribes as it records.
    pub live: bool,
    pub log: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<CliError>,
}

impl SessionState {
    /// Whether the process responsible for this session still exists.
    pub fn is_alive(&self) -> bool {
        process_matches(self.pid, &self.exe)
    }
}

/// What the session file says, judged against the process table.
pub enum Lookup {
    Idle,
    Active(SessionState),
    /// A session whose responsible process is gone without cleaning up: a
    /// crash, a `kill -9`, a reboot.
    Stale(SessionState),
}

/// The session file at one path. [`SessionFile::default_location`] everywhere
/// but tests.
pub struct SessionFile {
    path: PathBuf,
}

impl SessionFile {
    pub fn default_location() -> Self {
        Self::at(dir().join(FILE_NAME))
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The current contents, or `None` when there is no session.
    pub fn read(&self) -> Result<Option<SessionState>, CliError> {
        let raw = match fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        // Not treated as "no session": writes are atomic, so an unparsable
        // file was put there by something other than this program, and
        // quietly replacing it could hide a recorder that is still running.
        serde_json::from_str(&raw).map(Some).map_err(|e| {
            CliError::new(
                ErrorKind::Io,
                format!(
                    "the session file {} is unreadable ({e}); delete it if no recording is running",
                    self.path.display()
                ),
            )
        })
    }

    /// The current contents, or `None` when the file is missing or belongs to
    /// a different session.
    pub fn read_session(&self, session_id: &str) -> Result<Option<SessionState>, CliError> {
        Ok(self.read()?.filter(|s| s.session_id == session_id))
    }

    pub fn lookup(&self) -> Result<Lookup, CliError> {
        Ok(match self.read()? {
            None => Lookup::Idle,
            Some(s) if s.is_alive() => Lookup::Active(s),
            Some(s) => Lookup::Stale(s),
        })
    }

    /// Claim the session slot for `state`.
    ///
    /// Fails with `session_active` while a live session holds it. A stale one
    /// is cleared first — its process is gone, so nothing will ever clean it
    /// up otherwise.
    pub fn create(&self, state: &SessionState) -> Result<(), CliError> {
        let tmp = self.write_tmp(state)?;
        let result = self.link_exclusive(&tmp);
        let _ = fs::remove_file(&tmp);
        result
    }

    fn link_exclusive(&self, tmp: &Path) -> Result<(), CliError> {
        // Two passes at most: the first may find a stale file and clear it.
        for _ in 0..2 {
            match fs::hard_link(tmp, &self.path) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
            match self.lookup()? {
                Lookup::Active(s) => {
                    return Err(CliError::new(
                        ErrorKind::SessionActive,
                        format!(
                            "a session is already {} into {} (pid {}); `jotter stop` ends it",
                            s.state.as_str(),
                            s.dir.display(),
                            s.pid
                        ),
                    ));
                }
                Lookup::Stale(s) => self.remove(&s.session_id)?,
                Lookup::Idle => {}
            }
        }
        Err(CliError::new(
            ErrorKind::SessionActive,
            "another `jotter start` claimed the session at the same moment",
        ))
    }

    /// Change the session in place, if the file still holds `session_id`.
    /// Returns the state as written, or `None` when the session is gone — in
    /// which case nothing was written, so a recorder that outlived its session
    /// can never resurrect it.
    pub fn update(
        &self,
        session_id: &str,
        change: impl FnOnce(&mut SessionState),
    ) -> Result<Option<SessionState>, CliError> {
        let Some(mut state) = self.read_session(session_id)? else {
            return Ok(None);
        };
        change(&mut state);
        let tmp = self.write_tmp(&state)?;
        fs::rename(&tmp, &self.path)?;
        Ok(Some(state))
    }

    /// Delete the file if it still holds `session_id`.
    pub fn remove(&self, session_id: &str) -> Result<(), CliError> {
        if self.read_session(session_id)?.is_none() {
            return Ok(());
        }
        match fs::remove_file(&self.path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// Write `state` beside the real file, under a name no other process will
    /// use, ready to be linked or renamed into place.
    fn write_tmp(&self, state: &SessionState) -> Result<PathBuf, CliError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut name = OsString::from(".");
        name.push(self.path.file_name().unwrap_or_default());
        name.push(format!(".{}.tmp", std::process::id()));
        let tmp = self.path.with_file_name(name);
        let json = serde_json::to_string_pretty(state).map_err(io::Error::other)?;
        fs::write(&tmp, json)?;
        Ok(tmp)
    }
}

/// This process, as a session file names it.
pub fn current_process() -> Result<(u32, PathBuf), CliError> {
    Ok((std::process::id(), canonical(std::env::current_exe()?)))
}

/// Ask the process responsible for a session to stop, if it is still that
/// process. Returns whether a signal was sent.
pub fn terminate(state: &SessionState) -> bool {
    if !state.is_alive() {
        return false;
    }
    let Ok(pid) = libc::pid_t::try_from(state.pid) else {
        return false;
    };
    // SAFETY: `kill` has no memory-safety preconditions. `pid` is positive —
    // `process_matches` rejects 0, which would signal our own process group.
    unsafe { libc::kill(pid, libc::SIGTERM) == 0 }
}

/// Whether `pid` exists, belongs to this user, and is running `exe`.
fn process_matches(pid: u32, exe: &Path) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 sends nothing; it only reports whether the pid exists
    // and we may signal it. EPERM — it exists, but is another user's — counts
    // as not ours: every process in a session runs as the user who started it.
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    // Unreadable is taken as a match: the pid is ours and alive, and wrongly
    // calling a live recorder stale would be the worse mistake.
    process_exe(pid).is_none_or(|actual| canonical(actual) == exe)
}

#[cfg(target_os = "macos")]
fn process_exe(pid: libc::pid_t) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer is valid for writes of the length passed.
    let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    let len = usize::try_from(len).ok().filter(|&n| n > 0)?;
    buf.truncate(len);
    Some(PathBuf::from(OsString::from_vec(buf)))
}

#[cfg(not(target_os = "macos"))]
fn process_exe(pid: libc::pid_t) -> Option<PathBuf> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let link = fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    // A binary replaced on disk while it runs — a rebuild during a session —
    // reads back with this suffix, and is still the same program.
    let bytes = link.as_os_str().as_bytes();
    let bytes = bytes.strip_suffix(b" (deleted)").unwrap_or(bytes);
    Some(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

fn canonical(path: PathBuf) -> PathBuf {
    fs::canonicalize(&path).unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> SessionFile {
        let dir =
            std::env::temp_dir().join(format!("jotter-session-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        SessionFile::at(dir.join(FILE_NAME))
    }

    fn session(id: &str, pid: u32, exe: PathBuf) -> SessionState {
        SessionState {
            session_id: id.to_string(),
            state: Phase::Recording,
            pid,
            exe,
            dir: PathBuf::from("/tmp/rec"),
            started_at: "2026-01-01T00:00:00Z".to_string(),
            sources: SourcesArg::Both,
            mic: None,
            system: None,
            live: false,
            log: PathBuf::from("/tmp/session.log"),
            error: None,
        }
    }

    /// A pid that certainly belonged to a process of ours and certainly no
    /// longer does.
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[test]
    fn a_live_session_blocks_a_second_start() {
        let file = scratch("live");
        let (pid, exe) = current_process().unwrap();
        file.create(&session("a", pid, exe.clone())).unwrap();

        let err = file.create(&session("b", pid, exe)).unwrap_err();
        assert_eq!(err.kind, ErrorKind::SessionActive);
        assert_eq!(file.read().unwrap().unwrap().session_id, "a");
    }

    #[test]
    fn a_stale_session_is_reported_and_then_replaced() {
        let file = scratch("stale");
        let (_, exe) = current_process().unwrap();
        file.create(&session("dead", dead_pid(), exe.clone()))
            .unwrap();
        assert!(matches!(file.lookup().unwrap(), Lookup::Stale(s) if s.session_id == "dead"));

        file.create(&session("fresh", std::process::id(), exe))
            .unwrap();
        assert!(matches!(file.lookup().unwrap(), Lookup::Active(s) if s.session_id == "fresh"));
    }

    /// A live pid running a different program is someone else's process that
    /// inherited a recycled pid, not our recorder.
    #[test]
    fn a_reused_pid_is_not_mistaken_for_the_recorder() {
        let file = scratch("reused");
        file.create(&session(
            "old",
            std::process::id(),
            PathBuf::from("/nonexistent/jotter"),
        ))
        .unwrap();
        assert!(matches!(file.lookup().unwrap(), Lookup::Stale(_)));
    }

    /// The recorder must not be able to write into a session that has been
    /// abandoned or replaced while it was starting.
    #[test]
    fn updates_and_removals_only_touch_their_own_session() {
        let file = scratch("owned");
        let (pid, exe) = current_process().unwrap();
        file.create(&session("mine", pid, exe)).unwrap();

        let changed = file.update("other", |s| s.state = Phase::Failed).unwrap();
        assert!(changed.is_none());
        file.remove("other").unwrap();
        assert_eq!(
            file.read().unwrap().unwrap().state,
            Phase::Recording,
            "another session's writes must not land"
        );

        let changed = file.update("mine", |s| s.state = Phase::Stopping).unwrap();
        assert_eq!(changed.unwrap().state, Phase::Stopping);
        file.remove("mine").unwrap();
        assert!(matches!(file.lookup().unwrap(), Lookup::Idle));
    }
}
