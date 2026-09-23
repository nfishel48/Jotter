# jotter commands for an agent

Checked against `target/debug/jotter` built from `origin/trunk/cli-agent` at `cd58327` (plus this skill commit) on 2026-09-23. `jotter context --help`, `jotter recordings --help`, and the `--json` runs below match this file. If a later binary disagrees, trust the binary.

## How to call it

`--json` is global. `jotter models list --json` and `jotter --json models list` are the same command.

| Outcome | Stdout | Exit |
| --- | --- | --- |
| Success | Exactly one JSON object, one line. No prose before or after it. | 0 |
| `--follow` | One JSON object per new segment, one line, until the process exits. This stream is JSON even without `--json`. | 0 when the session has ended and the file has been drained |
| Failure with `--json` | `{"error":{"kind":"<snake_case>","message":"..."}}` | non-zero |
| Help or version | clap's own text, even if `--json` is present. Not an error. | 0 |

Branch on `error.kind`. Do not scrape `message`, and do not branch on the exit code beyond "non-zero means it failed". A command line that does not parse is kind `usage` and exit 2. Other failures exit 1.

There is no interactive prompt when `--json` is set, or when stdin is not a terminal. Do not wait for one.

`--follow` has no end sentinel. The process exits after it has drained the last line. Read until it exits, then `jotter context --json` for the finished transcript.

## Error kinds

These strings are the interface. New kinds may be added; none of these will be renamed.

| `kind` | When | What to do |
| --- | --- | --- |
| `usage` | The command line did not parse. `--last -1` is this, because clap takes the dash as a flag; `--last=-1` is `invalid_argument`. | Fix the invocation. Do not retry it unchanged. |
| `invalid_argument` | Parsed, but the value is unacceptable: a bad cursor, a negative `--last`, an unknown config key, or `--follow` with a `--dir` that is not the active session. | Fix the value. Take a fresh `context` without `--since` rather than editing a cursor. |
| `unknown_model` | `models pull --model` was given an id not in the catalogue. | `jotter models list --json` and use a listed `id`. |
| `device_not_found` | The device id does not exist, or there is no device. | `jotter devices --json`, then retry once. |
| `capture_failed` | The device was found but capture could not start or stop. Usual shape of a denied permission. | Tell the user. On macOS they grant microphone and system audio to `Jotter.app`. Do not loop. |
| `stage_failed` | Echo cancellation, transcription, or diarization hit an error. | Report `message`. The recording is still on disk. |
| `download_failed` | A model download failed or did not verify. | Report `message`. Do not start the meeting without a ready model. |
| `io` | Reading or writing a file failed. The first recording can also be Documents-folder access denied. | Report `message`. Do not loop. |
| `session_active` | `start` while a session is already recording. | Use it: `status`, then `context`. Do not start another. |
| `session_busy` | `stop` while another `stop` is already finishing this session. | Wait. Do not start a second `stop`. |
| `no_session` | `stop` or `context --follow` with nothing recording. A dead previous session is also this kind on `stop`; `message` says it is stale. | Do not invent a recording. `recordings` and `context` without `--follow` can still read the last one. |
| `session_failed` | The recorder exited without starting, or without finalising. | Report `message`. Do not immediately `start` again unless the user asks. |
| `session_timeout` | The recorder did not report that it started within 30s, or did not finalise the WAV headers within 60s of `stop`. The offline passes run after that 60s and can take as long as the meeting; this kind is not "transcription was slow". | Report `message`. Do not retry in a loop. |
| `bundle_not_found` | macOS: no `Jotter.app` to run the recorder in. | Tell the user to run `scripts/bundle.sh`. Do not retry. Do not set `JOTTER_NO_BUNDLE`. |
| `launch_failed` | The recorder process could not be launched at all. | Report `message`. Do not retry in a loop. |
| `not_found` | `context` has no recording to read, or `--dir` is not a directory. | Do not invent dialogue. `start` one, or pass a `dir` from `recordings`. |

Observed:

```json
{"error":{"kind":"not_found","message":"no recording at /tmp/jotter-no-such-recording"}}
```

