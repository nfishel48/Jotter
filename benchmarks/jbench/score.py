"""Word error rate, and an honest error bar around it.

The arithmetic is the easy part. What makes a benchmark trustworthy is the
things around it:

- **The S/D/I split.** A 10% WER made of deletions is a segmentation problem;
  the same 10% made of substitutions is an acoustic one. A single percentage
  cannot tell you which half of the pipeline to go and fix.
- **A confidence interval.** Two thousand utterances is not infinite. Quoting
  "5.1%" with no interval invites reading a 0.2% difference as a result when it
  is noise, so every figure here carries a bootstrap CI.
- **A paired test for comparisons.** "The new VAD settings improved WER by 0.4%"
  is a claim about a difference, and the right test for it resamples the
  *pairs* — the same utterances under both systems — because per-utterance
  difficulty is shared between the two and cancels out. An unpaired comparison
  of two overlapping intervals throws that away and will call real improvements
  insignificant.

Alignment is Levenshtein with backtrace, which is the same edit distance NIST's
`sclite` computes for its default scoring. `sclite` breaks ties differently and
can weight substitutions, so the totals agree but individual alignments may
not; `bench score --sclite` cross-checks against it where it is installed.
"""

from __future__ import annotations

import os
import random
from concurrent.futures import ProcessPoolExecutor, as_completed
from dataclasses import dataclass, field

# Backtrace directions. Plain ints rather than an enum: this is the inner loop
# of an O(n*m) table over thousands of utterances.
_MATCH, _SUB, _DEL, _INS = 0, 1, 2, 3


@dataclass
class Counts:
    """Edit counts for one utterance or a whole corpus."""

    hits: int = 0
    substitutions: int = 0
    deletions: int = 0
    insertions: int = 0

    @property
    def reference_length(self) -> int:
        """Tokens in the reference. The denominator — note it excludes
        insertions, which is why WER can exceed 100%."""
        return self.hits + self.substitutions + self.deletions

    @property
    def errors(self) -> int:
        return self.substitutions + self.deletions + self.insertions

    @property
    def rate(self) -> float:
        """Errors per reference token.

        An empty reference with a non-empty hypothesis is 1.0, not infinity:
        every insertion is an error and there is nothing to divide by, so
        reporting the worst finite score keeps corpus totals summable.
        """
        if self.reference_length == 0:
            return 1.0 if self.insertions else 0.0
        return self.errors / self.reference_length

    def __add__(self, other: "Counts") -> "Counts":
        return Counts(
            self.hits + other.hits,
            self.substitutions + other.substitutions,
            self.deletions + other.deletions,
            self.insertions + other.insertions,
        )


@dataclass
class Utterance:
    """One scored item, kept whole so the report can show the worst offenders."""

    id: str
    reference: str
    hypothesis: str
    counts: Counts
    audio_secs: float = 0.0
    elapsed_secs: float = 0.0

    @property
    def rate(self) -> float:
        return self.counts.rate


@dataclass
class Result:
    """A scored run."""

    utterances: list[Utterance] = field(default_factory=list)

    @property
    def counts(self) -> Counts:
        total = Counts()
        for u in self.utterances:
            total = total + u.counts
        return total

    @property
    def wer(self) -> float:
        """Corpus WER: total errors over total reference tokens.

        Pooled, not the mean of per-utterance rates. The mean lets a
        three-word utterance count as much as a thirty-word one, which is how
        you get a figure nobody else can reproduce.
        """
        return self.counts.rate

    @property
    def audio_secs(self) -> float:
        return sum(u.audio_secs for u in self.utterances)

    @property
    def elapsed_secs(self) -> float:
        return sum(u.elapsed_secs for u in self.utterances)

    @property
    def rtf(self) -> float:
        """Real-time factor: seconds of compute per second of audio. Lower is
        faster; below 1.0 means it keeps up with the meeting."""
        return self.elapsed_secs / self.audio_secs if self.audio_secs else 0.0

    def worst(self, n: int = 10) -> list[Utterance]:
        """The utterances that hurt most — errors first, not rate.

        Rate would put every one-word utterance at the top, where they tell you
        nothing. Ranking by absolute errors surfaces the long failures that
        actually move the corpus number.
        """
        return sorted(self.utterances, key=lambda u: (-u.counts.errors, u.id))[:n]


