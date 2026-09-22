# Measuring Jotter's accuracy

Jotter's pitch is that a fully local pipeline is good enough to transcribe your
meetings and let you search them by meaning. That is a claim about accuracy, and
until there is a number attached it is just a claim.

This describes how that number is produced: what is measured, on what, with what
scoring, and — as much as anything — what the numbers do *not* say.

The harness lives in [`benchmarks/`](../benchmarks). It is developer tooling.
Nothing in it is compiled into the shipped binary, and the `bench` cargo feature
that builds its driver is off by default.

---

## What is measured

Four things, because "accuracy" means four different things here and reporting
one of them as though it were the others is how benchmarks mislead.

### 1. Word error rate

The headline. Errors per reference word on standard corpora, scored the way the
[Open ASR Leaderboard](https://huggingface.co/spaces/hf-audio/open_asr_leaderboard)
scores them so the figures can be set beside published ones for Parakeet,
Whisper and Canary.

Four corpora, chosen to disagree with each other:

| corpus | what it is | licence |
| --- | --- | --- |
| LibriSpeech test-clean / test-other | read audiobooks; the universal reference | CC BY 4.0 |
| AMI (IHM) | real meetings, headset mics | CC BY 4.0 |
| TED-LIUM 3 | prepared talks | CC BY-NC-ND 3.0 — **non-commercial** |
| Common Voice (en) | crowdsourced; accent and microphone diversity | CC0 1.0 |

LibriSpeech is the easy end and doubles as the harness's own acceptance test: a
figure far from the published one means the harness is broken, not the model.
AMI is the one that matters, because it is the only one that looks like a
meeting.

TED-LIUM's licence forbids derivatives and commercial use. Measuring with it is
fine; redistributing it, or anything derived from it, is not — which is why it
is opt-in rather than part of the default set.

### 2. Two-track meeting accuracy

The measurement no single-stream tool can make.

Jotter records two files: `mic.wav` is you, `system.wav` is everyone else, and
"was this me?" is answered by which file the audio came from rather than by a
diarization model ([ARCHITECTURE.md](ARCHITECTURE.md)). AMI gives one headset
microphone per participant, which is enough to build exactly that situation:
one participant becomes the microphone, the rest become system audio, and the
others' speech is pushed through a synthetic room and mixed back into the mic
track as speaker bleed.

The full pipeline then runs — echo cancellation, then transcription — and three
things are scored: word error rate per track, **speaker attribution** (for every
10 ms where exactly one side is genuinely speaking, did the transcript put it on
the right track?), and what the echo canceller bought, by running the same
meetings again with it skipped.

**This does not measure diarization, and cannot be made to.** "Speaker
attribution" here means mic-versus-system — which of the two tracks a segment
landed on — not which of several people inside the system track said it. The
distinction matters because the fixture is actively hostile to the second
question: AMI is a co-located meeting recorded on headsets, so every headset
picks up every participant, and mixing three of them produces audio where all
three voices are present at once essentially throughout. Diarizing it returns
somewhere between 85 and 208 speakers for a meeting of three, depending on the
embedding model, and no clustering threshold rescues it. Real system audio is
the opposite case — each remote participant arrives as a separately encoded
stream with no acoustic path between them — so this fixture is pessimistic in a
way that says nothing useful. Scoring diarization needs a synthesis where the
system track is built from speakers who were never in the same room.

Attribution excludes overlapped speech. When two people talk at once there is no
single correct track, and counting it would measure AMI's overlap rate as much
as Jotter.

### 3. Echo cancellation

`Cargo.toml` justifies the AEC3 dependency with figures from one reference
recording. This generalises that to a swept test set where the ground truth is
known by construction.

Each item is three regimes back to back, matching what `process::classify` sorts
frames into: **near only** (you), **far only** (the room), **double talk**
(both). Those give the three figures `AecStats` reports separately — `erle_db`
from far-only frames, `near_gain_db` from near-only frames (how much of *your*
voice the canceller ate, the failure ERLE cannot see), and
`double_talk_gain_db`.

Echo return loss — how far the bleed sits below your voice — is swept from 0 to
24 dB, so the result says where cancellation stops working rather than asserting
that it works. Every item is transcribed with and without the pass, because a
canceller that adds 20 dB of ERLE and nothing to the transcript has not helped.

### 4. Speed

Real-time factor is recorded on every run. It is not accuracy, but an accuracy
figure with no cost attached is half an answer: this runs on a laptop after a
meeting, and "how long do I wait" is a real question.

---

## How the scoring works

### Normalisation is the whole ballgame

"Mr. Smith paid $5" against "mister smith paid five dollars" is either a perfect
transcript or a total failure depending entirely on what you did to the strings
first. Every published ASR figure has made that choice, usually silently, and
comparing across two different choices is not a comparison.

The leaderboard uses Whisper's `EnglishTextNormalizer`, so that is what the
harness runs — the real one, from `transformers`, fed OpenAI's own `english.json`
spelling map, whose SHA-256 is pinned in `jbench/normalizer.lock.json` so it
cannot change underneath a result.

Reimplementing it was considered and rejected. Its number handling ("twenty
twenty three" → "2023") runs to several hundred lines of special cases, and a
version that is subtly wrong is worse than none: it produces a plausible WER
that quietly is not the one being quoted.

Where it is unavailable — offline machines, CI — the harness falls back to a
deliberately simpler normaliser, records that it did, and **refuses to print the
result next to a published figure**. A fallback that is obviously different is
honest; a near-copy that is 0.3% off is not.

### Both sides, always

Reference and hypothesis go through the same normaliser. Normalising only the
hypothesis is a quiet way to score your own model generously.

### The corpus figure is pooled, not averaged

Total errors over total reference words. The mean of per-utterance rates lets a
three-word utterance count as much as a thirty-word one, and produces a number
nobody else can reproduce.

### Errors are split, not summed

A 10% WER made of deletions is a segmentation problem; the same 10% made of
substitutions is an acoustic one. The report always shows substitutions,
deletions and insertions separately, because the total does not say which half
of the pipeline to go and fix.

### Every figure carries an interval

Two thousand utterances is not infinite. Each WER comes with a 95% percentile
bootstrap interval, resampling whole utterances — words within an utterance are
not independent, and resampling words gives an interval several times too
narrow.

### Comparisons are paired

"The new VAD settings improved WER by 0.4%" is a claim about a difference.
`bench compare` resamples the *pairs* — the same utterances under both systems —
so shared per-utterance difficulty cancels instead of inflating the variance.
Comparing two independent intervals and looking for overlap is the common
mistake, and it routinely hides real improvements.

---

## Two segmentations, reported separately

Corpus utterances arrive pre-cut at sentence boundaries. Jotter cuts its own with
Silero VAD. These measure different things:

- `--segmentation none` feeds the whole clip to the recogniser. This is what
  published figures do. It measures **the model**.
- `--segmentation vad` runs the detector first, exactly as a real recording
  would. It measures **the pipeline**.

Both are reported. The gap between them is the cost of Jotter's own
segmentation, and it is a result rather than an inconvenience.

---

## Why there is a second binary

`jotter transcribe` takes a recording directory and builds a recogniser per
call. LibriSpeech test-clean is 2620 utterances, so driving the benchmark
through it would spend hours reloading 660 MB of model — measuring process
startup, not accuracy.

`jotter-bench` (cargo feature `bench`, off by default) loads the model once and
walks a manifest. The thing that makes its numbers meaningful is that it is not
a second implementation: it calls `audio::transcribe::transcribe_track`, the
same function the real pass calls, which is why that function is `pub`.

That claim is checked rather than asserted. `bench verify` puts the same clips
through both `jotter-bench` and a real `jotter transcribe` over a synthesised
recording directory and requires the text to match exactly. A mismatch
invalidates every number the harness has produced since, so it is a command and
not a comment.

---

## What these numbers do not say

- **The meeting benchmark's bleed is synthetic.** A generated impulse response,
  not a measured room, and with no coupling of your own voice back through the
  speakers. It is a fair test of the two-track idea and a fair comparison
  between builds. It is not a measurement of a real laptop in a real room. Pass
  `--rir` with a measured response (openslr.org/28) for an absolute figure.
- **AMI is not your meetings.** Four people, a fixed room, British and European
  accents, 2005 recording equipment. It is the best public proxy for the job and
  it is still a proxy.
- **A corpus WER is not a per-meeting WER.** It is an average over a
  distribution somebody else chose.
- **Nothing here measures search.** Semantic search is not implemented yet, so
  there is nothing to benchmark. When it exists it needs its own retrieval
  measurements, and a transcription WER will not stand in for them.
- **The AMI test split ships as data, not gospel.**
  `benchmarks/jbench/corpora/ami_test.txt` carries the Full-corpus-ASR
  partition; verify it against the official partition before quoting a number
  produced with it.

---

## Running it

See [`benchmarks/README.md`](../benchmarks/README.md).

## What CI does, and does not

Corpora are tens of gigabytes, partly behind agreements, and CI has no network
access to them. So CI never asserts a word error rate. It runs the harness's own
tests — known-answer cases for the aligner, normaliser behaviour, manifest
round-trips — and a smoke run over generated audio that proves manifests reach
the binary, the binary writes parseable output, the scorer reads it, and
`recording.py` still writes a `meta.json` that Jotter parses.

Real corpus runs are a local command. Their results are committed under
`benchmarks/results/`, which is the evidence.
