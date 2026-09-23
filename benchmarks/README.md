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

## Reading a result

### The terms

Every report is built from four counts. Three are errors; the fourth is the one
people forget to look at.

| term | what it means |
| --- | --- |
| **hit** | a reference word the transcript got right |
| **substitution** (S) | a reference word transcribed as some other word — "cat" heard as "cab" |
| **deletion** (D) | a reference word missing from the transcript entirely |
| **insertion** (I) | a word in the transcript with nothing in the reference to match it |
| **reference words** | the size of the truth: hits + S + D. **Insertions are not in it.** |

Everything else in a report is derived from those:

| term | what it means |
| --- | --- |
| **WER** (word error rate) | `(S + D + I) / reference words`. Lower is better. Not a percentage of anything — see below. |
| **95% CI** | how much the WER would move on a different sample of the same size. Computed by resampling whole utterances 1,000 times and taking the 2.5th and 97.5th percentiles. Seeded, so it does not drift between runs. A wide interval means the corpus figure is being driven by a few items, not that the measurement is sloppy. |
| **RTF** (real-time factor) | compute seconds per second of audio. `0.034` means an hour of audio takes ~2 minutes. Below 1.0 keeps up with live speech. |
| **pooled** | corpus WER is total errors over total reference words, not the average of per-utterance rates. Averaging would let a three-word utterance count as much as a thirty-word one. |
| **normaliser** | what both sides are reduced to before comparison — case, punctuation, "don't"/"do not", British/American spelling. Published leaderboards use Whisper's; a run that falls back to the basic one cannot be set beside them. |
| **`comparable_to_published`** | `false` means the fallback normaliser was used. The number is real, but it is not yours to compare against anyone else's. |
| **segmentation `none` / `vad`** | `none` feeds pre-cut clips straight to the model — what leaderboards measure. `vad` runs the voice detector first, as a real recording would. The gap between them is what Jotter's own segmentation costs. |
| **p-value** (`bench compare`) | the fraction of resamples where the candidate failed to beat the baseline. Below 0.05 is the usual bar for "this change was real". |

### WER is not "percentage of words wrong"

This is the one that causes trouble, so here it is with real output. The AMI run
reports **99.30% WER** — which sounds like nothing worked. What actually
happened:

```
hits          110,761      <- correct, and not part of any error total
substitutions  15,242
deletions       9,619
insertions    109,810
reference     135,622      = hits + subs + dels
```

`110,761 / 135,622` — **81.7% of the reference words were transcribed
correctly.** The 99.30% comes almost entirely from 109,810 insertions, which are
counted in the numerator but not the denominator. Insertions alone equal 81% of
the reference.

Two consequences:

- **WER can exceed 100%**, and regularly does on meeting audio. A transcript
  twice as long as the truth can score over 200% while getting most words right.
- **`accuracy = 100 − WER` is wrong.** It is only defensible when insertions are
  negligible, which on clean read speech they are and on meeting audio they are
  not. Use `hits / reference words` if you want an accuracy figure.

On this AMI run the insertions are not hallucination — an AMI headset mic picks
up the whole room, while that channel's reference holds only its own speaker, so
correctly transcribed speech from other people is scored as error. The report
says which of the three dominates and what it implicates, because they point at
different stages: deletions at segmentation, substitutions at the acoustic
model, insertions usually at reference coverage.

### Which numbers your users actually feel

WER is an engineering comparison metric. It is the right thing to put beside a
competitor and the wrong thing to put in front of a user. What they experience
maps to the error split, not the total:

| what a user notices | the number behind it |
| --- | --- |
| "it got my words right" | **word accuracy** — `hits / reference words` |
| "it typed the wrong word" | **substitution rate** — visible, and they will fix it by hand |
| "words are just missing" | **deletion rate** — the worst failure, because nothing signals it happened |
| "it typed things nobody said" | **insertion rate** — background speech, noise, a second person in the room |
| "I waited too long" | **RTF**, and the wall-clock in the Cost section |
| "it put my words on the wrong speaker" | **speaker attribution**, from `bench meeting` |

Deletions deserve the most weight per unit. A substitution is visible and gets
corrected; a deleted clause leaves a fluent sentence that is quietly missing
something, and the user may never catch it.

For a user-facing claim, the defensible pair is **word accuracy on
`librispeech-test-clean --segmentation none`** (clean read speech, comparable to
published figures) plus **RTF** for speed. Quote the `vad` number if you want to
describe the product rather than the model — it includes the segmentation a real
recording goes through.

### Before you quote anything

Four checks, all in the JSON:

- `comparable_to_published` is `true` — otherwise the fallback normaliser ran.
- `run.git_dirty` is `false` — otherwise the recorded commit is not the code that
  produced the number.
- `counts.reference_words` is large enough to mean something. A handful of
  utterances will happily report a WER with a confidence interval spanning zero.
  `partial` stays `false` when only part of a corpus was ever prepared, so it is
  not the check you want — look at the word count.
- The 95% CI is narrow enough that the figure survives being quoted. If it spans
  40 points, the corpus is telling you it has more than one population in it.

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
| `jbench/score.py` | WER, error split, bootstrap intervals, paired test |
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
