# jotter commands for an agent

**Check this file against the binary before trusting it.**

Verified on 2026-09-23 against `target/debug/jotter` built from `origin/trunk/cli-agent` at `1f7dbd6`:

- `jotter models list --json`, `jotter models path --json`, `jotter config --json`, `jotter config get transcribe --json`, `jotter status --json` (no session), `jotter stop --json` (no session), and a usage error.
- `jotter context` and `jotter recordings` are **not in that binary** (`unrecognized subcommand`, exit 2). Their flags and JSON fields below are the plan's contract, not observed output. Before trusting them, run `jotter context --help` and one `jotter context --json` (and the same for `recordings`) and correct this file if they differ.
- On that same binary, `jotter start --help` still says live transcription is not available and every session records without it. "Live is on unless `--no-live`" is the contract being landed with the context commands, not what `1f7dbd6` does. `start` was not run here: it would have opened the microphone.

`jotter start` was not invoked. Its success JSON is from `crates/jotter-cli/src/session/mod.rs` at that commit, not from a live capture.

## How to call it

`--json` is global. `jotter models list --json` and `jotter --json models list` are the same command.

| Outcome | Stdout | Exit |
| --- | --- | --- |
| Success | Exactly one JSON object, one line. No prose before or after it. | 0 |
| `--follow` | One JSON object per line, until the session ends. | 0 when the stream ends cleanly |
| Failure with `--json` | `{"error":{"kind":"<snake_case>","message":"..."}}` | non-zero |
| Help or version | clap's own text, even if `--json` is present. Not an error. | 0 |

Branch on `error.kind`. Do not scrape `message`, and do not branch on the exit code beyond "non-zero means it failed". A command line that does not parse is kind `usage` and exit 2. Other failures exit 1.

Observed:

```json
{"error":{"kind":"usage","message":"error: unexpected argument '--nope' found\n\nUsage: jotter status [OPTIONS]\n\nFor more information, try '--help'.\n"}}
```

```json
{"error":{"kind":"no_session","message":"no session is recording"}}
```

There is no interactive prompt when `--json` is set, or when stdin is not a terminal. Do not wait for one.

## Error kinds

From `ErrorKind` at `1f7dbd6`. These strings are the interface. New kinds may be added; none of these will be renamed.

| `kind` | When | What to do |
| --- | --- | --- |
| `usage` | The command line did not parse. | Fix the invocation. Do not retry it unchanged. |
| `invalid_argument` | Parsed, but the value is unacceptable (speaker count, unknown config key). | Fix the value. |
| `unknown_model` | `models pull` was given an id not in the catalogue. | `jotter models list --json` and use a listed `id`. |
| `device_not_found` | The device id does not exist, or there is no device. | `jotter devices --json`, then retry once. |
| `capture_failed` | The device was found but capture could not start or stop. Usual shape of a denied permission. | Tell the user. On macOS they grant microphone and system audio to `Jotter.app`. Do not loop. |
| `stage_failed` | Echo cancellation, transcription, or diarization hit an error. | Report `message`. The recording is still on disk. |
| `download_failed` | A model download failed or did not verify. | Report `message`. Do not start the meeting without a ready model. |
| `io` | Reading or writing a file failed. The first recording can also be Documents-folder access denied. | Report `message`. Do not loop. |
| `session_active` | `start` while a session is already recording. | Use it: `status`, then `context`. Do not start another. |
| `session_busy` | `stop` while another `stop` is already finishing this session. | Wait. Do not start a second `stop`. |
| `no_session` | `stop` with nothing recording. A dead previous session is also this kind; `message` says it is stale. | Do not invent a recording. `recordings` can still find the directory. |
| `session_failed` | The recorder exited without starting, or without finalising. | Report `message`. Do not immediately `start` again unless the user asks. |
| `session_timeout` | The recorder did not report that it started within 30s, or did not finalise the WAV headers within 60s of `stop`. The offline passes run after that 60s and can take as long as the meeting; this kind is not "transcription was slow". | Report `message`. Do not retry in a loop. |
| `bundle_not_found` | macOS: no `Jotter.app` to run the recorder in. | Tell the user to run `scripts/bundle.sh`. Do not retry. Do not set `JOTTER_NO_BUNDLE`. |
| `launch_failed` | The recorder process could not be launched at all. | Report `message`. Do not retry in a loop. |

