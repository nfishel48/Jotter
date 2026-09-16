//! The one thread in the process that touches the network.
//!
//! Everything here runs off the UI thread. The front ends only ever push a
//! [`Cmd`] onto a channel, so a slow or unreachable PostHog cannot stall a
//! repaint, and a recording never waits on an HTTP request.
//!
//! The thread owns a small tokio runtime and drives the SDK's futures with
//! `block_on`. Receiving on a blocking channel is what it does most of the time,
//! which is exactly the thing you must not do inside an async task — hence the
//! sync loop with async islands, rather than an async main loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, RwLock};

use posthog_rs::{
    CaptureExceptionOptions, ClientOptionsBuilder, ErrorTrackingOptionsBuilder,
    EvaluateFlagsOptions, Event, FeatureFlagEvaluations,
};

use super::{Prop, Surface, events, scrub};

/// How long teardown may spend draining, per surface.
///
/// This is a ceiling, not a delay: the drain returns as soon as the server
/// responds, which on a healthy network is well under 200ms. It only bites when
/// PostHog is slow or unreachable.
///
/// The tray app is quitting anyway, so two seconds is invisible. A CLI run is a
/// different matter — `jotter devices` prints its table instantly, and making
/// someone watch a finished command sit there while a background thread
/// negotiates TLS is the kind of thing that gets telemetry ripped out of a
/// project. Its ceiling is low enough to stay tolerable in the worst case while
/// still comfortably fitting a real round trip.
pub fn shutdown_budget_ms(surface: Surface) -> u64 {
    match surface {
        Surface::Gui => 2_000,
        Surface::Cli => 1_500,
    }
}

/// Work queued by the front ends.
pub enum Cmd {
    Capture {
        name: &'static str,
        props: Vec<Prop>,
    },
    Exception {
        kind: &'static str,
        detail: Option<&'static str>,
        props: Vec<Prop>,
    },
    SetEnabled(bool),
    /// Drain and stop. The channel acknowledges so the caller can bound its wait.
    Shutdown(SyncSender<()>),
}

/// State shared between the handle and the worker.
pub struct State {
    pub api_key: String,
    pub distinct_id: String,
    pub surface: Surface,
    /// The enqueue gate, flipped by the handle the instant the user acts, so
    /// `track` stops accepting work immediately rather than whenever the worker
    /// gets round to the message.
    ///
    /// `Relaxed` throughout: this gates a best-effort side channel, and nothing
    /// is ordered against it.
    pub enabled: AtomicBool,
    /// The transmit gate, read by `before_send` on the SDK's own worker.
    ///
    /// Separate from `enabled` because of one event: `telemetry_opted_out` is
    /// enqueued *while opting out*, so a single flag would have it dropped by
    /// the very change it reports. The worker lowers this only after flushing
    /// what was already queued — see the `SetEnabled` arm.
    pub sending: AtomicBool,
    pub flags: RwLock<Option<FeatureFlagEvaluations>>,
    pub home: String,
}

pub fn run(rx: Receiver<Cmd>, state: Arc<State>) {
    // One worker thread: this handles a few dozen events per session, and the
    // default (one per core) would be an absurd tax on a tray app.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("jotter-telemetry-io")
        .enable_all()
        .build();

    let Ok(runtime) = runtime else {
        return;
    };

    // Whether the PostHog client exists yet. It is created on the first
    // transition to enabled and never destroyed: `init_global` installs the
    // panic hook and stores the client in a `OnceLock`, so there is exactly one
    // chance to do it. Opting back out is handled by `before_send` dropping
    // every event instead — see `client_options`.
    let mut started = false;

    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::SetEnabled(true) => {
                state.sending.store(true, Ordering::Relaxed);
                if !started {
                    started = runtime.block_on(start(&state));
                }
            }

            Cmd::SetEnabled(false) => {
                // Flush before closing the gate, not after. Anything already
                // queued was enqueued while the user was still opted in — most
                // importantly `telemetry_opted_out` itself, which is enqueued
                // immediately before this message and would otherwise be
                // dropped by the change it is reporting.
                if started {
                    runtime.block_on(posthog_rs::flush());
                }
                state.sending.store(false, Ordering::Relaxed);
            }

            // No gate check here: the handle already checked `enabled` at
            // enqueue time, and the channel is FIFO, so re-checking would only
            // misjudge events queued before a change that has since landed.
            Cmd::Capture { name, props } => {
                if !started {
                    continue;
                }
                posthog_rs::capture(build_event(&state, name, props));
            }

            Cmd::Exception {
                kind,
                detail,
                props,
            } => {
                if !started {
                    continue;
                }
                let error = ReportedError { kind, detail };
                let mut options = CaptureExceptionOptions::new()
                    .distinct_id(state.distinct_id.clone())
                    // Group by our own error kind rather than letting PostHog
                    // infer one from the message: `ReportedError`'s message is
                    // already the kind, but a fingerprint makes that a promise.
                    .fingerprint(kind)
                    .level("error");
                for (key, value) in props {
                    options = options.property(key, value).unwrap_or_else(|_| {
                        CaptureExceptionOptions::new().distinct_id(state.distinct_id.clone())
                    });
                }
                let _ = runtime.block_on(posthog_rs::capture_exception_with(&error, options));
            }

            Cmd::Shutdown(ack) => {
                if started {
                    runtime.block_on(posthog_rs::shutdown());
                }
                // After the drain, so a caller that waits for this knows the
                // queue is empty rather than merely accepted.
                let _ = ack.send(());
                break;
            }
        }
    }
}

