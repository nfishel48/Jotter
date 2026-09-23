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

/// Which front end is running. Present on every event, so that a new front end
/// arrives as a new value in an existing column rather than as traffic that
/// cannot be told apart from the command line's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    Cli,
}

impl Surface {
    pub fn as_str(self) -> &'static str {
        match self {
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
    #[derive(Clone)]
    pub struct Telemetry {
        /// `None` when nothing will be sent: no API key, which is the default
        /// for any build that is not an official release — a fork or a `cargo
        /// build` sends nothing, with no configuration required — or
        /// telemetry turned off by the user or the environment.
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
        /// Start the telemetry worker, if telemetry is allowed at all.
        ///
        /// Off means off from the first instruction: no thread, no client, and
        /// no install id minted. The choice is fixed for the life of the
        /// handle — a front end is a short-lived process that reads the setting
        /// afresh on its next run, so there is no mid-session opt-out to
        /// honour. When it is on, the id is a random v4 UUID: it identifies an
        /// installation, and is derived from nothing about the machine or the
        /// person.
        pub fn init(surface: Surface, settings: &mut Settings) -> Self {
            let Some(api_key) = api_key() else {
                return Self { inner: None };
            };
            if !settings.telemetry_allowed() {
                return Self { inner: None };
            }

            if settings.install_id.is_none() {
                settings.install_id = Some(uuid::Uuid::new_v4().to_string());
                // Best effort: an unwritable config directory is not a reason to
                // fail to start. The id is simply regenerated next launch.
                let _ = settings.save();
            }
            let Some(distinct_id) = settings.install_id.clone() else {
                return Self { inner: None };
            };

            let state = Arc::new(State {
                api_key,
                distinct_id,
                surface,
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

            // Client construction happens on the worker, so the network never
            // sits between the caller and its first line of output.
            let _ = inner.tx.send(Cmd::Start);

            Self { inner: Some(inner) }
        }

        /// Queue an event. Never blocks; drops silently if the worker is gone.
        pub fn track(&self, name: &'static str, props: &[Prop]) {
            let Some(inner) = &self.inner else { return };

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

            let mut all = inner.context.clone();
            all.extend(props.iter().cloned());
            let _ = inner.tx.send(Cmd::Exception {
                kind,
                detail,
                props: all,
            });
        }

        /// Whether anything will be sent from this handle.
        ///
        /// False with no API key or with telemetry turned off, and always false
        /// without the `telemetry` feature. The first-run notice is keyed on
        /// this, so it appears exactly when there is something to give notice
        /// of.
        pub fn is_active(&self) -> bool {
            self.inner.is_some()
        }

        /// Flush buffered events and stop the worker. Bounded by
        /// [`worker::shutdown_budget_ms`]; safe to call more than once.
        ///
        /// Must be called explicitly on every exit path: `process::exit` runs
        /// no destructors, so `Drop` is not enough.
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
        /// Every exit path calls `shutdown` explicitly, because `process::exit`
        /// runs no destructors at all. This only catches a handle dropped on
        /// some path nobody thought about, and only for the last clone:
        /// shutting the worker down when a temporary copy goes out of scope
        /// would be a memorable bug.
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
        pub fn is_active(&self) -> bool {
            false
        }
        pub fn shutdown(&self) {}
    }
}

#[cfg(not(feature = "telemetry"))]
pub use inert::Telemetry;