`context` and `recordings` may add kinds. They must not rename the ones above. If a new kind appears, treat it like the others: report `message`, do not scrape it, do not loop.

## Models

Verified.

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
| `models[].engine` | Always `sherpa-onnx` for the shipped catalogue. |
| `models[].description` | Human label. Do not branch on it. |
| `models[].bytes` | Download size. |
| `models[].ready` | `true` only when every file is present and the right size. |
| `models[].missing` | Files missing or the wrong size. `0` when `ready` is true. |

The default recogniser is `parakeet-tdt-0.6b-v2-int8`. Live and offline transcription also need `silero-vad`. Pull if either is not ready. `models pull` with no id also fetches `pyannote-segmentation-3-0` and `nemo-en-titanet-small`; that is fine, and it does not turn diarization on.

`models pull` was not run here (it downloads hundreds of megabytes). The shape, from source, is `models_dir` plus `pulled[]` of `{id, bytes}` — one entry per model fetched, four when no `--model` is given:

```json
{"models_dir":"…","pulled":[{"id":"parakeet-tdt-0.6b-v2-int8","bytes":661190513}]}
```

## Config

Verified. Keys are the short names, not the field names inside `settings.json`.

```bash
jotter config --json
jotter config get transcribe --json
jotter config set transcribe true --json
```

`config` with no subcommand:

| Field | Meaning |
| --- | --- |
| `path` | The settings file. macOS: `~/Library/Application Support/Jotter/settings.json`. Linux: `${XDG_CONFIG_HOME:-~/.config}/jotter/settings.json`. |
| `settings.aec` | bool. Default `true`. Echo cancellation when a recording stops. |
| `settings.transcribe` | bool. Default `false`. Offline transcript when a recording stops. |
| `settings.diarize` | bool. Default `false`. Label system-track speakers after transcription. |
| `settings.speakers` | number, or `null` if unset. Diarization declines without it. |
| `settings.telemetry` | bool. Anonymous usage. Not a place for transcript text. |

`config get` and `config set` both print `{"key":"<name>","value":...}`. A flag's value is `true` or `false`. `speakers` is a number or `null`. Observed: `{"key":"transcribe","value":true}`.

Booleans accept `true`/`false`, `on`/`off`, `yes`/`no`, `1`/`0`. `speakers` accepts a count, or `none` to unset. A bad value is `invalid_argument` and changes nothing.

`transcribe` is the offline pass. It is not the live-transcription switch. Set it when the user asked to listen, because `stop` only writes the accurate transcript when it is on. Leave `diarize` and `speakers` alone unless the user asked who said what and gave a count.

## Start

Help text verified. Success JSON is from source at `1f7dbd6`, not from a capture.

```bash
jotter start --json
jotter start --json --no-live
jotter start --json --only both|mic|system --mic <ID> --system <ID> --out <DIR>
```

| Flag | Meaning |
| --- | --- |
| `--only` | `both` (default), `mic`, or `system`. The two macOS permissions are separate. |
| `--mic`, `--system` | Device id from `jotter devices --json`. Omit both to use the built-in mic and the default output. |
| `--out` | Recording directory. Default `~/Documents/Jotter/<YYYY-MM-DD_HH-MM-SS>/`. Must be absolute once it reaches the bundle; the CLI makes a relative path absolute. |
| `--no-live` | Do not transcribe while recording. |

Contract, once the context work has landed: live is on unless `--no-live`. This binary does not do that yet — it accepts `--no-live` and then records with live off anyway.

Success object (source):

| Field | Meaning |
| --- | --- |
| `session_id` | Id of this session. One at a time. |
| `dir` | Recording directory. Keep it. |
| `pid` | Recorder pid. |
| `started_at` | RFC3339 timestamp. |
| `live` | Whether live transcription was requested. Bool in this source. Confirm against a live `--json` run after the context work lands; do not assume a richer object. |

