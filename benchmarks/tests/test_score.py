"""Known-answer tests for the scorer.

A word error rate is a number nobody can eyeball for correctness, so the only
thing standing between a broken aligner and a published figure is a set of
cases whose answers are worked out by hand.
"""

import unittest
from unittest import mock

from jbench import normalize
from jbench import score as jbench_score
from jbench.score import (
    Counts,
    Result,
    Utterance,
    align,
    Task,
    bootstrap_interval,
    paired_bootstrap,
    score_all,
    score_pair,
)


class TestAlign(unittest.TestCase):
    def check(self, reference, hypothesis, **expected):
        counts = align(reference.split(), hypothesis.split())
        for field, value in expected.items():
            self.assertEqual(getattr(counts, field), value, f"{field} for {reference!r} vs {hypothesis!r}")

    def test_identical_text_has_no_errors(self):
        self.check("a b c", "a b c", substitutions=0, deletions=0, insertions=0, hits=3)

    def test_one_wrong_word_is_a_substitution(self):
        self.check("a b c", "a x c", substitutions=1, deletions=0, insertions=0, hits=2)

    def test_a_missing_word_is_a_deletion(self):
        self.check("a b c", "a c", substitutions=0, deletions=1, insertions=0, hits=2)

    def test_an_extra_word_is_an_insertion(self):
        self.check("a b c", "a x b c", substitutions=0, deletions=0, insertions=1, hits=3)

    def test_an_empty_hypothesis_deletes_everything(self):
        self.check("a b c", "", substitutions=0, deletions=3, insertions=0, hits=0)

    def test_an_empty_reference_inserts_everything(self):
        self.check("", "a b", substitutions=0, deletions=0, insertions=2, hits=0)

    def test_the_classic_pangram_case(self):
        # Two substitutions over nine reference words.
        counts = align(
            "the quick brown fox jumps over the lazy dog".split(),
            "the quick brown fox jumped over a lazy dog".split(),
        )
        self.assertEqual(counts.substitutions, 2)
        self.assertAlmostEqual(counts.rate, 2 / 9)

    def test_insertions_can_push_the_rate_above_one(self):
        # The denominator is the reference length, so a hypothesis that invents
        # words can score worse than 100%. That is correct, and surprising
        # enough to be worth pinning.
        counts = align(["a"], "a b c d".split())
        self.assertEqual(counts.insertions, 3)
        self.assertEqual(counts.rate, 3.0)


class TestResult(unittest.TestCase):
    def setUp(self):
        self.normalizer = normalize.load("basic")

    def utterance(self, item_id, reference, hypothesis, **kwargs):
        return score_pair(item_id, reference, hypothesis, self.normalizer, **kwargs)

    def test_corpus_wer_is_pooled_not_averaged(self):
        # One short utterance scored 100%, one long one scored 0%. The mean of
        # the rates would be 50%; the pooled figure — the one every published
        # number uses — is 1 error over 11 reference words.
        result = Result([
            self.utterance("short", "yes", "no"),
            self.utterance("long", " ".join(f"w{i}" for i in range(10)),
                           " ".join(f"w{i}" for i in range(10))),
        ])
        self.assertAlmostEqual(result.wer, 1 / 11)

    def test_worst_ranks_by_errors_not_rate(self):
        result = Result([
            self.utterance("tiny", "a", "b"),  # 100%, 1 error
            self.utterance("big", "a b c d e f", "x y z d e f"),  # 50%, 3 errors
        ])
        self.assertEqual([u.id for u in result.worst(2)], ["big", "tiny"])

    def test_real_time_factor(self):
        result = Result([self.utterance("a", "a", "a", audio_secs=10.0, elapsed_secs=2.5)])
        self.assertAlmostEqual(result.rtf, 0.25)


class TestScoreAll(unittest.TestCase):
    def setUp(self):
        self.normalizer = normalize.load("basic")

    def tasks(self):
        return [
            Task(id="a", reference="hello world", hypothesis="hello world"),
            Task(id="b", reference="one two three", hypothesis="one two"),
            Task(id="c", reference="alpha", hypothesis="alpha beta"),
        ]

    def test_scores_every_task_in_the_order_given(self):
        # Order is load-bearing: bootstrap_interval indexes into this list, so
        # a run whose order depended on worker scheduling would produce a
        # different confidence interval each time.
        result = score_all(self.tasks(), self.normalizer, workers=1)
        self.assertEqual([u.id for u in result.utterances], ["a", "b", "c"])
        self.assertEqual(result.counts.deletions, 1)
        self.assertEqual(result.counts.insertions, 1)

    def test_parallel_agrees_with_serial(self):
        tasks = self.tasks()
        serial = score_all(tasks, self.normalizer, workers=1)
        # Drop the threshold so three tiny utterances really do get fanned out
        # across processes — otherwise this silently retests the serial path.
        with mock.patch.object(jbench_score, "_PARALLEL_THRESHOLD_CELLS", 0):
            parallel = score_all(tasks, self.normalizer, workers=2)
        self.assertEqual(
            [(u.id, u.counts) for u in serial.utterances],
            [(u.id, u.counts) for u in parallel.utterances],
        )

    def test_progress_counts_up_to_the_total(self):
        seen = []
        score_all(self.tasks(), self.normalizer, workers=1, on_progress=lambda d, t: seen.append((d, t)))
        self.assertEqual(seen, [(1, 3), (2, 3), (3, 3)])

    def test_no_tasks_is_an_empty_result(self):
        self.assertEqual(score_all([], self.normalizer).utterances, [])


class TestIntervals(unittest.TestCase):
    def setUp(self):
        self.normalizer = normalize.load("basic")

    def make(self, error_every):
        utterances = []
        for i in range(200):
            reference = "alpha bravo charlie delta"
            hypothesis = "alpha bravo charlie x" if i % error_every == 0 else reference
            utterances.append(score_pair(str(i), reference, hypothesis, self.normalizer))
        return Result(utterances)

    def test_the_interval_brackets_the_point_estimate(self):
        result = self.make(4)
        low, high = bootstrap_interval(result)
        self.assertLessEqual(low, result.wer)
        self.assertLessEqual(result.wer, high)

    def test_the_interval_is_reproducible(self):
        # A confidence interval that moves between runs of the same report is
        # one more number nobody can check.
        result = self.make(4)
        self.assertEqual(bootstrap_interval(result), bootstrap_interval(result))

    def test_a_real_difference_survives_the_paired_test(self):
        baseline, candidate = self.make(2), self.make(20)
        verdict = paired_bootstrap(baseline, candidate)
        self.assertLess(verdict["delta_wer"], 0)  # candidate has fewer errors
        self.assertLess(verdict["p_value"], 0.05)

    def test_no_difference_does_not(self):
        verdict = paired_bootstrap(self.make(4), self.make(4))
        self.assertAlmostEqual(verdict["delta_wer"], 0.0)
        self.assertGreater(verdict["p_value"], 0.05)

    def test_comparing_different_corpora_is_an_error(self):
        other = Result([Utterance("elsewhere", "", "", Counts(hits=1))])
        with self.assertRaises(ValueError):
            paired_bootstrap(self.make(4), other)


if __name__ == "__main__":
    unittest.main()
