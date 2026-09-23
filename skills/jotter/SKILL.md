---
name: jotter
description: Listens to a meeting with the local jotter CLI and pulls the transcript so the user does not have to re-explain it. Use when the user is in a meeting, wants the agent to hear what is being said, asks what was just decided, or asks to start, stop, or look up a recording.
compatibility: Requires the jotter command on PATH. Apple Silicon macOS needs build/Jotter.app from scripts/bundle.sh before jotter start can capture system audio. Linux x86_64 runs the binary directly.
---

# Listen to a meeting with jotter

Jotter is a local command. There is no window, tray, or settings pane. Drive it with `--json` and parse JSON. Do not scrape English, and do not read `live.jsonl`, `transcript.json`, or the session file yourself — the commands below are the interface.

Command lines, JSON fields, and error kinds: [references/commands.md](references/commands.md). Read that file before the first `jotter` invocation in a session, and again if a command's JSON does not match it. If `jotter context --help` says the subcommand is unrecognized, this `jotter` is older than the skill. Say so. Do not compensate by parsing files.

## Rules

- Put `--json` on every command. Success is one JSON object on stdout (a stream is one object per line). Failure is `{"error":{"kind":"...","message":"..."}}` on stdout and a non-zero exit. Branch on `error.kind`. The message is for the user.
- Never invent dialogue. If `segments` is empty, say the transcript has not caught up yet.
- Do not dump the transcript back unprompted. Answer from it.
- Do not pass `--follow` unless the user asked to watch continuously. Poll with `--since` instead, and keep the cursor the previous call returned. Do not invent one.
- One recording at a time. `session_active` means one is already running: use it. Do not start another.
- On macOS, `bundle_not_found` means there is no `Jotter.app`. Tell the user to run `scripts/bundle.sh` from the jotter repo. Do not retry `start`, and do not set `JOTTER_NO_BUNDLE` — outside the bundle the system track is silence, not a captured meeting.
- A recording is the user's voice and other people's. Do not upload `mic.wav`, `system.wav`, or `mic_aec.wav`. Do not put transcript text into telemetry, a commit, an issue, or any request that leaves the machine. Files live under `~/Documents/Jotter/` (on Linux, `$XDG_DOCUMENTS_DIR/Jotter` when that variable is an absolute path).

## Before the meeting

1. `jotter models list --json`. The default recogniser is `parakeet-tdt-0.6b-v2-int8`. Live transcription also needs `silero-vad`. If either has `"ready": false`, run `jotter models pull` (no id fetches the recogniser, the voice-activity model, and the two diarization models — about 675 MB). Tell the user, then wait. Do not start until both are `"ready": true`.
2. `jotter config get transcribe --json`. A useful listen needs `"value": true`: live lines are a rough draft, and `jotter stop` writes the accurate transcript only when this is on. If the user asked you to listen and it is false, `jotter config set transcribe true --json`. Do not change `telemetry`, `diarize`, or `speakers` unless they asked. Diarization needs a count they supply; do not guess one.
3. `jotter status --json`. If `active` is true, a recording is already running. Use that session.

## Start

`jotter start --json`

Live transcription is on unless you pass `--no-live`. Do not pass `--no-live` when the user wants you to hear the meeting. Keep `dir`.

If `start --help` still says live transcription is not available, this binary will record without a live transcript. Say so; do not pretend later `context` calls will fill in.

Other start failures are not fixed by retrying the same command. `capture_failed` is usually a denied microphone or system-audio permission for `Jotter.app`. `device_not_found` means the id is wrong — `jotter devices --json`, then retry once with a real id. `session_timeout`, `session_failed`, and `launch_failed`: report `error.message` and stop.

## While it runs

- What has been said: `jotter context --json`
- Only the new part: `jotter context --since <cursor> --json`. Save the returned `cursor`.
- A recent slice, when the user asks what was just decided: `jotter context --last <seconds> --json`
- A specific recording, not the active one: `jotter context --dir <dir> --json`
- Still recording, and how far live transcription has got: `jotter status --json`. Read `session.live` (`state`, `segments`, `last_end_secs`).

`source` is `"live"` during the meeting and `"transcript"` once the finished file exists. Trust `"transcript"`. Live lines lag, can drop a tail, and are a draft.

`track` `"mic"` is the user. `"system"` is everyone else. `speaker` is absent until diarization has labelled the system track; mic segments stay unlabelled on purpose.

If `segments` is empty, check status before saying nobody spoke. `live.state` of `declined` or `failed` means the live transcript will stay empty — say that, and the `kind` or `reason` if the object has one. Do not invent the missing words.

## When the meeting is over

`jotter stop --json`

That ends capture and runs the offline passes that are enabled: echo cancellation by default, transcription if `transcribe` is on, diarization only if `diarize` is on and `speakers` is set. It blocks until those passes finish. Do not kill it. Do not pass `--no-finish` unless the user asked to skip them.

Then `jotter context --json`. Prefer this over the live lines you saw during the meeting. `source` should be `"transcript"`. If it is still `"live"`, the offline pass did not write a transcript — say so, and do not present the draft as the final record.

`no_session` means nothing is recording. `session_busy` means another stop is already finishing it; wait, do not start a second one.

## An earlier meeting

`jotter recordings --json` lists recent recordings: start time, duration, and which artifacts exist. Then `jotter context --dir <dir> --json` for that one. `--limit N` shortens the list.

## Do not

- Do not use `jotter record` for a meeting you need to keep working through. It blocks until the recording ends.
- Do not parse human-format output. Help and version stay text even with `--json`; results and errors do not.
- Do not upload audio, and do not copy transcript text into telemetry.
