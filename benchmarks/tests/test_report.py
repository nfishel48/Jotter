"""Tests for the prose the report puts around the numbers.

The figures are checked in `test_score.py`. What is checked here is the
sentence that tells a reader which stage to go and look at — wrong prose over
a correct number is worse than no prose, because it is acted on.
"""

import subprocess
import unittest
from unittest import mock

from jbench import paths, report
from jbench.report import diagnose


class TestGitDirty(unittest.TestCase):
    """`git_dirty` must mean "the code was uncommitted", not "the tree moved".

    It used to shell `git status --porcelain` bare, which counts untracked
    files — so the results directory marked the tree dirty with the very file
    being written, and the flag was true on essentially every run. Since
    `results/README.md` tells people to check it before quoting a figure, a
    flag that is always true is worse than no flag.
    """

    def dirty(self, porcelain: str) -> bool:
        root = str(paths.REPO.resolve())

        def fake_run(command, **kwargs):
            out = root if "--show-toplevel" in command else porcelain
            return subprocess.CompletedProcess(command, 0, stdout=out, stderr="")

        with mock.patch.object(report.subprocess, "run", side_effect=fake_run):
            return report._git_dirty()

    def test_a_clean_tree_is_not_dirty(self):
        self.assertFalse(self.dirty(""))

    def test_an_untracked_result_is_not_dirty(self):
        # The first run of any new corpus. There is no prior file to modify,
        # so this was the case that made the flag unavoidable.
        self.assertFalse(self.dirty("?? benchmarks/results/librispeech-test-clean-none.json\n"))

    def test_a_modified_result_is_not_dirty(self):
        # Every rerun after the first.
        self.assertFalse(self.dirty(" M benchmarks/results/ami-ihm-test-vad.json\n"))

    def test_modified_source_is_dirty(self):
        self.assertTrue(self.dirty(" M benchmarks/jbench/score.py\n"))

    def test_modified_rust_is_dirty(self):
        # The figure depends on the binary as much as on the harness.
        self.assertTrue(self.dirty(" M src/audio/transcribe.rs\n"))

    def test_source_is_still_caught_alongside_results(self):
        # The regression that matters: results must not mask real dirt.
        self.assertTrue(
            self.dirty(
                "?? benchmarks/results/aec-sweep.json\n"
                " M benchmarks/results/aec-sweep.md\n"
                " M benchmarks/jbench/report.py\n"
            )
        )

    def test_a_rename_is_judged_by_its_destination(self):
        self.assertFalse(
            self.dirty("R  benchmarks/results/old.json -> benchmarks/results/new.json\n")
        )

    def test_a_quoted_path_is_still_recognised(self):
        # git quotes paths holding spaces or non-ASCII.
        self.assertFalse(self.dirty('?? "benchmarks/results/a b.json"\n'))

    def test_git_failing_is_not_reported_as_dirty(self):
        with mock.patch.object(
            report.subprocess, "run", side_effect=FileNotFoundError("no git")
        ):
            self.assertFalse(report._git_dirty())


def counts(substitutions=0, deletions=0, insertions=0):
    return {
        "substitutions": substitutions,
        "deletions": deletions,
        "insertions": insertions,
    }


class TestDiagnose(unittest.TestCase):
    def test_insertions_are_named_and_not_blamed_on_the_model(self):
        # The AMI IHM case: the recogniser is accurate, but each headset mic
        # hears the whole room while the reference holds one speaker. Reading
        # this as an acoustic failure sends you off fixing the wrong stage.
        text = diagnose(counts(substitutions=15242, deletions=9619, insertions=109810))
        self.assertIn("Insertions dominate", text)
        self.assertIn("81.5%", text)
        self.assertIn("reference does not cover", text)

    def test_deletions_point_at_segmentation(self):
        text = diagnose(counts(substitutions=10, deletions=80, insertions=10))
        self.assertIn("Deletions dominate", text)
        self.assertIn("segmentation", text)

    def test_substitutions_point_at_the_acoustic_model(self):
        text = diagnose(counts(substitutions=80, deletions=10, insertions=10))
        self.assertIn("Substitutions dominate", text)
        self.assertIn("acoustic model", text)

    def test_an_even_spread_claims_no_dominant_cause(self):
        # A plurality is not a diagnosis. Three-way-even errors mean general
        # difficulty, and saying otherwise would point at an innocent stage.
        text = diagnose(counts(substitutions=34, deletions=33, insertions=33))
        self.assertIn("No single error type dominates", text)
        self.assertNotIn("points at", text)

    def test_a_clean_run_says_so_instead_of_dividing_by_zero(self):
        self.assertIn("No errors", diagnose(counts()))


if __name__ == "__main__":
    unittest.main()