/// Bring the client up: evaluate flags, honour the kill switch, then initialize.
///
/// Flags are evaluated *before* `init_global` so that a kill switch can prevent
/// the reporting client from ever being constructed, rather than switching it
/// off after the fact. The flag client is a separate short-lived instance
/// because the SDK exposes flag evaluation on `Client` only, and the global is
/// private — it is shut down again a few lines later.
///
/// Returns whether capture is now possible.
async fn start(state: &Arc<State>) -> bool {
    let Ok(options) = client_options(state) else {
        return false;
    };

    // `init_global` first, and before any network call, because it is what
    // installs the panic hook. Every panic in this app is in startup — tray
    // construction — so the hook is in a race with the thing it exists to catch,
    // and the only lever is to make it win more often. `init_global` itself does
    // no I/O; it just builds the client. Evaluating flags first, as this used to,
    // put a full round trip in front of the hook and lost the panic outright.
    // Measured, not assumed: see docs/TELEMETRY.md.
    if posthog_rs::init_global(options).await.is_err() {
        return false;
    }

    // Flags are a GUI-only concern. The tray app runs for hours, so a round trip
    // at startup costs nothing and the kill switch protects the surface that
    // actually produces volume. A CLI invocation lives for a second or two, and
    // a blocking `/flags` request is most of that.
    if state.surface == Surface::Gui {
        let Ok(flag_options) = client_options(state) else {
            return true;
        };
        // A second client purely because flag evaluation is exposed on `Client`
        // and the global is private. Shut down again a few lines later.
        let flag_client = posthog_rs::client(flag_options).await;
        if let Ok(flags) = flag_client
            .evaluate_flags(state.distinct_id.clone(), EvaluateFlagsOptions::default())
            .await
        {
            if flags.is_enabled(events::FLAG_KILL_SWITCH) {
                // The client now exists — it had to, for the panic hook — so the
                // switch works by closing the transmit gate instead of by never
                // constructing it. Same observable result: `before_send` drops
                // everything, including anything already queued. The stronger
                // "no client at all" property still holds for the opt-out, which
                // never calls this function in the first place.
                state.enabled.store(false, Ordering::Relaxed);
                state.sending.store(false, Ordering::Relaxed);
                flag_client.shutdown().await;
                return false;
            }
            if let Ok(mut slot) = state.flags.write() {
                *slot = Some(flags);
            }
        }
        // A failed evaluation is not fatal: every flag then reads `false`, which
        // is the same answer as "not rolled out to you".
        flag_client.shutdown().await;
    }

    true
}

fn client_options(state: &Arc<State>) -> Result<posthog_rs::ClientOptions, ()> {
    let enabled = Arc::clone(state);
    let home = state.home.clone();

    let error_tracking = ErrorTrackingOptionsBuilder::default()
        .capture_stacktrace(true)
        // Installs a process-wide panic hook. The six `.unwrap()`/`.expect()`
        // calls in tray construction are the app's most likely hard crash, and
        // today they produce nothing but a line on a stderr nobody reads.
        .capture_panics(true)
        // Without this every cpal, eframe and wgpu frame is "in app" and the
        // grouped issue is named after whichever dependency was on top.
        .in_app_include_paths(vec!["jotter".to_string()])
        .build()
        .map_err(|_| ())?;

    ClientOptionsBuilder::default()
        .api_key(state.api_key.clone())
        .host(posthog_rs::US_INGESTION_ENDPOINT)
        // Desktop app, not a server: stops PostHog attributing the host OS to
        // the person, which would make every user look like a Mac datacentre.
        .is_server(false)
        // Verified against a real project: leaving this on does not produce the
        // coarse country signal you might assume. Ingestion stamps
        // `$geoip_city_name`, `$geoip_postal_code` and lat/long — a postal code
        // and coordinates, attached to a stable install id, on every event from
        // an app that records your meetings. There is no country-only setting;
        // the enrichment is all or nothing, and it happens server-side where
        // `before_send` cannot reach it. So: nothing.
        .disable_geoip(true)
        // Sessions are short and sparse; the defaults (100 events / 5s) would
        // mean most sessions never flush before exit.
        .flush_at(20)
        .flush_interval_ms(10_000)
        .max_queue_size(500)
        // Quitting a recorder must not depend on the network being up.
        .shutdown_timeout_ms(shutdown_budget_ms(state.surface))
        .feature_flags_request_timeout_seconds(3)
        .error_tracking(error_tracking)
        .before_send(move |mut event| {
            // The opt-out gate. Runs on the SDK's worker immediately before the
            // HTTP send, so a `false` here means the event is dropped without
            // ever reaching the network — including events already queued when
            // the user unticked the box, and including the panic `$exception`
            // events the SDK's own hook produces without passing through us.
            if !enabled.sending.load(Ordering::Relaxed) {
                return None;
            }
            // Stop ingestion recording the address the event arrived from.
            // `disable_geoip` only suppresses the *derived* location properties;
            // without this the raw `$ip` is still stored on every event, and an
            // IPv6 address is close enough to a household identifier.
            //
            // A placeholder rather than `null`, which was tried first and does
            // not work: ingestion only leaves `$ip` alone when the property is
            // already set, and treats a JSON null as unset, so it refills it
            // from the connection. Verified both ways against a live project.
            //
            // Set here rather than per-event so it also covers the `$exception`
            // events the SDK's panic hook builds without passing through us.
            let _ = event.insert_prop("$ip", "0.0.0.0");
            redact(&mut event, &home);
            Some(event)
        })
        .on_error(|err| {
            // Not fatal, and not the user's problem. Visible in /tmp/jotter.err
            // via scripts/run_app.sh for anyone debugging ingestion.
            eprintln!("telemetry: {err:?}");
        })
        .build()
        .map_err(|_| ())
}

