//! Starting the recorder process in the background.
//!
//! Linux: the recorder is this same binary, spawned directly in its own session
//! (`setsid`), so closing the terminal that ran `jotter start` neither sends it
//! SIGHUP nor lets a Ctrl-C meant for something else reach it.
//!
//! macOS: the recorder must run inside `Jotter.app`. The system grants
//! microphone and system-audio access to an app bundle with a stable identity,
//! not to a bare binary, and a recorder spawned from a terminal would have its
//! capture attributed to the terminal — which, for system audio, means a track
//! of digital silence and no error. So `jotter start` asks LaunchServices to
//! start the bundle (`open -n -a Jotter.app --args __session-run …`) and learns
//! how it went from the session file. The bundle is looked for, in order:
//!
//! 1. around this binary, when it is itself `Jotter.app/Contents/MacOS/jotter`;
//! 2. at `$JOTTER_APP`;
//! 3. at `build/Jotter.app` in any directory above this binary — where
//!    `scripts/bundle.sh` puts it, so a `cargo build` in the repository finds
//!    the bundle beside it.
//!
//! `JOTTER_NO_BUNDLE=1` skips the bundle and spawns directly, as on Linux. For
//! development and CI only: the microphone then works through the terminal's
//! grant, and the system track records silence.

use std::ffi::OsString;
use std::fs::File;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use crate::output::{CliError, ErrorKind};

/// The hidden subcommand the recorder runs as.
pub const RECORDER_COMMAND: &str = "__session-run";

/// Launch the recorder for the session in `state_file`, returning the child
/// when it was spawned directly and can be watched, or `None` when it was
/// launched through LaunchServices and can only be reached through the session
/// file.
pub fn launch(state_file: &Path, session_id: &str, log: &Path) -> Result<Option<Child>, CliError> {
    let args: [OsString; 5] = [
        RECORDER_COMMAND.into(),
        "--state".into(),
        state_file.into(),
        "--session".into(),
        session_id.into(),
    ];

    #[cfg(target_os = "macos")]
    if !bundle_opted_out() {
        return bundle::open(&args, log).map(|()| None);
    }

    spawn(&args, log).map(Some)
}

fn spawn(args: &[OsString], log: &Path) -> Result<Child, CliError> {
    use std::os::unix::process::CommandExt;

    let launch_failed = |e: std::io::Error| {
        CliError::new(
            ErrorKind::LaunchFailed,
            format!("could not start the recorder: {e}"),
        )
    };

    let exe = std::env::current_exe().map_err(launch_failed)?;
    let out = File::create(log)?;
    let err = out.try_clone()?;
    let mut command = Command::new(exe);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    // SAFETY: `setsid` is async-signal-safe, which is all a `pre_exec` closure
    // may call, and touches no memory.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().map_err(launch_failed)
}

#[cfg(target_os = "macos")]
fn bundle_opted_out() -> bool {
    std::env::var_os("JOTTER_NO_BUNDLE").is_some_and(|v| !v.is_empty() && v != "0")
}

#[cfg(target_os = "macos")]
mod bundle {
    use std::ffi::OsString;
    use std::fs::File;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use crate::output::{CliError, ErrorKind};

    /// The executable inside the bundle; `scripts/bundle.sh` names it.
    const EXEC: &str = "jotter";

    /// Environment the recorder should see. A LaunchServices launch starts
    /// from the login session's environment, not the shell's, so anything the
    /// user exported for Jotter would otherwise be silently dropped — and an
    /// opt-out such as `DO_NOT_TRACK` must never be.
    const FORWARDED_ENV: &[&str] = &[
        "DO_NOT_TRACK",
        "JOTTER_TELEMETRY",
        "JOTTER_MODELS_DIR",
        "RUST_BACKTRACE",
    ];