def align(reference: list[str], hypothesis: list[str]) -> Counts:
    """Levenshtein counts, with substitutions distinguished from ins+del.

    Two rows rather than the full table: 2620 LibriSpeech utterances is fine
    either way, but an AMI meeting transcript is thousands of tokens per side
    and the square table is what would make this slow.
    """
    n, m = len(reference), len(hypothesis)
    if n == 0:
        return Counts(insertions=m)
    if m == 0:
        return Counts(deletions=n)

    # Each cell holds (cost, hits, subs, dels, ins) so the counts come out of
    # the same pass as the distance and no backtrace table is needed.
    previous: list[tuple[int, int, int, int, int]] = [
        (j, 0, 0, 0, j) for j in range(m + 1)
    ]

    for i in range(1, n + 1):
        current = [(i, 0, 0, i, 0)]
        ref_token = reference[i - 1]
        for j in range(1, m + 1):
            diag = previous[j - 1]
            if ref_token == hypothesis[j - 1]:
                current.append((diag[0], diag[1] + 1, diag[2], diag[3], diag[4]))
                continue

            sub = (diag[0] + 1, diag[1], diag[2] + 1, diag[3], diag[4])
            dele = previous[j]
            dele = (dele[0] + 1, dele[1], dele[2], dele[3] + 1, dele[4])
            ins = current[j - 1]
            ins = (ins[0] + 1, ins[1], ins[2], ins[3], ins[4] + 1)
            # Ties go substitution, then deletion, then insertion — sclite's
            # order, so the two agree on more than just the total.
            current.append(min(sub, dele, ins, key=lambda c: c[0]))
        previous = current

    _, hits, subs, dels, ins = previous[m]
    return Counts(hits=hits, substitutions=subs, deletions=dels, insertions=ins)


def score_pair(
    item_id: str,
    reference: str,
    hypothesis: str,
    normalizer,
    audio_secs: float = 0.0,
    elapsed_secs: float = 0.0,
) -> Utterance:
    """Normalise both sides, then align. Both sides, always — normalising only
    the hypothesis is a way to score your own model generously."""
    ref_tokens = normalizer(reference).split()
    hyp_tokens = normalizer(hypothesis).split()
    return Utterance(
        id=item_id,
        reference=" ".join(ref_tokens),
        hypothesis=" ".join(hyp_tokens),
        counts=align(ref_tokens, hyp_tokens),
        audio_secs=audio_secs,
        elapsed_secs=elapsed_secs,
    )


# ---------------------------------------------------------------------------
# Running the scorer over a whole corpus.
#
# `align` is O(n*m) pure Python, so a meeting-length corpus is minutes of
# arithmetic rather than seconds. Threads cannot help — the work is bytecode
# and the GIL serialises it — so this fans out across processes.
# ---------------------------------------------------------------------------


@dataclass
class Task:
    """One item waiting to be scored, before normalisation."""

    id: str
    reference: str
    hypothesis: str
    audio_secs: float = 0.0
    elapsed_secs: float = 0.0

    @property
    def cost(self) -> int:
        """Rough size of the alignment table, for scheduling.

        Word counts off the raw text: normalising first would be more accurate
        and would also mean doing the expensive half of the work twice.
        """
        return (self.reference.count(" ") + 1) * (self.hypothesis.count(" ") + 1)


_worker_normalizer = None


def _init_worker(mode: str) -> None:
    """Rebuild the normaliser inside each worker rather than pickling it.

    The whisper normaliser closes over a few thousand spelling entries and
    comes from an import each process has to do anyway.
    """
    global _worker_normalizer
    from . import normalize

    _worker_normalizer = normalize.load(mode)


def _score_task(indexed: tuple[int, Task]) -> tuple[int, Utterance]:
    index, task = indexed
    return index, score_pair(
        task.id,
        task.reference,
        task.hypothesis,
        _worker_normalizer,
        audio_secs=task.audio_secs,
        elapsed_secs=task.elapsed_secs,
    )


# Below this much alignment work, spawning processes costs more than it saves:
# every worker re-imports transformers to rebuild the normaliser.
_PARALLEL_THRESHOLD_CELLS = 20_000_000