/// Strip the home directory out of every property.
///
/// Targets panic payloads, whose message and stack frames come from `std` and
/// from dependencies rather than from this crate. See [`scrub`].
fn redact(event: &mut Event, home: &str) {
    if home.is_empty() {
        return;
    }

    let rewrites: Vec<(String, serde_json::Value)> = event
        .properties()
        .iter()
        .filter_map(|(key, value)| scrub::redacted(value, home).map(|new| (key.clone(), new)))
        .collect();

    for (key, value) in rewrites {
        let _ = event.insert_prop(key, value);
    }
}

fn build_event(state: &Arc<State>, name: &'static str, props: Vec<Prop>) -> Event {
    let mut event = Event::new(name.to_string(), state.distinct_id.clone());

    for (key, value) in props {
        // Fails only on a non-serializable value, which `serde_json::Value`
        // never is. Dropping one property beats dropping the event.
        let _ = event.insert_prop(key, value);
    }

    // Adds `$feature/<key>` and `$active_feature_flags`, so any event can be
    // sliced by flag without a join.
    if let Ok(flags) = state.flags.read()
        && let Some(flags) = flags.as_ref()
    {
        event.with_flags(flags);
    }

    event
}

/// A handled error, reduced to the parts that are safe to send.
///
/// The SDK wants something implementing `std::error::Error`, and the obvious
/// move — wrapping the real error — would ship its `Display`, which for
/// `CaptureError` names the device. So the message is built from the two
/// `&'static str` classifications instead, and the real error never leaves the
/// call site.
#[derive(Debug)]
struct ReportedError {
    kind: &'static str,
    detail: Option<&'static str>,
}

impl std::fmt::Display for ReportedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.detail {
            Some(detail) => write!(f, "{}: {}", self.kind, detail),
            None => write!(f, "{}", self.kind),
        }
    }
}

impl std::error::Error for ReportedError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reported_error_message_is_only_its_classifications() {
        let err = ReportedError {
            kind: "stream",
            detail: Some("permission_denied"),
        };
        assert_eq!(err.to_string(), "stream: permission_denied");

        let bare = ReportedError {
            kind: "no_input_device",
            detail: None,
        };
        assert_eq!(bare.to_string(), "no_input_device");
    }

    #[test]
    fn redact_rewrites_home_in_event_properties() {
        let mut event = Event::new("$exception".to_string(), "id".to_string());
        event
            .insert_prop("$exception_message", "open /Users/nfishel/x.png failed")
            .unwrap();
        event.insert_prop("kind", "io").unwrap();

        redact(&mut event, "/Users/nfishel");

        let props = event.properties();
        assert_eq!(
            props.get("$exception_message").unwrap(),
            &serde_json::json!("open ~/x.png failed")
        );
        // Untouched properties survive intact.
        assert_eq!(props.get("kind").unwrap(), &serde_json::json!("io"));
    }

    #[test]
    fn redact_is_a_noop_without_a_home() {
        let mut event = Event::new("e".to_string(), "id".to_string());
        event.insert_prop("p", "/Users/nfishel/x").unwrap();

        redact(&mut event, "");

        assert_eq!(
            event.properties().get("p").unwrap(),
            &serde_json::json!("/Users/nfishel/x")
        );
    }
}
