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
pub const RECORDING_TRANSCRIBED: &str = "recording_transcribed";
pub const RECORDING_DIARIZED: &str = "recording_diarized";

pub const DEVICES_REFRESHED: &str = "devices_refreshed";
pub const DEVICE_LIST_FAILED: &str = "device_list_failed";

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

/// Which offline passes this binary was built with.
///
/// Worth recording because each one is a feature a packager can turn off, and
/// a missing `recording_transcribed` means something very different from a
/// build that has no transcription stage to send it. `diarize` implies
/// `transcribe`, so the combinations below are all there are.
pub fn build_features() -> &'static str {
    match (
        cfg!(feature = "aec"),
        cfg!(feature = "transcribe"),
        cfg!(feature = "diarize"),
    ) {
        (true, true, true) => "aec+transcribe+diarize",
        (true, true, false) => "aec+transcribe",
        (true, false, _) => "aec",
        (false, true, true) => "transcribe+diarize",
        (false, true, false) => "transcribe",
        (false, false, _) => "none",
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
    // For `AecBypass::kind` below. The trait is ungated core code; this
    // function is not, so the import is local rather than at file scope.
    use crate::audio::stage::DeclineReason;

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

/// Everything worth reporting about a transcription pass.
///
/// Same contract as [`recording_props`] and [`aec_props`]: this function picks,
/// call sites do not assemble. `TranscriptReport` carries the output path — and
/// the stage it came from has the transcript itself in memory — so funnelling
/// through here keeps "no meeting content ever leaves the machine" a single
/// place to review.
///
/// **No text, and nothing derived from text.** Not the words, not a sample, not
/// a language guess. `words` is a count and `speech_secs` a duration; neither
/// says anything about what was said. If a property here ever needs the
/// transcript to compute, that is the signal it does not belong.
///
/// The figure worth having in aggregate is the real-time factor: it is the one
/// number that decides whether this feature is usable on the hardware people
/// actually own, and it cannot be measured anywhere but here.
#[cfg(feature = "transcribe")]
pub fn transcript_props(report: &crate::audio::transcribe::TranscriptReport) -> Vec<Prop> {
    use crate::audio::stage::DeclineReason;

    let mut props = vec![
        // The model id is catalogue data, never free-form — see `models::Model`.
        ("model", report.model_id.into()),
        ("engine", report.engine.into()),
        ("produced_transcript", report.output.is_some().into()),
        (
            "duration_bucket",
            duration_bucket(report.audio_secs as f64).into(),
        ),
    ];

    if let Some(decline) = &report.decline {
        // From `kind()`, never `Display`: the human-facing message names the
        // model and tells the user what to run.
        props.push(("decline_reason", decline.kind().into()));
        // A decline decided nothing about the audio, so the figures below would
        // all be zero and would drag every average down with them.
        return props;
    }

    props.push(("segments", report.segments.into()));
    props.push(("mic_segments", report.mic_segments.into()));
    props.push(("system_segments", report.system_segments.into()));
    props.push(("words", report.words.into()));

    // Fractions and ratios rather than raw seconds, for the reason `aec_props`
    // gives: the shape of a meeting is the signal, an exact duration is closer
    // to a fingerprint.
    if report.audio_secs > 0.0 {
        props.push((
            "speech_pct",
            ((report.speech_secs / report.audio_secs * 100.0).round() as i64).into(),
        ));
        // Scaled by 100 because `Prop` carries integers, and a bare `0` would
        // lose the difference between "twice as fast as realtime" and "fifty
        // times", which is the whole point of recording it.
        props.push((
            "realtime_factor_pct",
            ((report.elapsed_secs / report.audio_secs * 100.0).round() as i64).into(),
        ));
    }

    props
}

/// Everything worth reporting about a diarization pass.
///
/// Same contract as [`transcript_props`]: this function picks, call sites do not
/// assemble.
///
/// **No speaker labels, and nothing that could become one.** Not the labels,
/// not a per-speaker word count, not how long each person talked. `speakers` is
/// a count — how many people were on the call — and the rest describe how well
/// the pass ran. A property here that needed the transcript to compute is the
/// signal it does not belong.
///
/// The figure worth having in aggregate is `attributed_pct`: the share of
/// system segments that got a label at all. It is the one number that says
/// whether this feature works on real meetings rather than on the one the
/// developer tested, and — like the transcription pass's real-time factor — it
/// cannot be measured anywhere but here.
#[cfg(feature = "diarize")]
pub fn diarize_props(report: &crate::audio::diarize::DiarizeReport) -> Vec<Prop> {
    use crate::audio::stage::DeclineReason;

    let mut props = vec![
        // Both ids are catalogue data, never free-form — see `models::Model`.
        ("segmentation_model", report.segmentation_model_id.into()),
        ("embedding_model", report.embedding_model_id.into()),
        ("engine", report.engine.into()),
        ("labelled_transcript", report.output.is_some().into()),
        (
            "duration_bucket",
            duration_bucket(report.audio_secs as f64).into(),
        ),
    ];

    if let Some(decline) = &report.decline {
        // From `kind()`, never `Display`: the human-facing message tells the
        // user what to run.
        props.push(("decline_reason", decline.kind().into()));
        // A decline decided nothing about the audio, so the figures below would
        // all be zero and would drag every average down with them.
        return props;
    }

    props.push(("speakers", report.speakers.into()));
    props.push(("system_segments", report.system_segments.into()));

    // Fractions rather than raw seconds, for the reason `aec_props` gives: the
    // shape of a meeting is the signal, an exact duration is closer to a
    // fingerprint.
    if report.system_segments > 0 {
        props.push((
            "attributed_pct",
            ((report.attributed_segments as f32 / report.system_segments as f32 * 100.0).round()
                as i64)
                .into(),
        ));
    }
    if report.audio_secs > 0.0 {
        // Scaled by 100 for the reason the transcription pass's is: `Prop`
        // carries integers, and a bare `0` would lose the difference between
        // "twice as fast as realtime" and "fifty times".
        props.push((
            "realtime_factor_pct",
            ((report.elapsed_secs / report.audio_secs * 100.0).round() as i64).into(),
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
            RECORDING_PROCESSED,
            RECORDING_TRANSCRIBED,
            RECORDING_DIARIZED,
            DEVICES_REFRESHED,
            DEVICE_LIST_FAILED,
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
            transcript: None,
            diarization: None,
            live: None,
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

    /// The same PII contract for the processing pass. `AecReport` carries the
    /// output path, so this is the test that keeps it out.
    #[cfg(feature = "aec")]
    #[test]
    fn aec_props_omit_the_output_path() {
        use crate::audio::process::{AecReport, Census};

        let report = AecReport {
            delay: crate::audio::aec::delay::DelayEstimate::unaligned(Some(256)),
            census: Census {
                silence: 28.0,
                near_only: 91.0,
                far_only: 22.0,
                double_talk: 394.0,
            },
            stats: crate::audio::aec::AecStats {
                frames: 53_580,
                erle_db: Some(20.8),
                near_gain_db: Some(-0.4),
                double_talk_gain_db: Some(-17.6),
                reported_delay_ms: Some(16),
            },
            config: crate::audio::aec::AecConfig::default(),
            far_gap_secs: 0.021,
            bypass: None,
            output: Some("/Users/nfishel/Documents/Jotter/2026-09-16/mic_aec.wav".into()),
        };

        let rendered = format!("{:?}", aec_props(&report, false));
        for leaked in ["nfishel", "Documents", "Jotter", ".wav", "mic_aec"] {
            assert!(
                !rendered.contains(leaked),
                "{leaked:?} leaked into aec props: {rendered}"
            );
        }
    }

    /// Absent metrics are omitted rather than zeroed: a 0 dB ERLE means
    /// "removed nothing", which is a completely different finding from "never
    /// measured", and averaging the two together would hide both.
    #[cfg(feature = "aec")]
    #[test]
    fn aec_props_omit_unmeasured_figures_rather_than_zeroing_them() {
        use crate::audio::process::{AecBypass, AecReport, Census};

        let report = AecReport {
            delay: crate::audio::aec::delay::DelayEstimate::unaligned(None),
            census: Census::default(),
            stats: crate::audio::aec::AecStats::default(),
            config: crate::audio::aec::AecConfig::default(),
            far_gap_secs: 5.4,
            bypass: Some(AecBypass::TrackLengthMismatch { delta_secs: 5.4 }),
            output: None,
        };

        let props = aec_props(&report, false);
        let keys: Vec<&str> = props.iter().map(|(k, _)| *k).collect();
        assert!(!keys.contains(&"erle_db"), "unmeasured ERLE must be absent");
        assert!(!keys.contains(&"near_gain_db"));
        // The reason a pass declined is the most useful thing it can report.
        let rendered = format!("{props:?}");
        assert!(rendered.contains("track_length_mismatch"));
        assert!(rendered.contains("bypass_reason"));
    }

    #[test]
    fn recording_props_report_the_signal_that_matters() {
        let meta = crate::audio::meta::Meta {
            started_at: 0.0,
            ended_at: 125.0,
            mic: Some(track("mic", 6_000_000)),
            system: Some(track("system", 0)),
            aec: None,
            transcript: None,
            diarization: None,
            live: None,
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
            transcript: None,
            diarization: None,
            live: None,
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
        let props = context(crate::telemetry::Surface::Cli);
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

    #[cfg(feature = "transcribe")]
    fn transcript_report(
        decline: Option<crate::audio::transcribe::TranscribeDecline>,
    ) -> crate::audio::transcribe::TranscriptReport {
        crate::audio::transcribe::TranscriptReport {
            model_id: "parakeet-tdt-0.6b-v2-int8",
            engine: "sherpa-onnx",
            segments: 214,
            mic_segments: 88,
            system_segments: 126,
            words: 3_104,
            speech_secs: 487.5,
            audio_secs: 1_062.0,
            elapsed_secs: 73.2,
            output: decline
                .is_none()
                .then(|| "/Users/nfishel/Documents/Jotter/2026-09-16/transcript.json".into()),
            decline,
        }
    }

    /// The PII contract for transcription, which carries the highest stakes of
    /// the three: the stage this describes has the entire contents of a private
    /// meeting in memory. `TranscriptReport` holds the output path, and the
    /// recording directory is named after a timestamp under the user's home.
    ///
    /// Nothing derived from the transcript's *text* may appear either — no
    /// sample, no language guess, no first words. `words` is a count and
    /// `speech_secs` a duration, and neither says anything about what was said.
    #[cfg(feature = "transcribe")]
    #[test]
    fn transcript_props_omit_the_path_and_everything_said() {
        let rendered = format!("{:?}", transcript_props(&transcript_report(None)));

        for leaked in ["nfishel", "Documents", "Jotter", ".json", "2026-09-16"] {
            assert!(
                !rendered.contains(leaked),
                "{leaked:?} leaked into transcript props: {rendered}"
            );
        }

        // Every value sent is a number, a bool, or a string from the catalogue.
        // A free-form string appearing here is the shape a leak would take.
        for (key, value) in transcript_props(&transcript_report(None)) {
            if let Some(text) = value.as_str() {
                assert!(
                    ["parakeet-tdt-0.6b-v2-int8", "sherpa-onnx"].contains(&text)
                        || key == "duration_bucket",
                    "unexpected free-form value {text:?} under {key:?}"
                );
            }
        }
    }

    /// A decline measured nothing, so reporting zero segments and a zero
    /// real-time factor would drag every aggregate down with values that
    /// describe a pass which never ran. The reason is the whole finding.
    #[cfg(feature = "transcribe")]
    #[test]
    fn a_decline_reports_its_reason_and_no_measurements() {
        use crate::audio::transcribe::TranscribeDecline;

        let props = transcript_props(&transcript_report(Some(TranscribeDecline::ModelMissing {
            model_id: "parakeet-tdt-0.6b-v2-int8",
            files: 4,
        })));
        let get = |key: &str| {
            props
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
        };

        assert_eq!(
            get("decline_reason"),
            Some(serde_json::json!("model_missing"))
        );
        assert_eq!(get("produced_transcript"), Some(serde_json::json!(false)));
        assert_eq!(get("segments"), None);
        assert_eq!(get("words"), None);
        assert_eq!(get("realtime_factor_pct"), None);

        // And the reason is the stable `kind()`, never the sentence — which
        // names the model and tells the user which command to run.
        let rendered = format!("{props:?}");
        assert!(!rendered.contains("jotter models pull"), "{rendered}");
    }

    /// The figure the whole event exists for: whether this is fast enough to be
    /// usable on the hardware people own. Integer percent, because `Prop` values
    /// are JSON numbers and a bare `0` would lose the difference between twice
    /// realtime and fifty times.
    #[cfg(feature = "transcribe")]
    #[test]
    fn the_realtime_factor_survives_as_an_integer() {
        let props = transcript_props(&transcript_report(None));
        let get = |key: &str| {
            props
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
        };

        // 73.2s of work for 1062s of audio.
        assert_eq!(get("realtime_factor_pct"), Some(serde_json::json!(7)));
        assert_eq!(get("speech_pct"), Some(serde_json::json!(46)));
    }

    #[cfg(feature = "diarize")]
    fn diarize_report(
        decline: Option<crate::audio::diarize::DiarizeDecline>,
    ) -> crate::audio::diarize::DiarizeReport {
        crate::audio::diarize::DiarizeReport {
            // From the catalogue rather than spelled out, so swapping a model
            // cannot leave this fixture asserting against an id that no longer
            // exists — which is exactly what happened once already.
            segmentation_model_id: crate::models::DEFAULT_SEGMENTATION_MODEL.id,
            embedding_model_id: crate::models::DEFAULT_EMBEDDING_MODEL.id,
            engine: "sherpa-onnx",
            speakers: 3,
            system_segments: 126,
            attributed_segments: 119,
            audio_secs: 1_062.0,
            elapsed_secs: 106.2,
            output: decline
                .is_none()
                .then(|| "/Users/nfishel/Documents/Jotter/2026-09-16/transcript.json".into()),
            decline,
        }
    }

    /// The PII contract for diarization. The stakes here are subtly different
    /// from transcription's: this stage does not hold the text, but it holds the
    /// one thing the text does not make explicit — *who was in the meeting*.
    /// A speaker label is a pseudonym today and the anchor for a real name one
    /// step later, so none of them may leave the machine, and neither may
    /// anything per-speaker that a label could be reconstructed from.
    #[cfg(feature = "diarize")]
    #[test]
    fn diarize_props_omit_the_path_and_every_speaker() {
        let rendered = format!("{:?}", diarize_props(&diarize_report(None)));

        for leaked in [
            "nfishel",
            "Documents",
            "Jotter",
            ".json",
            "2026-09-16",
            "speaker_",
        ] {
            assert!(
                !rendered.contains(leaked),
                "{leaked:?} leaked into diarize props: {rendered}"
            );
        }

        // Every value sent is a number, a bool, or a string from the catalogue.
        // A free-form string appearing here is the shape a leak would take.
        for (key, value) in diarize_props(&diarize_report(None)) {
            if let Some(text) = value.as_str() {
                assert!(
                    [
                        crate::models::DEFAULT_SEGMENTATION_MODEL.id,
                        crate::models::DEFAULT_EMBEDDING_MODEL.id,
                        "sherpa-onnx",
                    ]
                    .contains(&text)
                        || key == "duration_bucket",
                    "unexpected free-form value {text:?} under {key:?}"
                );
            }
        }
    }

    /// Same rule as transcription's: a decline measured nothing, so the reason
    /// is the whole finding and the figures must be absent rather than zero.
    #[cfg(feature = "diarize")]
    #[test]
    fn a_diarization_decline_reports_its_reason_and_no_measurements() {
        use crate::audio::diarize::DiarizeDecline;

        let props = diarize_props(&diarize_report(Some(DiarizeDecline::ModelsMissing {
            files: 2,
        })));
        let get = |key: &str| {
            props
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
        };

        assert_eq!(
            get("decline_reason"),
            Some(serde_json::json!("models_missing"))
        );
        assert_eq!(get("labelled_transcript"), Some(serde_json::json!(false)));
        assert_eq!(get("speakers"), None);
        assert_eq!(get("attributed_pct"), None);
        assert_eq!(get("realtime_factor_pct"), None);

        // The reason is the stable `kind()`, never the sentence — which tells
        // the user which command to run.
        let rendered = format!("{props:?}");
        assert!(!rendered.contains("jotter models pull"), "{rendered}");
    }

    /// The figure the whole event exists for: how often the pass could place a
    /// segment at all. Segments it could not are left unlabelled rather than
    /// guessed at, so this is the honest measure of whether diarization works on
    /// real meetings.
    #[cfg(feature = "diarize")]
    #[test]
    fn the_attributed_share_survives_as_an_integer() {
        let props = diarize_props(&diarize_report(None));
        let get = |key: &str| {
            props
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
        };

        // 119 of 126 system segments placed.
        assert_eq!(get("attributed_pct"), Some(serde_json::json!(94)));
        // 106.2s of work for 1062s of audio.
        assert_eq!(get("realtime_factor_pct"), Some(serde_json::json!(10)));
        assert_eq!(get("speakers"), Some(serde_json::json!(3)));
    }
}