    pub fn open(args: &[OsString], log: &Path) -> Result<(), CliError> {
        let app = find()?;

        // Truncated here so the log holds only this session. Only stderr goes
        // to it: `open` opens each redirect separately, and two descriptors
        // writing one file from offset zero would overwrite each other.
        File::create(log)?;
        let mut command = Command::new("/usr/bin/open");
        // -n: a new instance even if a Jotter.app is already running — without
        // it LaunchServices hands the arguments to the running one, which
        // drops them. -g: stay in the background; the recorder has no window.
        command.args(["-n", "-g", "-a"]).arg(&app);
        command.arg("--stderr").arg(log);
        for name in FORWARDED_ENV {
            if let Some(value) = std::env::var_os(name) {
                let mut pair = OsString::from(format!("{name}="));
                pair.push(value);
                command.arg("--env").arg(pair);
            }
        }
        command.arg("--args").args(args);

        let output = command.output().map_err(|e| {
            CliError::new(
                ErrorKind::LaunchFailed,
                format!("could not run /usr/bin/open: {e}"),
            )
        })?;
        if !output.status.success() {
            return Err(CliError::new(
                ErrorKind::LaunchFailed,
                format!(
                    "LaunchServices could not start {}: {}",
                    app.display(),
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            ));
        }
        Ok(())
    }

    fn find() -> Result<PathBuf, CliError> {
        let exe = std::env::current_exe()?;
        let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
        let env = std::env::var_os("JOTTER_APP").map(PathBuf::from);
        locate(&exe, env.as_deref()).map_err(|why| {
            CliError::new(
                ErrorKind::BundleNotFound,
                format!(
                    "background recording on macOS runs inside Jotter.app, because macOS \
                     grants microphone and system-audio access to an app bundle, not to a \
                     bare binary — recorded from here, the system track would be silence. \
                     {why} Build one with scripts/bundle.sh, or set JOTTER_APP to an \
                     installed Jotter.app."
                ),
            )
        })
    }

    /// The search order in the module docs, as a pure function of the paths
    /// involved.
    pub(super) fn locate(exe: &Path, env: Option<&Path>) -> Result<PathBuf, String> {
        if let Some(app) = enclosing(exe) {
            return Ok(app);
        }
        if let Some(app) = env {
            // Set but wrong is an error rather than a reason to keep looking:
            // falling through to a different bundle would record through one
            // the user did not choose.
            return if is_bundle(app) {
                Ok(app.to_path_buf())
            } else {
                Err(format!(
                    "JOTTER_APP is {}, which has no Contents/MacOS/{EXEC}.",
                    app.display()
                ))
            };
        }
        exe.ancestors()
            .skip(1)
            .map(|dir| dir.join("build/Jotter.app"))
            .find(|app| is_bundle(app))
            .ok_or_else(|| {
                format!(
                    "{} is not inside a bundle, JOTTER_APP is not set, and there is no \
                     build/Jotter.app in any directory above it.",
                    exe.display()
                )
            })
    }

    /// `X.app` when `exe` is `X.app/Contents/MacOS/<exe>`.
    fn enclosing(exe: &Path) -> Option<PathBuf> {
        let macos = exe.parent()?;
        let contents = macos.parent()?;
        let app = contents.parent()?;
        (macos.file_name()? == "MacOS"
            && contents.file_name()? == "Contents"
            && app.extension()? == "app")
            .then(|| app.to_path_buf())
    }

    fn is_bundle(app: &Path) -> bool {
        app.join("Contents/MacOS").join(EXEC).is_file()
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::bundle::locate;

    fn tree(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("jotter-bundle-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn make_bundle(app: &Path) {
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        fs::write(app.join("Contents/MacOS/jotter"), b"").unwrap();
    }

    #[test]
    fn a_binary_inside_a_bundle_uses_that_bundle_over_everything_else() {
        let root = tree("inside");
        let installed = root.join("Applications/Jotter.app");
        make_bundle(&installed);
        let other = root.join("Other.app");
        make_bundle(&other);

        let exe = installed.join("Contents/MacOS/jotter");
        assert_eq!(locate(&exe, Some(&other)).unwrap(), installed);
    }

    #[test]
    fn jotter_app_wins_over_the_dev_bundle_and_must_be_real() {
        let root = tree("env");
        make_bundle(&root.join("build/Jotter.app"));
        let chosen = root.join("Chosen.app");
        make_bundle(&chosen);
        let exe = root.join("target/debug/jotter");

        assert_eq!(locate(&exe, Some(&chosen)).unwrap(), chosen);
        assert!(locate(&exe, Some(&root.join("Missing.app"))).is_err());
    }

    #[test]
    fn a_cargo_build_finds_the_bundle_script_output_above_it() {
        let root = tree("dev");
        let exe = root.join("target/debug/jotter");
        assert!(locate(&exe, None).is_err());

        make_bundle(&root.join("build/Jotter.app"));
        assert_eq!(locate(&exe, None).unwrap(), root.join("build/Jotter.app"));
    }
}
