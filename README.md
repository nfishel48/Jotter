# Jotter

**Let your AI agent hear your meetings, without the audio leaving your machine.**

Jotter records your microphone and the computer's audio as two separate tracks,
transcribes them on-device while the meeting is still going, and serves what has
been said through a command-line interface built for programs. An agent working
beside you, such as Claude Code, can start a recording, ask what was just decided,
and read the final transcript afterwards. You don't have to explain the meeting
to it again.

There is no window, tray, or settings pane. Jotter is the `jotter` command and
the Rust library it is built on. Speech recognition, echo cancellation, and
speaker labelling all run locally. No audio or transcript text is ever sent
anywhere.

## What it does

- **Records two tracks.** `mic.wav` is you and `system.wav` is everyone else, so
  every line of the transcript already says which side of the call it came from.
- **Transcribes live.** A draft transcript builds up a second or two behind the
  room. When the recording stops, a more accurate offline pass replaces it.
- **Works well with agents.** Every command accepts `--json`, prints one JSON
  object, and reports errors with a stable `kind`. An
  [Agent Skill](skills/jotter/SKILL.md) teaches an agent the whole procedure.
- **Labels speakers** on the system track, if you tell it how many people were
  on the call.
- **Removes echo** from your mic track, so the room's audio coming out of your
  speakers doesn't show up twice.
- **Keeps everything in one folder.** Each recording is a directory under
  `~/Documents/Jotter/` that you can back up.

Not built yet: searching across old meetings by meaning. Right now Jotter reads
one recording at a time.

## Install

Jotter runs on Apple Silicon macOS and on Linux x86_64 with PipeWire (Debian 12,
Ubuntu 24.04, Fedora, or newer). The Linux build compiles, but it hasn't been
tested much yet. Intel Macs are not supported, and that is a deliberate choice.

**Debian, Ubuntu (apt):**

```bash
sudo curl -fsSLo /usr/share/keyrings/jotter.asc https://nfishel48.github.io/Jotter/jotter.asc
sudo curl -fsSLo /etc/apt/sources.list.d/jotter.sources https://nfishel48.github.io/Jotter/jotter.sources
sudo apt update && sudo apt install jotter
```

**Fedora (dnf):**

```bash
sudo curl -fsSLo /etc/yum.repos.d/jotter.repo https://nfishel48.github.io/Jotter/jotter.repo
sudo dnf install jotter
```

