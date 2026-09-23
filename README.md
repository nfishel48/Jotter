# Jotter
## jotter is a fully local recording, transcription, and semantic search tool

### jotter is still in early development but thr roadmap is as follows
- Start/stop recording from the terminal, or from any app that links the library.   
-  Meeting appears as a transcript with speakers and times.    
-  Search box: “refund policy”, “what Jane said about pricing”.   
-  Results show snippet + meeting + timestamp; click plays that moment.   
- Everything lives in one folder the user can back up    

---

## Using Jotter
Jotter tries to be simple for less technical users to use and still get the advantages of local only transcription and semantic search while still be less opinionated then then other tools and allowing those who want to change things.

Runs on macOS on Apple Silicon and on Linux x86_64 (PipeWire; compile-verified
so far). Intel Macs are deliberately not supported, and releases ship an arm64
`Jotter.app` only.

## Getting a transcript

Transcription needs a speech model, which is too large to ship inside the binary. Fetch it once:

```bash
jotter models pull      # ~675 MB, verified by checksum
jotter models list      # what is known, and what is ready
```

Default model is NVIDIA Parakeet TDT 0.6b v2 (English), run through [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx), which is linked statically. You may choose any model you want if you feel the need to switch.

Then either transcribe an existing recording:

```bash
jotter transcribe ~/Documents/Jotter/2026-09-15_14-32-08
```

or have every recording transcribed as it finishes with
`jotter config set transcribe true`, or for a single run:

```bash
jotter record --transcribe --duration 600
```

`jotter record` blocks until the recording ends. To record in the background
and get on with something else, `jotter start` launches a recorder and returns
at once; `jotter status` shows whether it is running and `jotter stop` ends it
and runs the same offline passes. On macOS the recorder runs inside
`Jotter.app`, which is what carries the microphone and system-audio
permissions.

That writes `transcript.json` beside the audio. Your microphone and everyone
else's audio are transcribed separately and merged onto one timeline, so each
segment already says whether it was you or the room:

```json
{ "start": 0.42, "end": 3.10, "track": "mic", "text": "morning all" }
```

## Who said it

`track` already separates you from everyone else, for free — that is the whole
point of recording two files. Telling apart the several people inside the
`system` track is a second pass:

```bash
jotter diarize ~/Documents/Jotter/2026-09-15_14-32-08 --speakers 4
```

That fills in a `speaker` on each system segment, in place, without
re-transcribing:

```json
{ "start": 3.20, "end": 8.04, "track": "system",
  "speaker": "speaker_01", "text": "morning, shall we start" }
```

Your own segments are deliberately left unlabelled: the microphone track is you,
and there is nothing to work out.

**You have to say how many people were on the call.** Jotter can ask the model to
count them instead, and it is not reliable enough to ship: on a clean recording
it is right, and on a thirty-six minute meeting of three people who talked over
each other it reported two hundred and eight speakers. A transcript that
confidently names two hundred and eight people is worse than one that names
none, so the number comes from you. Set it once as `diarize_speakers` in
`settings.json`, or pass `--speakers` per run.

Speaker identification is off by default (`diarize_enabled`, or `--diarize` for
one run) and needs two more models (~44 MB), fetched by the same `jotter models pull`.

## Settings

There is no settings window: Jotter is a command-line tool, and the few
preferences it keeps live in one JSON file. Change them with `jotter config`
rather than editing it by hand:

```bash
jotter config                       # show every setting
jotter config set transcribe true   # transcribe every recording
jotter config set speakers 4        # how many people are on your calls
```

- macOS — `~/Library/Application Support/Jotter/settings.json`
- Linux — `${XDG_CONFIG_HOME:-~/.config}/jotter/settings.json`

`jotter config` prints the exact path.

| Key | Default | What it does |
| --- | --- | --- |
| `aec_enabled` | `true` | Remove speaker echo from your mic track when a recording stops |
| `transcribe_enabled` | `false` | Transcribe every recording when it stops |
| `diarize_enabled` | `false` | Label the people in the system track after transcribing |
| `diarize_speakers` | `0` (not set) | How many people were on the call |
| `telemetry_enabled` | `true` | Anonymous usage and crash reports — see [docs/TELEMETRY.md](docs/TELEMETRY.md) |

Any of the first three can be overridden for a single `jotter record` with
`--aec`/`--no-aec`, `--transcribe`/`--no-transcribe` and
`--diarize`/`--no-diarize`. Recordings go to `~/Documents/Jotter/<timestamp>/`
unless you pass `--out`.

## Using Jotter as a library

Everything the `jotter` command does lives in the `jotter` library crate
(`crates/jotter`); the CLI in `crates/jotter-cli` is a thin layer over it. Another
Rust program can record, finish and read a transcript through the same code:

```toml
jotter = { git = "https://github.com/nfishel48/Jotter", default-features = false, features = ["aec", "transcribe"] }
```

Each processing stage is a cargo feature — `aec`, `transcribe`, `diarize` — so
an app pays only for the ones it uses. Telemetry is off in the library unless a
build asks for it, and a host app should leave it that way: it would report into
Jotter's own PostHog project, as Jotter. The crate documentation at the top of
[`crates/jotter/src/lib.rs`](crates/jotter/src/lib.rs) walks through record →
finish → read transcript.

## How accurate is it?

Measured, not asserted. [`benchmarks/`](benchmarks) runs Jotter over the corpora
the speech recognition field uses — LibriSpeech, AMI, TED-LIUM, Common Voice —
and scores them the way the published leaderboards do, so the figures can be set
beside anyone else's:

```bash
benchmarks/bootstrap.sh
cargo build --release -p jotter-cli --features bench
benchmarks/bench score --corpus librispeech-test-clean
```

It also measures the thing no single-stream tool can: AMI's per-speaker headsets
are rebuilt into real two-track Jotter recordings, so **speaker attribution** —
did each transcribed moment land on the right track? — becomes a number rather
than an argument.

Method, and what the numbers do not say, in
[docs/BENCHMARKS.md](docs/BENCHMARKS.md). Results in
[benchmarks/results/](benchmarks/results).

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for how the passes fit together,
[docs/AUDIO_CAPTURE.md](docs/AUDIO_CAPTURE.md) for the macOS permission story,
and [docs/TELEMETRY.md](docs/TELEMETRY.md) for exactly what is reported and how
to turn it off. No transcript text ever leaves the machine.