```json
{"error":{"kind":"invalid_argument","message":"\"nope\" is not a live cursor (expected live:<hex>)"}}
```

```json
{"error":{"kind":"invalid_argument","message":"--last takes a non-negative number of seconds"}}
```

```json
{"error":{"kind":"invalid_argument","message":"--follow reads the active session; --dir names a different recording"}}
```

```json
{"error":{"kind":"no_session","message":"no session is recording"}}
```

## Models

```bash
jotter models list --json
jotter models path --json
jotter models pull --json
jotter models pull --model <ID> --json
```

`models list`:

| Field | Meaning |
| --- | --- |
| `models_dir` | Where weights live. macOS: `~/Library/Application Support/Jotter/models`. Linux: `${XDG_DATA_HOME:-~/.local/share}/jotter/models`. `JOTTER_MODELS_DIR` overrides either, if it is absolute. |
| `models[].id` | Catalogue id. |
| `models[].engine` | `sherpa-onnx` for the shipped catalogue. |
| `models[].description` | Human label. Do not branch on it. |
| `models[].bytes` | Download size. |
| `models[].ready` | `true` only when every file is present and the right size. |
| `models[].missing` | Files missing or the wrong size. `0` when `ready` is true. |

The default recogniser is `parakeet-tdt-0.6b-v2-int8`. Live and offline transcription also need `silero-vad`. Pull if either is not ready. `models pull` with no `--model` also fetches `pyannote-segmentation-3-0` and `nemo-en-titanet-small`. That does not turn diarization on.

`models pull` prints `{"models_dir","pulled":[{"id","bytes"}]}`, one entry per model fetched. It was not run here.

## Config

Keys are the short names, not the field names inside `settings.json`.

```bash
jotter config --json
jotter config get transcribe --json
jotter config set transcribe true --json
```

| Field | Meaning |
| --- | --- |
| `path` | The settings file. macOS: `~/Library/Application Support/Jotter/settings.json`. Linux: `${XDG_CONFIG_HOME:-~/.config}/jotter/settings.json`. |
| `settings.aec` | bool. Default `true`. Echo cancellation when a recording stops. |
| `settings.transcribe` | bool. Default `false`. Offline transcript when a recording stops. Not the live switch. |
| `settings.diarize` | bool. Default `false`. |
| `settings.speakers` | number, or `null` if unset. Diarization declines without it. |
| `settings.telemetry` | bool. Not a place for transcript text. |

`config get` and `config set` print `{"key","value"}`. Observed: `{"key":"transcribe","value":true}`.

Booleans accept `true`/`false`, `on`/`off`, `yes`/`no`, `1`/`0`. `speakers` accepts a count, or `none` to unset. A bad value is `invalid_argument` and changes nothing.

Set `transcribe` when the user asked to listen, so `stop` writes the accurate transcript. Leave `diarize` and `speakers` alone unless they asked who said what and gave a count. Live transcription does not read this setting.

## Start

```bash
jotter start --json
jotter start --json --no-live
jotter start --json --only both|mic|system --mic <ID> --system <ID> --out <DIR>
```

| Flag | Meaning |
| --- | --- |
| `--only` | `both` (default), `mic`, or `system`. |
| `--mic`, `--system` | Device id from `jotter devices --json`. Omit both to use the built-in mic and the default output. |
| `--out` | Recording directory. Default `~/Documents/Jotter/<YYYY-MM-DD_HH-MM-SS>/`. |
| `--no-live` | Do not transcribe while recording. |

Live is on unless `--no-live`. It is not gated on `config transcribe`.

Observed with live on, and with `--no-live`:

```json
{"session_id":"20260923T160740.116-49268","dir":"/private/tmp/jotter-skill-verify.11mA","pid":49269,"started_at":"2026-09-23T16:07:40.236Z","live":true}
```

| Field | Meaning |
| --- | --- |
| `session_id` | This session. One at a time. |
| `dir` | Recording directory. Keep it. |
| `pid` | Recorder pid. |
| `started_at` | RFC3339. |
| `live` | Bool. `true` unless `--no-live`. Not an object. |

## Status

```bash
jotter status --json
```

