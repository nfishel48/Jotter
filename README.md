# Jotter
## jotter is a fully local recording, transcription, and semantic search tool

### jotter is still in early development but thr roadmap is as follows
- Start/stop recording from a menu bar or floating window.   
-  Meeting appears as a transcript with speakers and times.    
-  Search box: “refund policy”, “what Jane said about pricing”.   
-  Results show snippet + meeting + timestamp; click plays that moment.   
- Everything lives in one folder the user can back up    

---

## Using Jotter
Jotter tries to be simple for less technical users to use and still get the advantages of local only transcription and semantic search while still be less opinionated then then other tools and allowing those who want to change things.

## Getting a transcript

Transcription needs a speech model, which is too large to ship inside the binary. Fetch it once:

```bash
jotter models pull      # ~675 MB, verified by checksum
jotter models list      # what is known, and what is ready
```

Default model is NVIDIA Parakeet TDT 0.6b v2 (English), run through [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx), which is linked statically. You may choose any model you want if you feel the need to switch.

Then either transcribe an existing recording:

```bash
jotter transcribe recordings/2026-09-15_14-32-08
```

or have every recording transcribed as it finishes tick **Transcribe
recordings when they finish** in the settings pane, or for a single run:

```bash
jotter record --transcribe --duration 600
```

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
jotter diarize recordings/2026-09-15_14-32-08 --speakers 4
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
none, so the number comes from you. Set it once in the settings pane, or pass
`--speakers` per run.

Speaker identification is off by default and needs two more models (~44 MB),
fetched by the same `jotter models pull`.

## How accurate is it?

Measured, not asserted. [`benchmarks/`](benchmarks) runs Jotter over the corpora
the speech recognition field uses — LibriSpeech, AMI, TED-LIUM, Common Voice —
and scores them the way the published leaderboards do, so the figures can be set
beside anyone else's:

```bash
benchmarks/bootstrap.sh
cargo build --release --features bench
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