def score_all(
    tasks: list[Task],
    normalizer,
    workers: int | None = None,
    on_progress=None,
) -> Result:
    """Score every item, in parallel when there is enough work to justify it.

    Utterances come back in the order given, never in the order the workers
    happened to finish: `bootstrap_interval` indexes into this list, so an
    order that depended on scheduling would move the confidence interval on
    every rerun.
    """
    total = len(tasks)
    if workers is None:
        workers = os.cpu_count() or 1
    workers = max(1, min(workers, total))

    done = 0

    def tick() -> None:
        nonlocal done
        done += 1
        if on_progress:
            on_progress(done, total)

    if workers == 1 or sum(t.cost for t in tasks) < _PARALLEL_THRESHOLD_CELLS:
        utterances = []
        for task in tasks:
            utterances.append(
                score_pair(
                    task.id,
                    task.reference,
                    task.hypothesis,
                    normalizer,
                    audio_secs=task.audio_secs,
                    elapsed_secs=task.elapsed_secs,
                )
            )
            tick()
        return Result(utterances)

    scored: list[Utterance | None] = [None] * total
    # Longest first. These items are wildly uneven — a 40-minute meeting beside
    # a one-line utterance — and starting a big one last leaves every other
    # core idle while it finishes alone.
    order = sorted(enumerate(tasks), key=lambda it: -it[1].cost)

    with ProcessPoolExecutor(
        max_workers=workers, initializer=_init_worker, initargs=(normalizer.mode,)
    ) as pool:
        futures = [pool.submit(_score_task, item) for item in order]
        for future in as_completed(futures):
            index, utterance = future.result()
            scored[index] = utterance
            tick()

    return Result([u for u in scored if u is not None])


def bootstrap_interval(
    result: Result, confidence: float = 0.95, resamples: int = 1000, seed: int = 0
) -> tuple[float, float]:
    """Percentile bootstrap over utterances.

    Resamples whole utterances rather than words: words within an utterance are
    not independent — one misheard proper noun takes its neighbours with it —
    and treating them as if they were produces an interval several times too
    narrow.

    Seeded, because a confidence interval that moves when you rerun the report
    is one more thing nobody can reproduce.
    """
    if not result.utterances:
        return (0.0, 0.0)

    rng = random.Random(seed)
    population = result.utterances
    size = len(population)
    rates = []
    for _ in range(resamples):
        total = Counts()
        for _ in range(size):
            total = total + population[rng.randrange(size)].counts
        rates.append(total.rate)

    rates.sort()
    tail = (1.0 - confidence) / 2.0
    low = rates[max(0, int(tail * resamples) - 1)]
    high = rates[min(resamples - 1, int((1.0 - tail) * resamples))]
    return (low, high)


def paired_bootstrap(
    baseline: Result, candidate: Result, resamples: int = 1000, seed: int = 0
) -> dict:
    """Is `candidate` really better than `baseline`?

    Resamples the utterances the two runs have in common and recomputes both
    WERs on the same resample, so shared per-utterance difficulty cancels
    instead of inflating the variance. The p-value is the fraction of
    resamples in which the candidate failed to beat the baseline — a one-sided
    test, because "did this change help" is the question actually being asked.

    Comparing two independent confidence intervals instead is the common
    mistake: overlapping intervals routinely hide differences this test finds.
    """
    by_id = {u.id: u for u in baseline.utterances}
    pairs = [(by_id[u.id], u) for u in candidate.utterances if u.id in by_id]
    if not pairs:
        raise ValueError("the two runs share no utterance ids — different corpora?")

    rng = random.Random(seed)
    size = len(pairs)
    observed = _pooled(p[1] for p in pairs) - _pooled(p[0] for p in pairs)

    worse = 0
    for _ in range(resamples):
        sample = [pairs[rng.randrange(size)] for _ in range(size)]
        delta = _pooled(p[1] for p in sample) - _pooled(p[0] for p in sample)
        # `observed < 0` means the candidate has the lower (better) WER.
        if (delta >= 0) if observed < 0 else (delta <= 0):
            worse += 1

    return {
        "paired_utterances": size,
        "baseline_wer": _pooled(p[0] for p in pairs),
        "candidate_wer": _pooled(p[1] for p in pairs),
        "delta_wer": observed,
        "p_value": worse / resamples,
        "resamples": resamples,
    }


def _pooled(utterances) -> float:
    total = Counts()
    for u in utterances:
        total = total + u.counts
    return total.rate