Idle, observed: `{"active":false}`. `session` and `stale` are omitted, not null, when they do not apply.

| Field | Meaning |
| --- | --- |
| `active` | `true` only while a live process owns the session. |
| `session.session_id`, `dir`, `pid`, `started_at` | Same as `start`. |
| `session.elapsed_secs` | Seconds since `started_at`. |
| `session.state` | `starting`, `recording`, `stopping`, `stopped`, `finishing`, or `failed`. |
| `session.live` | Null when the session was started with `--no-live`. Otherwise the object below. |
| `stale` | Last session's pid is dead. `session_id`, `dir`, `pid`, `state`. Not a recording in progress. |

`session.live` is exactly these three fields. It does not carry `kind`, `reason`, or `dropped_secs`.

| Field | Meaning |
| --- | --- |
| `state` | `starting` while `live.jsonl` is empty (the model may still be loading). `running` once a line exists. `declined` or `failed` only once `meta.json` has a live block that says so, which is after capture has finalised — not while the file is merely empty. |
| `segments` | Lines in `live.jsonl`. `0` while `starting`. |
| `last_end_secs` | End of the latest line, seconds on the mic timeline. Null when there are none. |

Observed just after `start` (live on):

```json
{"active":true,"session":{"session_id":"20260923T160740.116-49268","dir":"/private/tmp/jotter-skill-verify.11mA","pid":49269,"started_at":"2026-09-23T16:07:40.236Z","elapsed_secs":0.115,"state":"recording","live":{"state":"starting","segments":0,"last_end_secs":null}}}
```

`--no-live` observed `live: null` on that same object. `starting` with `segments` 0 means the transcript has not caught up. It does not mean live transcription failed.

## Context

```bash
jotter context --json
jotter context --since <CURSOR> --json
jotter context --last <SECONDS> --json
jotter context --dir <DIR> --json
jotter context --follow --json
```

Which recording: `--dir` if passed (even if a session is recording somewhere else), otherwise the active session, otherwise the newest recording under the recordings root.

| Flag | Meaning |
| --- | --- |
| `--since` | Only lines after this cursor. Pass the previous `cursor` back unchanged. Ignored when the final transcript is served — that file is the whole answer. A value that is not `live:<hex>` is `invalid_argument`. |
| `--last` | Segments whose `end` is within this many seconds of timeline now. While capturing, now is the session's elapsed time. Otherwise it is `meta.json`'s duration, or the latest segment end if there is no meta. A negative or non-finite value is `invalid_argument` (`--last=-1`, not `--last -1`). |
| `--dir` | That recording. `not_found` if it is not a directory. |
| `--follow` | One JSON object per new live segment until the session ends, then the process exits. No sentinel line. Needs an active session (`no_session` otherwise). A `--dir` that is not that session is `invalid_argument`. JSON even without `--json`. Do not use it unless the user asked to watch continuously. |

Success object:

| Field | Meaning |
| --- | --- |
| `dir` | Recording directory. |
| `source` | `"live"` or `"transcript"`. |
| `complete` | `true` only when `source` is `"transcript"`. |
| `cursor` | `live:<hex>` while `source` is `"live"`, including `live:0` before the first line. `null` when `source` is `"transcript"`. Do not invent one, and do not pass `null` to `--since`. |
| `segments` | Ordered speech. Empty means nothing new, or the transcript has not caught up, or `--last` fell in trailing silence. It does not mean the meeting was silent. |
| `segments[].track` | `"mic"` (the user) or `"system"` (everyone else). |
| `segments[].speaker` | Omitted, not null, until diarization labels a system segment. Mic segments stay unlabelled. |
| `segments[].start`, `end` | Seconds on the mic timeline from the start of the recording. |
| `segments[].text` | What the recogniser heard. Do not invent text that is not here. |

`source` is `"transcript"` and `complete` is true only when this directory is not still being captured and `transcript.json` exists. An old `transcript.json` in a directory that is being recorded into again does not hide the live lines. Otherwise `source` is `"live"` and `complete` is false, including when `live.jsonl` does not exist yet.

Observed during a just-started session:

```json
{"dir":"/private/tmp/jotter-skill-verify.11mA","source":"live","complete":false,"cursor":"live:0","segments":[]}
```

Observed on a finished directory that had `transcript.json` (`--since live:0` returned the same object; the cursor was ignored):

```json
{"dir":"/private/tmp/jotter-skill-verify.11mA","source":"transcript","complete":true,"cursor":null,"segments":[{"track":"mic","start":0.1,"end":0.4,"text":"synthetic check"}]}
```

A `--follow` line is `{cursor, track, start, end, text}` plus `speaker` when present. `cursor` is where that read ended, not a private offset for one segment: several lines from one read share it, and `--since` that value skips all of them.

## Stop

```bash
jotter stop --json
jotter stop --json --no-finish
```

`--no-finish` leaves the offline passes for later. Do not pass it unless the user asked. Without it, `stop` blocks on the passes that are enabled. That wait is the transcription, not the 60s recorder timeout. Transcription is skipped when `transcribe` is false. Diarization declines without a speaker count.

| Field | Meaning |
| --- | --- |
| `dir` | Recording directory. |
| `duration_secs` | Length of the recording. |
| `tracks.mic`, `tracks.system` | `null` if that source was not recorded. Otherwise `path`, `device`, `device_id`, `sample_rate`, `source_channels`, `frames`, `secs`, `stream_errors`. |
| `track_offset_secs` | Seconds the system track started after the mic. `null` unless both have audio. |
| `finish` | `null` with `--no-finish`. Otherwise `aec`, `transcribe`, `diarize`, each tagged with `status`. |
| `transcript_path` | The transcript just written, or `null`. Prefer `jotter context` over opening it. |

`finish` pass `status`: `ran` (report fields, including `declined`: `null` or `{kind, message}`), `skipped` (`reason` is `disabled`, `no_audio`, or `no_transcript`, plus `message`), or `failed` (`message`). A decline is a decision, not a crash.

Observed `stop --no-finish` (mic only, live session of a fraction of a second): `finish` null, `transcript_path` null, `tracks.system` null, `track_offset_secs` null.

After a successful `stop` the session file is gone. `status` is idle. `context --dir <dir>` still reads the recording.

## Recordings

```bash
jotter recordings --json
jotter recordings --limit <N> --json
```

`--limit` defaults to 10. Newest first. The list is one level under the recordings root (`~/Documents/Jotter`, or `$XDG_DOCUMENTS_DIR/Jotter` on Linux when that variable is absolute). The active session's directory is included even when `--out` put it outside that root, and even when it would otherwise fall past `--limit`.

| Field | Meaning |
| --- | --- |
| `recordings[].dir` | Recording directory. Pass to `context --dir`. |
| `recordings[].started_at` | RFC3339 from `meta.json`, or `null` until the recording has been finalised. The key is always present. |
| `recordings[].duration_secs` | Number, or `null` for the same reason. |
| `recordings[].artifacts` | Bool for each of `mic.wav`, `system.wav`, `live.jsonl`, `transcript.json`, `mic_aec.wav`. |

Observed (active session outside the recordings root, `--limit 1`, before `meta.json` existed):

```json
{"recordings":[{"dir":"/private/tmp/jotter-skill-verify.11mA","started_at":null,"duration_secs":null,"artifacts":{"mic.wav":true,"system.wav":false,"live.jsonl":false,"transcript.json":false,"mic_aec.wav":false}}]}
```

A finished recording under `~/Documents/Jotter` has a non-null `started_at` and `duration_secs`. `artifacts.transcript.json` is the file `context` will prefer.

## Devices

Use only to recover from `device_not_found`.

```bash
jotter devices --json
```

`{"devices":[{"name","id","direction","supports_input","supports_output","loopback","default_input","default_output"}],"loopback_available"}`. Pass `id` to `--mic` or `--system`. `loopback` false means do not use it as `--system`.

## What not to read

The session file (`session.json`, beside the settings file on macOS, or under `$XDG_RUNTIME_DIR/jotter` on Linux) is how the CLI's own processes talk. `status` is the interface. `live.jsonl` can end mid-line; `context` is the reader that knows a half-written line is not a line yet.
