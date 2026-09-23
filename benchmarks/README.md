# Benchmarks

How accurate is Jotter, on the corpora the speech recognition field uses, scored
the way published leaderboards score them.

The method — and what the numbers do and do not say — is
[`docs/BENCHMARKS.md`](../docs/BENCHMARKS.md). This is how to run it.

Nothing here ships. The `bench` cargo feature of `jotter-cli` is off by
default, and the Python lives in its own virtualenv so `scripts/` stays
stdlib-only.

## Setup

```bash
./bootstrap.sh                                                # venv + Whisper's spelling map
cd .. && cargo build --release -p jotter-cli --features bench # the driver
jotter models pull                                            # the speech model, ~660 MB
```

`bootstrap.sh` fetches OpenAI's `english.json`, which is what makes a score here
comparable to a published one. Without it the harness still runs and marks every
result as not comparable.

Check it works, offline and without a corpus:

```bash
./bench smoke
```

## A word error rate

```bash
./bench list                                       # corpora and their licences
./bench fetch  --corpus librispeech-test-clean     # ~350 MB, checksum-pinned
./bench score  --corpus librispeech-test-clean
```

`score` implies `run`, which implies `prepare`, so that last line is the whole
job. The stages are separate because they cost differently — preparing is
minutes, running is hours, scoring is seconds — and a scoring change should
never cost the hours again.

Results land in `results/` as JSON (the record, committed) and Markdown (the
argument).

### Model or pipeline?

```bash
./bench score --corpus librispeech-test-clean --segmentation none   # the model
./bench score --corpus librispeech-test-clean --segmentation vad    # the pipeline
```

`none` matches how leaderboard figures are computed, so it is the number to put
beside someone else's. `vad` runs Silero first, as a real recording would. The
gap between them is what Jotter's own segmentation costs.

### Did a change actually help?

```bash
./bench score --corpus librispeech-test-clean --tag baseline
# ... change something ...
./bench score --corpus librispeech-test-clean --tag candidate --force
./bench compare results/librispeech-test-clean-vad-baseline.json \
                results/librispeech-test-clean-vad-candidate.json
```

A paired bootstrap over the same utterances, so shared difficulty cancels.
Comparing two confidence intervals for overlap is the common mistake and it
hides real improvements.

## Two-track meeting accuracy

The measurement no single-stream tool can make: AMI's per-speaker headsets
rebuilt as real Jotter recordings, one speaker as the microphone and the rest as
system audio with synthetic speaker bleed, then the full pipeline over it.

```bash
./bench fetch   --corpus ami-ihm-test     # tens of GB
./bench meeting --limit 8
./bench meeting --limit 8 --no-aec        # what the echo canceller buys
```

Reports word error rate per track and **speaker attribution** — for every 10 ms
where exactly one side is genuinely speaking, did the transcript put it on the
right track?

## Echo cancellation

Needs the `aec` feature, which `jotter-cli` builds by default:
`cargo build --release -p jotter-cli --features bench,aec` makes it explicit.

```bash
./bench prepare --corpus librispeech-test-clean
./bench aec --items 4
```

Builds recordings that run near-only, then far-only, then double-talk — the
three regimes `AecStats` reports separately — and sweeps echo return loss from 0
to 24 dB, so the result says where cancellation stops working.

## Checking the harness itself

```bash
./bench verify --corpus librispeech-test-clean --limit 10
.venv/bin/python -m unittest discover -s tests
```

`verify` is the important one. `jotter-bench` exists to avoid reloading the
model per utterance, and the entire case for its numbers is that it runs the
same code as `jotter transcribe`. This puts the same clips through both and
requires the text to match. A mismatch invalidates every number produced since.

## Layout

| | |
| --- | --- |
| `bench` | entry point |
| `jbench/score.py` | WER/CER, error split, bootstrap intervals, paired test |
| `jbench/normalize.py` | Whisper's normaliser, and an honest fallback |
| `jbench/corpora/` | one adapter per corpus; add a file to add a corpus |
| `jbench/recording.py` | writes recording directories Jotter accepts |
| `jbench/meeting.py` | two-track synthesis and attribution scoring |
| `jbench/aec.py` | echo test set and the sweep |
| `jbench/verify.py` | fast path vs. production |
| `results/` | committed evidence |
| `data/`, `work/`, `.venv/` | gitignored |

## Licences

Corpora are not ours. `./bench list` prints each one's terms. TED-LIUM is
**CC BY-NC-ND 3.0** — non-commercial, no derivatives — so it is opt-in: measure
with it, do not redistribute it or anything built from it.