`session_active` and `bundle_not_found`: see the table above. Do not retry those.

## Status

Idle result verified: `{"active":false}`. The active object is from source, plus the live fields the plan adds. `session` and `stale` are omitted, not null, when they do not apply.

```bash
jotter status --json
```

| Field | Meaning |
| --- | --- |
| `active` | `true` only while a live process owns the session. A dead pid is not active. |
| `session` | Present only when `active` is true. |
| `session.session_id`, `dir`, `pid`, `started_at` | Same meanings as `start`. |
| `session.elapsed_secs` | Seconds since `started_at`. |
| `session.state` | `starting`, `recording`, `stopping`, `stopped`, `finishing`, or `failed`. This field is in the source and is not in the plan's short list; keep reading it. |
| `session.live` | `null` on `1f7dbd6`. The plan's object is below. |
| `stale` | Present when the last session's pid is dead. `session_id`, `dir`, `pid`, `state`. Not a recording in progress. |

Plan's `session.live`, not yet emitted by this binary:

| Field | Meaning |
| --- | --- |
| `state` | `starting` (model still loading), `running`, `declined`, or `failed`. |
| `segments` | Lines written so far. |
| `last_end_secs` | Where the latest line ended, in seconds on the mic timeline. Absent or null when nothing has been written. |

The library status also carries `kind`, `reason`, and `dropped_secs`. If a live run includes them, use them: `kind` is why a `declined` or `failed` state happened (`model_missing`, `unavailable`, `no_tracks`, `engine`, `io`). Do not require them until a `--json` run shows them. `dropped_secs` greater than zero means the live transcript has a gap; the WAV files do not.

`declined` and `failed` do not stop the recording. They do mean `context` will not grow new live lines.

## Context

**Not in the binary at `1f7dbd6`.** Flags and fields are the plan's contract.

```bash
jotter context --json
jotter context --since <CURSOR> --json
jotter context --last <SECONDS> --json
jotter context --dir <DIR> --json
jotter context --follow --json
```

Which recording: the active session, otherwise `--dir`, otherwise the most recent recording under the recordings root.

| Flag | Meaning |
| --- | --- |
| `--since` | Only what was appended after this cursor. Pass the `cursor` from the previous call back unchanged. |
| `--last` | Only the recent part of the timeline, in seconds. The exact window (which timestamp, relative to now or to the end of the file) was not observable. Trust `--help` if it disagrees with "the last N seconds of speech". |
| `--dir` | A recording directory, not the recordings root. |
| `--follow` | Stream one JSON object per new segment until the session ends. Do not use it unless the user asked to watch continuously. The exact per-line shape was not observable; do not assume it matches the object below until a live run shows it. |

Planned success object:

| Field | Meaning |
| --- | --- |
| `dir` | Recording directory. |
| `source` | `"live"` or `"transcript"`. `"transcript"` means `transcript.json` was preferred. That file is more accurate than the live lines: it is written after echo cancellation, with the whole meeting. |
| `complete` | Planned flag for "this is the finished read". Treat `source: "transcript"` as the record to trust even if you also check `complete`. Confirm the flag's exact meaning against a live run. |
| `cursor` | Opaque. Print it and pass it to the next `--since`. The library spelling is `live:<hex offset>`. Do not compute an offset. A cursor that is not the one you were given is `usage` or an I/O error, not something to repair by editing the hex. |
| `segments` | Ordered speech. Empty means nothing new, or the transcript has not caught up. It does not mean the meeting was silent. |
| `segments[].track` | `"mic"` (the user) or `"system"` (everyone else). |
| `segments[].speaker` | Optional. Omitted until diarization labels a system segment. Mic segments stay unlabelled: that track is the user. |
| `segments[].start`, `end` | Seconds on the **mic** timeline from the start of the recording. The same clock as `transcript.json`. |
| `segments[].text` | What the recogniser heard. Not a guarantee. Do not invent text that is not here. |

A missing `live.jsonl` on a recording that just started is an empty chunk, not an error. A cursor past the end of the file is an error; take a fresh `context` without `--since` rather than guessing an offset.

