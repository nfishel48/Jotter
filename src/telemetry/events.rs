//! The event vocabulary.
//!
//! Every name the app can emit is declared here, and nowhere else. Two reasons:
//! a typo'd event name is invisible until someone notices a chart is empty, and
//! `docs/TELEMETRY.md` has to stay an accurate list of what leaves the machine —
//! which is only maintainable if there is one place to read it off.
//!
//! Names follow PostHog's `[object] [verb]` convention, snake_cased.

use super::Prop;

pub const APP_STARTED: &str = "app_started";
pub const APP_EXITED: &str = "app_exited";

pub const RECORDING_STARTED: &str = "recording_started";
pub const RECORDING_COMPLETED: &str = "recording_completed";
pub const RECORDING_FAILED: &str = "recording_failed";
pub const RECORDING_PROCESSED: &str = "recording_processed";

pub const DEVICES_REFRESHED: &str = "devices_refreshed";
pub const DEVICE_LIST_FAILED: &str = "device_list_failed";

pub const SETTINGS_OPENED: &str = "settings_opened";
pub const TRAY_MENU_CLICKED: &str = "tray_menu_clicked";
pub const RECORDINGS_FOLDER_OPENED: &str = "recordings_folder_opened";

pub const TELEMETRY_OPTED_IN: &str = "telemetry_opted_in";
pub const TELEMETRY_OPTED_OUT: &str = "telemetry_opted_out";

/// The one feature flag the app ships with.
///
/// Lets ingestion be stopped for a release that turns out to be noisy or to
/// capture something it shouldn't, without waiting for users to update.
pub const FLAG_KILL_SWITCH: &str = "telemetry-kill-switch";

/// Properties attached to every event, describing the build rather than the user.
///
/// The `$`-prefixed names are PostHog reserved properties, so they populate the
/// standard columns instead of showing up as bespoke ones. `$os` is set
/// explicitly because the SDK's user agent is `posthog-rs/<version>` — with
/// nothing else to go on, PostHog would have no idea what platform this is.
pub fn context(surface: super::Surface) -> Vec<Prop> {
    vec![
        ("$app_name", "Jotter".into()),
        ("$app_version", env!("CARGO_PKG_VERSION").into()),
        ("$device_type", "Desktop".into()),
        ("$os", os_display_name().into()),
        ("arch", std::env::consts::ARCH.into()),
        ("surface", surface.as_str().into()),
        ("build_features", build_features().into()),
    ]
}

/// PostHog's spelling for the platform, not Rust's.
fn os_display_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "Mac OS X",
        "linux" => "Linux",
        "windows" => "Windows",
        other => other,
    }
}

/// Which front ends this binary was built with.
///
/// Worth recording because the three configurations fail differently, and a bug
/// report that says "the tray doesn't appear" means something very different
/// from a CLI-only build.
pub fn build_features() -> &'static str {
    match (cfg!(feature = "gui"), cfg!(feature = "cli")) {
        (true, true) => "gui+cli",
        (true, false) => "gui",
        (false, true) => "cli",
        (false, false) => "none",
    }
}

/// Everything worth reporting about a finished recording.
///
/// Takes the whole [`Meta`] and picks, rather than letting call sites assemble
/// their own: `Meta` exists to be written to `meta.json` next to the audio, so
/// it holds `path` and `device_name` — the two fields that must never be sent.
/// Funnelling through one function makes that a single place to review.
///
/// [`Meta`]: crate::audio::meta::Meta
pub fn recording_props(meta: &crate::audio::meta::Meta) -> Vec<Prop> {
    let mut props = vec![
        (
            "duration_bucket",
            duration_bucket(meta.duration_secs()).into(),
        ),
        (
            "track_count",
            (meta.mic.is_some() as u8 + meta.system.is_some() as u8).into(),
        ),
        ("stream_errors", track_sum(meta, |t| t.stream_errors).into()),
    ];

    for (label, track) in [("mic", &meta.mic), ("system", &meta.system)] {
        let Some(track) = track else {
            props.push((prefixed(label, "present"), false.into()));
            continue;
        };
        props.push((prefixed(label, "present"), true.into()));
        // The failure that matters most: a track that ran but produced nothing
        // is the signature of a denied permission prompt, and is otherwise
        // indistinguishable from a successful recording.
        props.push((prefixed(label, "captured_audio"), (track.frames > 0).into()));
        props.push((prefixed(label, "sample_rate"), track.sample_rate.into()));
        props.push((
            prefixed(label, "source_channels"),
            track.source_channels.into(),
        ));
    }

    // Millisecond resolution is plenty to spot a broken alignment and is not a
    // fingerprint the way an exact duration would be.
    if let Some(offset) = meta.track_offset_secs() {
        props.push(("track_offset_ms", ((offset * 1000.0).round() as i64).into()));
    }

    props
}

