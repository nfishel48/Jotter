# Results

Committed output from real benchmark runs — the evidence behind any accuracy
claim Jotter makes.

Each run writes two files:

- `<corpus>-<segmentation>[-<tag>].json` — the record. Complete and diffable:
  every utterance's error counts, the confidence interval, the model, the build,
  the detector settings and the host. `bench compare` reads these.
- `<corpus>-<segmentation>[-<tag>].md` — the argument. The headline figure, where
  the errors are, and what it cost.

Nothing is generated here by CI. CI has no access to the corpora and never
asserts a word error rate — see the note at the end of
[`docs/BENCHMARKS.md`](../../docs/BENCHMARKS.md). These files come from someone
running the harness on a real machine and committing what came out.

What the terms mean — WER, the S/D/I split, confidence intervals, RTF, and which
of them a user actually feels — is
[`../README.md#reading-a-result`](../README.md#reading-a-result). Start there if
a number looks surprising; a 99% WER usually does not mean what it appears to.

Before quoting a figure from here, check two fields in the JSON:

- `comparable_to_published` — `false` means the run fell back to basic
  normalisation and the number cannot be set beside a leaderboard figure.
- `run.git_dirty` — `true` means the recorded commit does not describe the code
  that produced the number.