**From a release:** each [GitHub release](https://github.com/nfishel48/Jotter/releases)
has `Jotter-X.Y.Z-macos-arm64.zip` (containing `Jotter.app`), a `.deb`, an
`.rpm`, and `jotter-X.Y.Z-linux-x86_64.tar.gz` with the bare binary. On Linux it
needs `libpipewire-0.3` and `libasound2` at runtime. The macOS app is not
notarized yet, so a downloaded `Jotter.app` needs
`xattr -dr com.apple.quarantine Jotter.app` before macOS will open it.

**From source:**

```bash
cargo build --release -p jotter-cli
scripts/bundle.sh          # macOS only: wraps the binary in build/Jotter.app
```

### Why macOS needs `Jotter.app`

macOS grants microphone and system-audio permission to an app bundle, not to a
bare binary. A recorder started straight from a terminal gets no error from
macOS: it just records silence on the system track. So on macOS `jotter start`
launches the actual recorder inside `Jotter.app`. The app has no window and no
Dock icon; it exists to hold the permissions. `jotter` looks for the bundle in
this order:

1. Around its own binary. Symlink `Jotter.app/Contents/MacOS/jotter` onto your
   `PATH` and this just works.
2. At `$JOTTER_APP`.
3. At `build/Jotter.app` in any directory above the binary, which is where
   `scripts/bundle.sh` puts it.

The first recording will ask for microphone and system-audio access for
`Jotter.app`. [docs/AUDIO_CAPTURE.md](docs/AUDIO_CAPTURE.md) explains the whole
permission setup.

### Models

The speech models are too large to ship inside the binary. Fetch them once:

```bash
jotter models pull      # recogniser, voice activity, diarization: ~675 MB, checksummed
jotter models list      # what is known, and what is ready
```

The default recogniser is NVIDIA Parakeet TDT 0.6b v2 (English). It runs through
[sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx), which is linked
statically.

## Letting an agent listen

Point your agent at [`skills/jotter/`](skills/jotter). It is an Agent Skills
package, so you can copy or symlink it
into your agent's skills directory. The skill covers when to start a recording,
how to poll for new lines, what each error kind means, and what never to do
with the audio.

Under the hood, the agent runs a loop like this:

```bash
jotter start --json                    # returns at once, recording in the background
jotter context --json                  # everything said so far, plus a cursor
jotter context --since live:1a4 --json # only lines after that cursor
jotter context --last 30 --json        # the last 30 seconds
jotter status --json                   # still recording? how far has live got?
jotter stop --json                     # end capture, run the offline passes
jotter context --json                  # now the accurate transcript
jotter recordings --json               # earlier meetings, newest first
```

`context` reads the active session. With `--dir` it reads another recording.
With neither, it reads the most recent recording. While audio is still being
captured it serves the live draft (`"source": "live"`). Once the offline
transcript exists it serves that instead (`"source": "transcript"`,
`"complete": true`).

```json
{"dir":"…/2026-09-23_16-07-40","source":"live","complete":false,"cursor":"live:1a4",
 "segments":[{"track":"system","start":3.2,"end":8.04,"text":"morning, shall we start"}]}
```

Errors go to stdout as `{"error":{"kind":"…","message":"…"}}` with a non-zero
exit, so programs branch on `kind` and never have to parse English. The contract
is in [docs/AGENT.md](docs/AGENT.md). Every command, JSON field, and error kind
is listed in
[skills/jotter/references/commands.md](skills/jotter/references/commands.md).

## Using it yourself

The same commands work without `--json`, and print text for a person to read.

```bash
jotter start                 # record in the background
jotter stop                  # stop, then run the enabled offline passes
jotter record --duration 600 # or record in the foreground for ten minutes
jotter record                # ...or until you press Enter
```

To transcribe a recording that was made without transcription turned on:

```bash
jotter transcribe ~/Documents/Jotter/2026-09-15_14-32-08
```

Each recording directory holds `mic.wav`, `system.wav`, and `meta.json`. It can
also hold `mic_aec.wav` (the echo-cancelled mic), `live.jsonl` (the live draft),
and `transcript.json` (the final transcript). Your track and everyone else's are
transcribed separately and merged onto one timeline:

```json
{ "start": 0.42, "end": 3.10, "track": "mic", "text": "morning all" }
```

## Who said it

The `track` field already separates you from everyone else, and that costs
nothing: it is the reason Jotter records two files in the first place. Telling
apart the several people inside the `system` track takes a second pass:

```bash
jotter diarize ~/Documents/Jotter/2026-09-15_14-32-08 --speakers 4
```

This adds a `speaker` to each system segment. It edits the transcript in place
and doesn't re-transcribe anything. Your own segments stay unlabelled, because
the mic track is always you.

**You have to say how many people were on the call.** Jotter can ask the model
to count them, but the count isn't reliable enough to ship. On a clean recording
it is right. On a thirty-six-minute meeting of three people who talked over each
other, it reported two hundred and eight speakers. A transcript that confidently
names 208 people is worse than one that names none, so the number comes from
you. Set it once with `jotter config set speakers 4`, or pass `--speakers` on
each run.

## Settings

Preferences live in one JSON file. Use `jotter config` to change them instead of
editing the file by hand:

```bash
jotter config                       # show every setting and where the file is
jotter config set transcribe true   # write a final transcript when a recording stops
jotter config set speakers 4        # how many people are on your calls
```

| Key | Default | What it does |
| --- | --- | --- |
| `aec` | `true` | Remove speaker echo from your mic track when a recording stops |
| `transcribe` | `false` | Write the final transcript when a recording stops. Live transcription doesn't depend on this; turn it off with `start --no-live` |
| `diarize` | `false` | Label the people in the system track after transcribing |
| `speakers` | unset | How many people were on the call. Diarization won't run without it |
| `telemetry` | `true` | Anonymous usage and crash reports. See [docs/TELEMETRY.md](docs/TELEMETRY.md) |

`jotter record` can override the first three for a single run with
`--aec`/`--no-aec`, `--transcribe`/`--no-transcribe`, and
`--diarize`/`--no-diarize`. The settings file is at
`~/Library/Application Support/Jotter/settings.json` on macOS and
`${XDG_CONFIG_HOME:-~/.config}/jotter/settings.json` on Linux.

## Using Jotter as a library

Everything the command does is implemented in the `jotter` library crate
(`crates/jotter`). The CLI in `crates/jotter-cli` is a thin layer over it, with
no private way into audio capture. That means another Rust program can record a
meeting, finish it, and read the transcript using the same code:

```toml
jotter = { git = "https://github.com/nfishel48/Jotter", default-features = false, features = ["aec", "transcribe"] }
```

Each processing stage is a cargo feature (`aec`, `transcribe`, `diarize`), so an
app pays only for the stages it uses. Telemetry is off in the library unless a
build asks for it. A host app should leave it off, because turning it on would
report into Jotter's own PostHog project under Jotter's name. The crate
documentation in [`crates/jotter/src/lib.rs`](crates/jotter/src/lib.rs) walks
through recording, finishing, and reading a transcript.

## How accurate is it?

The accuracy is measured, not asserted. [`benchmarks/`](benchmarks) runs Jotter
over the corpora the speech recognition field uses (LibriSpeech, AMI, TED-LIUM,
and Common Voice) and scores the results the same way the published leaderboards
do. That way the figures can be compared directly with anyone else's:

```bash
benchmarks/bootstrap.sh
cargo build --release -p jotter-cli --features bench
benchmarks/bench score --corpus librispeech-test-clean
```

The benchmarks also measure something a single-stream tool can't: speaker
attribution. AMI's per-speaker headset recordings are rebuilt into real
two-track Jotter recordings, so whether each transcribed moment landed on the
right track becomes a number you can check. The method, and what the numbers
don't tell you, are in [docs/BENCHMARKS.md](docs/BENCHMARKS.md). Results are in
[benchmarks/results/](benchmarks/results).

## More

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): how the passes fit together
- [docs/AUDIO_CAPTURE.md](docs/AUDIO_CAPTURE.md): capture internals and the macOS permission setup
- [docs/AGENT.md](docs/AGENT.md): the contract for programs driving `jotter`
- [docs/TELEMETRY.md](docs/TELEMETRY.md): exactly what is reported, and how to turn it off

## License

MIT. See [LICENSE](LICENSE).