/// Everything worth reporting about an echo-cancellation pass.
///
/// Same contract as [`recording_props`]: this function picks, call sites do not
/// assemble. `AecReport` carries the output path, which must never be sent, so
/// funnelling through here keeps that a single place to review.
///
/// The two numbers that matter are `erle_db` — how much echo came out — and
/// `near_gain_db`, whether the filter ate the user's own voice. The second is
/// the one an ERLE figure cannot show, and the reason a pass can look
/// successful while having made the recording worse.
#[cfg(feature = "aec")]
pub fn aec_props(report: &crate::audio::process::AecReport, dry_run: bool) -> Vec<Prop> {
    let census = &report.census;
    let total = census.silence + census.near_only + census.far_only + census.double_talk;

    let mut props = vec![
        ("dry_run", dry_run.into()),
        ("applied", (report.output.is_some()).into()),
        ("delay_source", report.delay.source.as_str().into()),
        (
            "delay_ms",
            ((report.delay.frames as f32 * 1_000.0 / report.config.sample_rate.max(1) as f32)
                .round() as i64)
                .into(),
        ),
        (
            "residual_suppression",
            report.config.residual_suppression.into(),
        ),
        ("delay_segments", (report.delay.segments_used as u32).into()),
        ("drift_ppm", (report.delay.drift_ppm.round() as i64).into()),
        ("far_gap_secs", (report.far_gap_secs.round() as i64).into()),
        ("duration_bucket", duration_bucket(total as f64).into()),
    ];

    // Absent metrics are omitted rather than zeroed: a zero ERLE means "removed
    // nothing", which is a completely different finding from "never measured".
    if let Some(erle) = report.stats.erle_db {
        props.push(("erle_db", (erle.round() as i64).into()));
    }
    if let Some(gain) = report.stats.near_gain_db {
        props.push(("near_gain_db", (gain.round() as i64).into()));
    }
    if let Some(gain) = report.stats.double_talk_gain_db {
        props.push(("double_talk_gain_db", (gain.round() as i64).into()));
    }
    // AEC3's own delay estimate, alongside ours. A wide disagreement in
    // aggregate would mean one of the two estimators is wrong on real hardware.
    if let Some(ms) = report.stats.reported_delay_ms {
        props.push(("aec3_delay_ms", ms.into()));
    }
    if let Some(bypass) = report.bypass {
        // From `kind()`, never `Display` — the human-facing message embeds
        // durations and a device-shaped description.
        props.push(("bypass_reason", bypass.kind().into()));
    }

    // Fractions rather than seconds: the shape of a meeting is the useful
    // signal, and an exact duration is closer to a fingerprint.
    if total > 0.0 {
        props.push((
            "double_talk_pct",
            ((census.double_talk / total * 100.0).round() as i64).into(),
        ));
        props.push((
            "far_only_pct",
            ((census.far_only / total * 100.0).round() as i64).into(),
        ));
    }

    props
}

/// Property keys must be `&'static str`, and these are built from a fixed pair
/// of labels, so the mapping is spelled out rather than formatted.
fn prefixed(label: &str, suffix: &str) -> &'static str {
    match (label, suffix) {
        ("mic", "present") => "mic_present",
        ("mic", "captured_audio") => "mic_captured_audio",
        ("mic", "sample_rate") => "mic_sample_rate",
        ("mic", "source_channels") => "mic_source_channels",
        ("system", "present") => "system_present",
        ("system", "captured_audio") => "system_captured_audio",
        ("system", "sample_rate") => "system_sample_rate",
        ("system", "source_channels") => "system_source_channels",
        _ => unreachable!("unknown track property {label}_{suffix}"),
    }
}

fn track_sum(
    meta: &crate::audio::meta::Meta,
    f: impl Fn(&crate::audio::meta::TrackInfo) -> u64,
) -> u64 {
    meta.mic.as_ref().map(&f).unwrap_or(0) + meta.system.as_ref().map(&f).unwrap_or(0)
}