## Stop

The no-session error was run. The success object is from source (`RecordingJson`), not from stopping a real session.

```bash
jotter stop --json
jotter stop --json --no-finish
```

`--no-finish` leaves the offline passes for later. Do not pass it unless the user asked. Without it, `stop` blocks until echo cancellation, transcription, and diarization have each run or declined, according to settings. That wait is the transcription, not the 60s recorder timeout. Transcription declines when `transcribe` is false (`finish.transcribe.status` is `skipped`, reason `disabled`). Diarization declines without a speaker count.

| Field | Meaning |
| --- | --- |
| `dir` | Recording directory. |
| `duration_secs` | Length of the recording. |
| `tracks.mic`, `tracks.system` | `null` if that source was not recorded. Otherwise `path`, `device`, `device_id`, `sample_rate`, `source_channels`, `frames`, `secs`, `stream_errors`. Non-zero `stream_errors` means the track may be cut short. |
| `track_offset_secs` | Seconds the system track started after the mic track. `null` unless both tracks have audio. |
| `finish` | `null` with `--no-finish`. Otherwise one key per pass this build contains: `aec`, `transcribe`, `diarize`. |
| `transcript_path` | The transcript just written, or `null`. Prefer `jotter context` over opening this path. A later re-run can clear the path stored in `meta.json` while `transcript.json` is still the file to read; `context` is what knows that. |

Each pass in `finish` is tagged with `status`:

| `status` | Fields beside it |
| --- | --- |
| `ran` | That pass's report, including `declined` (`null`, or `{kind, message}`). A decline is a decision, not a crash. `model_missing` means pull the model and the recording can be transcribed later with `jotter transcribe <dir>`. |
| `skipped` | `reason` (`disabled`, `no_audio`, or `no_transcript`) and `message`. |
| `failed` | `message`. The recording is kept. |


## Recordings

**Not in the binary at `1f7dbd6`.** The plan does not name the JSON keys.

```bash
jotter recordings --json
jotter recordings --limit <N> --json
```

What it must return: recent recordings, each with a start time, a duration, and which artifacts exist. `--limit` caps how many. The default order is most recent first, matching "otherwise the most recent recording" on `context`.

Do not assume key names. Run `jotter recordings --help` and one `--json` invocation, and use the keys that run returns. Then `jotter context --dir <that dir> --json`.

Artifacts a recording directory can contain, so a listing can be checked against the disk if the JSON is unclear:

| File | When it exists |
| --- | --- |
| `mic.wav` | Microphone was recorded. |
| `system.wav` | System audio was recorded. |
| `meta.json` | Always, once capture finalised. |
| `live.jsonl` | Live transcription wrote at least one line. |
| `transcript.json` | Offline transcription wrote a transcript. This is the file `context` prefers. |
| `mic_aec.wav` | Echo cancellation wrote a cancelled mic track. |

The recordings root is `~/Documents/Jotter` (Linux: `$XDG_DOCUMENTS_DIR/Jotter` when that variable is absolute). Directory names are local time, `YYYY-MM-DD_HH-MM-SS`.

## Devices

Shape verified (`devices` and `loopback_available`). Use it only to recover from `device_not_found`, not as a step in a normal meeting.

```bash
jotter devices --json
```

| Field | Meaning |
| --- | --- |
| `loopback_available` | Whether any device can be tapped for system audio. |
| `devices[].id` | Pass to `--mic` or `--system`. May be null. |
| `devices[].name` | Human label. |
| `devices[].direction` | `input`, `output`, `duplex`, or `unknown`. |
| `devices[].loopback` | Safe to use as `--system`. A duplex device is not: it would record its own microphone. |
| `devices[].default_input`, `default_output` | What `start` uses when the matching flag is omitted. |

## What not to read

The session file (`session.json`, beside the settings file on macOS, or under `$XDG_RUNTIME_DIR/jotter` on Linux) is how the CLI's own processes talk. `status` is the interface. `live.jsonl` is append-only and can end mid-line; `context` is the reader that knows a half-written line is not a line yet.
