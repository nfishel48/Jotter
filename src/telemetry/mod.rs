//! Anonymous usage and crash reporting.
//!
//! # Shape
//!
//! [`Telemetry`] is a handle the front ends hold and call freely. Every method
//! is non-blocking and infallible; nothing a call site does can fail because
//! telemetry is off, unconfigured, or offline. That is deliberate — instrumenting
//! a code path should never change its error handling.
//!
//! There are two implementations of that handle, selected by the `telemetry`
//! Cargo feature, with identical signatures: the real one below, and an inert
//! one that does nothing. A build without the feature has no HTTP client and no
//! async runtime linked in at all, which is the only way to *show* rather than
//! assert that a binary sends nothing.
//!
//! # What is sent
//!
//! See `docs/TELEMETRY.md`, which is the user-facing contract. In code, the two
//! rules that keep it true:
//!
//! - Event properties are `&'static str` or numbers. A `&'static str` cannot
//!   hold a device name, a file path, or a username, so the type system does
//!   most of the work. [`crate::audio::capture::CaptureError::kind`] exists for
//!   exactly this reason.
//! - [`scrub`] redacts the home directory from everything on the way out, which
//!   covers the payloads the app does not construct itself — panic messages and
//!   stack frames.

pub mod events;
pub mod scrub;

#[cfg(feature = "telemetry")]
mod worker;

/// One event property. `&'static str` keys keep the vocabulary closed; see the
/// module docs for why the values are constrained in practice too.
pub type Prop = (&'static str, serde_json::Value);

/// Which front end is running. Present on every event, because the tray app and
/// the CLI have almost nothing in common operationally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    Gui,
    Cli,
}

impl Surface {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gui => "gui",
            Self::Cli => "cli",
        }
    }
}

// ---------------------------------------------------------------------------
// Real implementation
// ---------------------------------------------------------------------------

#[cfg(feature = "telemetry")]
mod real {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::worker::{self, Cmd, State};
    use super::{Prop, Surface, events};
    use crate::config::Settings;

    /// Cheap to clone — all clones share one worker.
    ///
    /// Needed because `eframe::run_native` takes ownership of the closure that
    /// builds the app, so a handle has to be kept behind for the "the window
    /// never opened" case.
    #[derive(Clone)]
    pub struct Telemetry {
        /// `None` when there is no API key, which is the default for any build
        /// that is not an official release — a fork or a `cargo build` sends
        /// nothing, with no configuration required.
        inner: Option<Arc<Inner>>,
    }

    struct Inner {
        tx: std::sync::mpsc::Sender<Cmd>,
        state: Arc<State>,
        context: Vec<Prop>,
        /// Makes `shutdown` idempotent.
        ///
        /// Every exit path calls it explicitly *and* `Drop` calls it as a
        /// backstop, so without this the second call queues a second
        /// `Cmd::Shutdown` behind a worker that is already draining and waits out
        /// the whole budget again — doubling the worst-case quit.
        shutdown_sent: AtomicBool,
    }

    impl Telemetry {
        /// Start the telemetry worker.
        ///
        /// Mints and persists `install_id` if absent, so that opting in later in
        /// the session works without a restart. The id is a random v4 UUID: it
        /// identifies an installation, is derived from nothing about the machine
        /// or the person, and never leaves the disk while telemetry is off.
        pub fn init(surface: Surface, settings: &mut Settings) -> Self {
            let Some(api_key) = api_key() else {
                return Self { inner: None };
            };

            if settings.install_id.is_none() {
                settings.install_id = Some(uuid::Uuid::new_v4().to_string());
                // Best effort: an unwritable config directory is not a reason to
                // fail to start. The id is simply regenerated next launch.
                let _ = settings.save();
            }
            let Some(distinct_id) = settings.install_id.clone() else {
                return Self { inner: None };
            };

            let enabled = settings.telemetry_allowed();
            let state = Arc::new(State {
                api_key,
                distinct_id,
                surface,
                enabled: AtomicBool::new(enabled),
                sending: AtomicBool::new(enabled),
                flags: Default::default(),
                home: super::scrub::home(),
            });

            let (tx, rx) = std::sync::mpsc::channel();
            let worker_state = Arc::clone(&state);
            // A named thread so it is identifiable in a sample or a crash log —
            // this is the only thread in the process that does network I/O.
            let spawned = std::thread::Builder::new()
                .name("jotter-telemetry".into())
                .spawn(move || worker::run(rx, worker_state));

            if spawned.is_err() {
                return Self { inner: None };
            }

            let inner = Arc::new(Inner {
                tx,
                state,
                context: events::context(surface),
                shutdown_sent: AtomicBool::new(false),
            });

            // Drives first-time client construction when already opted in.
            let _ = inner.tx.send(Cmd::SetEnabled(enabled));

            Self { inner: Some(inner) }
        }

        /// Queue an event. Never blocks; drops silently if the worker is gone.
        pub fn track(&self, name: &'static str, props: &[Prop]) {
            let Some(inner) = &self.inner else { return };
            if !inner.state.enabled.load(Ordering::Relaxed) {
                return;
            }

            let mut all = inner.context.clone();
            all.extend(props.iter().cloned());
            let _ = inner.tx.send(Cmd::Capture { name, props: all });
        }

