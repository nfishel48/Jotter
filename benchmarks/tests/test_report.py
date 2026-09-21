"""Tests for the prose the report puts around the numbers.

The figures are checked in `test_score.py`. What is checked here is the
sentence that tells a reader which stage to go and look at — wrong prose over
a correct number is worse than no prose, because it is acted on.
"""

import unittest

from jbench.report import diagnose


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
