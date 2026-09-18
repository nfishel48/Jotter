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
jotter models pull      # ~630 MB, verified by checksum
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

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for how the passes fit together,
[docs/AUDIO_CAPTURE.md](docs/AUDIO_CAPTURE.md) for the macOS permission story,
and [docs/TELEMETRY.md](docs/TELEMETRY.md) for exactly what is reported and how
to turn it off. No transcript text ever leaves the machine.
