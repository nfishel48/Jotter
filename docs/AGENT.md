# Driving Jotter from an agent

Jotter has no window and no tray. An agent drives the `jotter` command, not a GUI and not the files under a recording directory.

The procedure — when to start, how to pull what has been said, what not to do with the audio — is the Agent Skill at [`skills/jotter/SKILL.md`](../skills/jotter/SKILL.md). Command lines, JSON fields, and error kinds are in [`skills/jotter/references/commands.md`](../skills/jotter/references/commands.md). This page is the contract those files follow. If the two disagree, trust a live `jotter <command> --help` and one `--json` run over either document, and fix the skill.

## Contract

- `--json` on every call. One JSON object on stdout, or one object per line for a stream. Errors are `{"error":{"kind":"<snake_case>","message":"..."}}` on stdout, with a non-zero exit. Branch on `kind`. Do not scrape English.
- A meeting the agent must keep working through is `jotter start`, then `jotter context`, then `jotter stop`. `jotter record` blocks until the recording ends; it is the human-oriented command.
- Live transcription is on unless `start --no-live`, and that bool is not gated on `config transcribe`. `jotter context` reads the active session, or `--dir`, or the most recent recording. It serves `transcript.json` (`source` `"transcript"`, `complete` true, `cursor` null) only once capture has finished and that file exists; otherwise it serves the live lines. `jotter recordings` lists recent recordings under `~/Documents/Jotter`, plus the active session if its directory is outside that root.
- If `context` or `recordings` is an unrecognized subcommand, the installed `jotter` is older than this skill. Say so. Do not parse `live.jsonl` to compensate.

## Privacy

A recording is the user's voice and other people's. It lives under `~/Documents/Jotter/` (on Linux, `$XDG_DOCUMENTS_DIR/Jotter` when that variable is an absolute path). Do not upload the WAV files. Do not put transcript text into telemetry, a commit, an issue, or any request that leaves the machine. Jotter's own telemetry already refuses transcript text; an agent must not add it back.

## Platforms

Apple Silicon macOS and Linux x86_64. On macOS the recorder has to run inside `Jotter.app` or system audio is silence. If `start` returns `bundle_not_found`, the user runs `scripts/bundle.sh`. Do not set `JOTTER_NO_BUNDLE` to skip that.
