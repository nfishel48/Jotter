# Telemetry

Jotter sends anonymous usage and crash reports to [PostHog](https://posthog.com).
This document is the complete list of what that means. If anything here is wrong
or out of date, that is a bug — please open an issue.

Jotter records microphone and system audio. Nothing about that audio, or about
the files it produces, is ever sent.

## Turning it off

Any one of these is sufficient, and each takes effect immediately:

| How | Where |
| --- | --- |
| Untick **Send anonymous usage and crash reports** | Settings pane, bottom |
| `jotter telemetry --disable` | Terminal; `jotter telemetry` shows current state |
| `DO_NOT_TRACK=1` | Environment ([consoledonottrack.com](https://consoledonottrack.com)) |
| `JOTTER_TELEMETRY=0` | Environment; overrides the stored setting either way |
| `cargo build --no-default-features --features gui,cli` | Build with no telemetry code at all |

The preference lives in `settings.json`, alongside the rest of Jotter's config:

- macOS — `~/Library/Application Support/Jotter/settings.json`
- Linux — `${XDG_CONFIG_HOME:-~/.config}/jotter/settings.json`

`jotter telemetry` prints the exact path.

While telemetry is off, no PostHog client is created, no feature flags are
fetched, and the process makes no network connections of any kind. Jotter has no
other reason to use the network, so an opted-out Jotter is entirely offline.

Builds without an API key — which is every build that did not come from this
repository's release workflow, including any `cargo build` you run yourself —
also send nothing. The settings pane says so instead of showing a live checkbox.

## What is collected

### Attached to every event

| Property | Example |
| --- | --- |
| `$app_name`, `$app_version` | `Jotter`, `0.1.3` |
| `$os`, `$os_version` | `Mac OS X`, `26.3.1` |
| `arch`, `$device_type` | `aarch64`, `Desktop` |
| `surface` | `gui` or `cli` |
| `build_features` | `gui+cli` |
| `$feature/*` | Which feature flags were active |
| `$lib`, `$lib_version` | `posthog-rs`, `0.25.5` |

The PostHog SDK copies the `$`-prefixed ones above into a person profile, as
`$set` (current) and `$set_once` (`$initial_app_version`, and so on). That is the
entire profile: app version, OS, OS version, device type. Nothing is added to it
beyond what is in the table.

Plus a `distinct_id`: a random UUID generated on first run and stored in
`settings.json`. It is not derived from your hardware, username, hostname, or
network, and it is not linked to any account. Deleting `settings.json` produces a
new one.

No location data of any kind. GeoIP enrichment is disabled, and each event
carries an explicit null `$ip` so ingestion does not record the address it
arrived from.

This is stricter than PostHog's default, deliberately. With enrichment on, every
event is stamped with `$geoip_city_name`, `$geoip_postal_code` and latitude /
longitude — a postal code and coordinates, attached to a stable install id, from
an app that records your meetings. There is no country-only setting, so Jotter
takes the other option.

### Events

| Event | When | Properties |
| --- | --- | --- |
| `app_started` | Launch | `is_first_run`, `device_count`, `has_loopback_device`, `launch_failed` |
| `app_exited` | Quit | `reason`, `session_secs`, `recordings_this_session` |
| `recording_started` | Recording begins | `mic_is_default`, `system_is_default`, `has_loopback_device`, `sources`, `fixed_duration`, `force_system_on_duplex` |
| `recording_completed` | Recording saved | `duration_bucket`, `track_count`, `stream_errors`, `{mic,system}_present`, `{mic,system}_captured_audio`, `{mic,system}_sample_rate`, `{mic,system}_source_channels`, `track_offset_ms` |
| `recording_failed` | Recording could not start or finish | `phase`, `error_kind`, `cpal_kind`, `permission_shaped` |
| `recording_processed` | Echo cancellation ran, or declined to | `dry_run`, `applied`, `delay_source`, `delay_ms`, `delay_segments`, `drift_ppm`, `aec3_delay_ms`, `far_gap_secs`, `duration_bucket`, `erle_db`, `near_gain_db`, `double_talk_gain_db`, `bypass_reason`, `double_talk_pct`, `far_only_pct` |
| `recording_transcribed` | Transcription ran, or declined to | `model`, `engine`, `produced_transcript`, `duration_bucket`, `decline_reason`, `segments`, `mic_segments`, `system_segments`, `words`, `speech_pct`, `realtime_factor_pct` |
| `devices_refreshed` | Device list read | `total`, `input_capable`, `loopback_capable`, `has_default_output` |
| `device_list_failed` | Device list could not be read | `error_kind` |
| `settings_opened` | Settings window shown | `trigger` |
| `tray_menu_clicked` | Tray menu used | `id` |
| `recordings_folder_opened` | Folder opened in Finder/file manager | `source` |
| `telemetry_opted_in` / `telemetry_opted_out` | The setting changed | — |

Recording length is reported as a bucket (`<10s`, `1-5m`, `>2h`, …) rather than a
number. An exact duration paired with a timestamp would be a reasonably strong
fingerprint for *which meeting* was recorded; the bucket answers the only
question actually being asked.

`error_kind` is a fixed identifier such as `no_input_device` or
`duplex_system_device` — never the error message, which names the device
involved. `bypass_reason` works the same way: a fixed identifier like
`track_length_mismatch`, never the human-readable explanation, which embeds
durations and a device-shaped description.

The echo-cancellation properties are all integers or fixed identifiers. The two
worth explaining: `erle_db` is how much echo was removed, and `near_gain_db` is
how much of *your* voice was lost doing it — the second is what says whether the
feature is working or quietly making recordings worse, and it is the reason the
first is not reported alone. Both are omitted entirely when they could not be
measured, because a zero would be indistinguishable from "removed nothing".
`double_talk_pct` and `far_only_pct` are percentages rather than seconds: the
shape of a meeting is the useful signal, an exact duration is closer to a
fingerprint. The path of the file written is never sent.

The transcription properties carry the highest stakes of the lot, because the
code that produces them has the entire contents of a private meeting in memory.
So: **no text, and nothing derived from text.** Not the words, not a sample, not
a first line, not a language guess. `words` is a count, `speech_pct` a
percentage, `segments` a tally of how many stretches of speech were found —
none of them says anything about what was said, and the transcript itself is
never read to compute any of them. `model` is a catalogue id such as
`parakeet-tdt-0.6b-v2-int8`, and `decline_reason` is a fixed identifier like
`model_missing`, never the sentence shown to you. The path of the transcript is
never sent.

The one property that whole event exists for is `realtime_factor_pct`: how long
transcribing took as a percentage of the audio's own length. It is the number
that says whether this feature is usable on the hardware people actually own,
and there is nowhere else it can be measured.

### Crash reports

Panics and handled errors are sent to PostHog Error Tracking with a stack trace.
Stack traces are generated by Rust and can contain file paths from the machine
the binary ran on; every occurrence of your home directory is rewritten to `~`
before sending. Verified on a real captured panic whose message contained a home
path: the ingested event had no occurrence of the username or of `/Users/`, and
the path arrived rewritten.

A panic in the first few milliseconds of launch may not be captured. The hook is
installed as soon as the telemetry thread has built its client, but window and
tray construction can win that race. Nothing is lost when it does — the panic
simply is not reported.

## What is never collected

- Audio, in any form, whole or partial
- **Transcript text, in any form, whole or partial** — no words, no excerpt, no
  summary, and nothing computed from what was said
- File names, folder names, or recording paths
- **Device names or device ids.** A microphone is routinely named after its
  owner, so these are treated as personal data and never leave the machine
- `meta.json` contents
- Your username, hostname, or email
- Your IP address, or any location derived from it
- Keystrokes, window titles, or anything about other applications
- Anything at all while telemetry is off

## Feature flags

Jotter evaluates PostHog feature flags remotely, once at startup, in the tray app
only. The only flag that currently exists is `telemetry-kill-switch`, which lets
ingestion be stopped for a release that turns out to be misbehaving without
waiting for everyone to update.

Local flag evaluation is deliberately not used: it requires a personal API key,
and there is nowhere to put one in an open-source binary.

## For contributors

- Event names live in `src/telemetry/events.rs` and nowhere else.
- Event property values are `&'static str` or numbers. That is not a convention,
  it is the type signature of `Telemetry::track` and `Telemetry::report_error` —
  a `&'static str` cannot hold a device name or a path, so the compiler enforces
  most of this document. Use `CaptureError::kind()`, never `err.to_string()`.
- `src/telemetry/scrub.rs` redacts the home directory from everything on the way
  out. It exists for payloads Jotter does not construct itself, namely panic
  messages and stack frames.
- Adding an event means adding a row to the table above in the same commit.