        /// Report a handled error to PostHog Error Tracking.
        ///
        /// `kind` and `detail` are `&'static str` on purpose: the interesting
        /// errors in this app carry device names, and a signature that accepts a
        /// `String` is an invitation to pass `err.to_string()`. Use
        /// [`crate::audio::capture::CaptureError::kind`] and `cpal_kind`.
        pub fn report_error(
            &self,
            kind: &'static str,
            detail: Option<&'static str>,
            props: &[Prop],
        ) {
            let Some(inner) = &self.inner else { return };
            if !inner.state.enabled.load(Ordering::Relaxed) {
                return;
            }

            let mut all = inner.context.clone();
            all.extend(props.iter().cloned());
            let _ = inner.tx.send(Cmd::Exception {
                kind,
                detail,
                props: all,
            });
        }

        /// Turn collection on or off for the rest of the session.
        ///
        /// The caller is responsible for persisting the choice; this only moves
        /// the runtime switch.
        pub fn set_enabled(&self, on: bool) {
            let Some(inner) = &self.inner else { return };
            // Store eagerly so `track` starts dropping immediately, rather than
            // whenever the worker gets round to the message.
            inner.state.enabled.store(on, Ordering::Relaxed);
            let _ = inner.tx.send(Cmd::SetEnabled(on));
        }

        /// Whether this build is capable of collecting anything.
        ///
        /// Deliberately independent of whether it currently *is*: the settings
        /// checkbox is drawn from this, and keying it on the live state would
        /// disable the control the moment someone opted out, leaving them no way
        /// back in. False means no `telemetry` feature or no API key.
        pub fn is_configured(&self) -> bool {
            self.inner.is_some()
        }

        /// Read a feature flag evaluated at startup.
        ///
        /// Remote evaluation only — a flag the app has never heard back about is
        /// `false`. Local evaluation would need a personal API key, and there is
        /// nowhere to put one in an open-source binary.
        pub fn flag(&self, key: &str) -> bool {
            let Some(inner) = &self.inner else {
                return false;
            };
            inner
                .state
                .flags
                .read()
                .ok()
                .and_then(|f| f.as_ref().map(|f| f.is_enabled(key)))
                .unwrap_or(false)
        }

        /// Flush buffered events and stop the worker. Bounded by
        /// [`worker::shutdown_budget_ms`]; safe to call more than once.
        ///
        /// Must be called explicitly on every exit path. The tray's Quit calls
        /// `process::exit`, which runs no destructors, so `Drop` is not enough.
        pub fn shutdown(&self) {
            let Some(inner) = &self.inner else { return };
            if inner.shutdown_sent.swap(true, Ordering::SeqCst) {
                return;
            }

            // A little longer than the SDK's own drain budget, so the wait ends
            // because the worker finished rather than because we gave up first —
            // which would leave it draining into a process that is exiting.
            let grace =
                Duration::from_millis(worker::shutdown_budget_ms(inner.state.surface) + 250);

            let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel(0);
            if inner.tx.send(Cmd::Shutdown(ack_tx)).is_err() {
                return;
            }
            let _ = ack_rx.recv_timeout(grace);
        }
    }

    impl Drop for Telemetry {
        /// A backstop, not the mechanism.
        ///
        /// Every exit path calls `shutdown` explicitly, because the one that
        /// matters most — the tray's Quit — goes through `process::exit` and
        /// runs no destructors at all. This only catches a handle dropped on
        /// some path nobody thought about, and only for the last clone: shutting
        /// the worker down when a temporary copy goes out of scope would be a
        /// memorable bug.
        fn drop(&mut self) {
            let last = self
                .inner
                .as_ref()
                .is_some_and(|inner| Arc::strong_count(inner) == 1);
            if last {
                self.shutdown();
            }
        }
    }

    /// The PostHog project token.
    ///
    /// Baked in at build time by CI and overridable at runtime for development.
    /// A project token is write-only and public by design — it is in the page
    /// source of every site running PostHog — so shipping it in an open-source
    /// binary gives away nothing. `None` (an unofficial build) disables
    /// telemetry entirely.
    fn api_key() -> Option<String> {
        if let Ok(key) = std::env::var("JOTTER_POSTHOG_KEY")
            && !key.trim().is_empty()
        {
            return Some(key);
        }
        option_env!("JOTTER_POSTHOG_KEY")
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .map(str::to_owned)
    }
}

#[cfg(feature = "telemetry")]
pub use real::Telemetry;

// ---------------------------------------------------------------------------
// Inert implementation
// ---------------------------------------------------------------------------

/// The `--no-default-features` stand-in for [`Telemetry`].
///
/// Signatures must match the real one exactly, so that no call site needs a
/// `cfg`. Kept next to it rather than in its own file for the same reason: the
/// two drift apart the moment they are not read together.
#[cfg(not(feature = "telemetry"))]
mod inert {
    use super::{Prop, Surface};
    use crate::config::Settings;

    #[derive(Clone)]
    pub struct Telemetry;

    impl Telemetry {
        pub fn init(_surface: Surface, _settings: &mut Settings) -> Self {
            Self
        }
        pub fn track(&self, _name: &'static str, _props: &[Prop]) {}
        pub fn report_error(
            &self,
            _kind: &'static str,
            _detail: Option<&'static str>,
            _props: &[Prop],
        ) {
        }
        pub fn set_enabled(&self, _on: bool) {}
        pub fn is_configured(&self) -> bool {
            false
        }
        pub fn flag(&self, _key: &str) -> bool {
            false
        }
        pub fn shutdown(&self) {}
    }
}

#[cfg(not(feature = "telemetry"))]
pub use inert::Telemetry;