/// Bucket a duration in seconds.
///
/// A recording's exact length is closer to content than to usage: paired with a
/// timestamp it is a fairly strong fingerprint for "was this person in the 3pm
/// meeting". Buckets answer the only question actually being asked — are people
/// recording for seconds or for hours.
pub fn duration_bucket(secs: f64) -> &'static str {
    match secs {
        s if s < 10.0 => "<10s",
        s if s < 60.0 => "10s-1m",
        s if s < 300.0 => "1-5m",
        s if s < 900.0 => "5-15m",
        s if s < 1800.0 => "15-30m",
        s if s < 3600.0 => "30-60m",
        s if s < 7200.0 => "1-2h",
        _ => ">2h",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_buckets_cover_the_range_without_gaps() {
        assert_eq!(duration_bucket(0.0), "<10s");
        assert_eq!(duration_bucket(9.99), "<10s");
        assert_eq!(duration_bucket(10.0), "10s-1m");
        assert_eq!(duration_bucket(59.9), "10s-1m");
        assert_eq!(duration_bucket(60.0), "1-5m");
        assert_eq!(duration_bucket(3599.0), "30-60m");
        assert_eq!(duration_bucket(3600.0), "1-2h");
        assert_eq!(duration_bucket(f64::MAX), ">2h");
    }

    #[test]
    fn duration_buckets_are_monotonic() {
        // Each boundary must move to a different bucket, or the bucket is
        // unreachable and the chart silently loses a band.
        let boundaries = [10.0, 60.0, 300.0, 900.0, 1800.0, 3600.0, 7200.0];
        for edge in boundaries {
            assert_ne!(
                duration_bucket(edge - 0.001),
                duration_bucket(edge),
                "bucket boundary at {edge} does not change bucket"
            );
        }
    }

    #[test]
    fn event_names_are_snake_case() {
        for name in [
            APP_STARTED,
            APP_EXITED,
            RECORDING_STARTED,
            RECORDING_COMPLETED,
            RECORDING_FAILED,
            DEVICES_REFRESHED,
            DEVICE_LIST_FAILED,
            SETTINGS_OPENED,
            TRAY_MENU_CLICKED,
            RECORDINGS_FOLDER_OPENED,
            TELEMETRY_OPTED_IN,
            TELEMETRY_OPTED_OUT,
        ] {
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{name} is not snake_case"
            );
        }
    }

    fn track(name: &str, frames: u64) -> crate::audio::meta::TrackInfo {
        crate::audio::meta::TrackInfo {
            path: format!("/Users/nfishel/Documents/Jotter/2026-09-16_10-00-00/{name}.wav"),
            device_name: "Nick's AirPods Pro".into(),
            device_id: Some("AppleHDA:91:Nicks-AirPods".into()),
            sample_rate: 48_000,
            channels: 1,
            source_channels: 2,
            frames,
            first_callback_nanos: Some(1_000_000),
            stream_errors: 2,
        }
    }

    #[test]
    fn recording_props_omit_device_names_and_paths() {
        let meta = crate::audio::meta::Meta {
            started_at: 0.0,
            ended_at: 125.0,
            mic: Some(track("mic", 6_000_000)),
            system: Some(track("system", 0)),
            aec: None,
        };

        let props = recording_props(&meta);
        let rendered = format!("{props:?}");

        // The exact fields `Meta` carries that identify a person or a machine.
        for leaked in ["AirPods", "Nick", "nfishel", "Documents", ".wav", "Apple"] {
            assert!(
                !rendered.contains(leaked),
                "{leaked:?} leaked into recording props: {rendered}"
            );
        }
    }

    #[test]
    fn recording_props_report_the_signal_that_matters() {
        let meta = crate::audio::meta::Meta {
            started_at: 0.0,
            ended_at: 125.0,
            mic: Some(track("mic", 6_000_000)),
            system: Some(track("system", 0)),
            aec: None,
        };

        let props = recording_props(&meta);
        let get = |key: &str| {
            props
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("missing {key}"))
        };

        assert_eq!(get("duration_bucket"), serde_json::json!("1-5m"));
        assert_eq!(get("track_count"), serde_json::json!(2));
        // A track that ran but produced nothing: the permission-denial signature.
        assert_eq!(get("mic_captured_audio"), serde_json::json!(true));
        assert_eq!(get("system_captured_audio"), serde_json::json!(false));
        assert_eq!(get("stream_errors"), serde_json::json!(4));
        assert_eq!(get("mic_sample_rate"), serde_json::json!(48_000));
    }

    #[test]
    fn recording_props_handle_a_single_track() {
        let meta = crate::audio::meta::Meta {
            started_at: 0.0,
            ended_at: 5.0,
            mic: Some(track("mic", 240_000)),
            system: None,
            aec: None,
        };

        let props = recording_props(&meta);
        let get = |key: &str| {
            props
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
        };

        assert_eq!(get("track_count"), Some(serde_json::json!(1)));
        assert_eq!(get("system_present"), Some(serde_json::json!(false)));
        // Absent tracks contribute no rate or channel count at all, rather than
        // a zero that would drag down an average.
        assert_eq!(get("system_sample_rate"), None);
        assert_eq!(get("track_offset_ms"), None);
    }

    #[test]
    fn context_carries_no_user_identifying_values() {
        let props = context(crate::telemetry::Surface::Gui);
        let rendered = format!("{props:?}");

        // Everything in `context` must describe the build or the platform. If a
        // path or a hostname ever appears here it would be on every event.
        let home = std::env::var("HOME").unwrap_or_default();
        if !home.is_empty() {
            assert!(!rendered.contains(&home));
        }
        assert!(props.iter().any(|(k, _)| *k == "$app_version"));
        assert!(props.iter().any(|(k, _)| *k == "build_features"));
    }
}
