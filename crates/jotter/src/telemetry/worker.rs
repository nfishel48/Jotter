//! The one thread in the process that touches the network.
//!
//! Everything here runs off the caller's thread. Call sites only ever push a
//! [`Cmd`] onto a channel, so a slow or unreachable PostHog cannot stall a
//! command's output, and a recording never waits on an HTTP request.
//!
//! The thread owns a small tokio runtime and drives the SDK's futures with
//! `block_on`. Receiving on a blocking channel is what it does most of the time,
//! which is exactly the thing you must not do inside an async task — hence the
//! sync loop with async islands, rather than an async main loop.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender};

use posthog_rs::{
    CaptureExceptionOptions, ClientOptionsBuilder, ErrorTrackingOptionsBuilder, Event,
};

use super::{Prop, Surface, scrub};

/// How long teardown may spend draining, per surface.
///
/// This is a ceiling, not a delay: the drain returns as soon as the server
/// responds, which on a healthy network is well under 200ms. It only bites when
/// PostHog is slow or unreachable.
///
/// A CLI run is the case that sets it: `jotter devices` prints its table
/// instantly, and making someone watch a finished command sit there while a
/// background thread negotiates TLS is the kind of thing that gets telemetry
/// ripped out of a project. The ceiling is low enough to stay tolerable in the
/// worst case while still comfortably fitting a real round trip.
pub fn shutdown_budget_ms(surface: Surface) -> u64 {
    match surface {
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
    /// Build the client. Sent once, by `Telemetry::init`, which only creates a
    /// worker at all when telemetry is allowed.
    Start,
    /// Drain and stop. The channel acknowledges so the caller can bound its wait.
    Shutdown(SyncSender<()>),
}

/// State shared between the handle and the worker.
pub struct State {
    pub api_key: String,
    pub distinct_id: String,
    pub surface: Surface,
    pub home: String,
}

pub fn run(rx: Receiver<Cmd>, state: Arc<State>) {
    // One worker thread: this handles a few dozen events per run, and the
    // default (one per core) would be an absurd tax on a command that lives for
    // seconds.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("jotter-telemetry-io")
        .enable_all()
        .build();

    let Ok(runtime) = runtime else {
        return;
    };

    // Whether the PostHog client exists yet. `init_global` installs the panic
    // hook and stores the client in a `OnceLock`, so there is exactly one
    // chance to create it; a failure leaves every later event dropped here.
    let mut started = false;

    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Start => {
                if !started {
                    started = runtime.block_on(start(&state));
                }
            }

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

/// Bring the client up.
///
/// Returns whether capture is now possible.
async fn start(state: &Arc<State>) -> bool {
    let Ok(options) = client_options(state) else {
        return false;
    };

    // `init_global` before anything else, and before any network call,
    // because it is what installs the panic hook, and the hook is in a race
    // with the startup panics it exists to catch. `init_global` itself does no
    // I/O; it just builds the client. Evaluating feature flags first, as this
    // once did, put a full round trip in front of the hook and lost the panic
    // outright. Measured, not assumed: see docs/TELEMETRY.md.
    posthog_rs::init_global(options).await.is_ok()
}

fn client_options(state: &Arc<State>) -> Result<posthog_rs::ClientOptions, ()> {
    let home = state.home.clone();

    let error_tracking = ErrorTrackingOptionsBuilder::default()
        .capture_stacktrace(true)
        // Installs a process-wide panic hook. Without it a panic produces
        // nothing but a line on a stderr that, for anything launched through
        // the macOS bundle, nobody is reading.
        .capture_panics(true)
        // Without this every cpal, sherpa-onnx and tokio frame is "in app" and
        // the grouped issue is named after whichever dependency was on top.
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
        .error_tracking(error_tracking)
        .before_send(move |mut event| {
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
