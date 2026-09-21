"""Tests for the thing that decides whether a WER is comparable.

The stakes here are different from the scorer's. A broken aligner produces an
obviously wrong number; a subtly wrong normaliser produces a *plausible* number
that quietly is not the one the leaderboard is quoting.
"""

import unittest

from jbench import normalize


class TestBasic(unittest.TestCase):
    def test_case_and_punctuation_are_not_errors(self):
        self.assertEqual(normalize.basic_normalize("Hello, world!"), "hello world")

    def test_contractions_are_expanded(self):
        self.assertEqual(normalize.basic_normalize("don't"), "do not")
        self.assertEqual(normalize.basic_normalize("won't"), "will not")
        self.assertEqual(normalize.basic_normalize("I'm here"), "i am here")

    def test_titles_are_expanded(self):
        self.assertEqual(normalize.basic_normalize("Mr. Smith"), "mister smith")

    def test_filler_is_dropped(self):
        self.assertEqual(normalize.basic_normalize("so um yeah"), "so yeah")

    def test_bracketed_markup_is_dropped(self):
        # AMI and TED-LIUM references carry these; a recogniser emits none of
        # them, so scoring against them would be scoring against the format.
        self.assertEqual(normalize.basic_normalize("hello [noise] world"), "hello world")
        self.assertEqual(normalize.basic_normalize("hello <unk> world"), "hello world")

    def test_accents_are_not_errors(self):
        self.assertEqual(normalize.basic_normalize("café"), "cafe")

    def test_digit_grouping_is_removed(self):
        self.assertEqual(normalize.basic_normalize("1,000"), "1000")

    def test_whitespace_is_collapsed(self):
        self.assertEqual(normalize.basic_normalize("  a   b  "), "a b")


class TestLoad(unittest.TestCase):
    def test_basic_is_never_claimed_to_be_comparable(self):
        n = normalize.load("basic")
        self.assertEqual(n.mode, "basic")
        self.assertFalse(n.comparable)
        self.assertIn("comparable", n.provenance())

    def test_provenance_always_says_which_one_ran(self):
        for mode in ("basic", "whisper"):
            p = normalize.load(mode).provenance()
            self.assertIn(p["mode"], {"basic", "whisper"})
            self.assertTrue(p["detail"])

    def test_the_fallback_explains_itself(self):
        # A run that silently fell back is a run whose numbers get quoted by
        # mistake, so the reason has to travel with the result.
        n = normalize.load("basic")
        self.assertTrue(n.detail)


@unittest.skipUnless(
    normalize.load().comparable,
    "Whisper's normaliser is unavailable — run bootstrap.sh",
)
class TestWhisper(unittest.TestCase):
    """Only runs where the real normaliser is installed.

    These pin the behaviours that make a published comparison valid, and that
    the fallback deliberately does not attempt.
    """

    def setUp(self):
        self.n = normalize.load()

    def test_spoken_numbers_become_digits(self):
        self.assertEqual(self.n("twenty twenty three"), "2023")

    def test_british_spellings_are_standardised(self):
        self.assertEqual(self.n("colourful organisation"), "colorful organization")

    def test_it_agrees_with_the_fallback_on_plain_text(self):
        # Where neither numbers nor spelling are involved the two must not
        # disagree; if they do, the fallback is broken in a way that would
        # make offline runs misleading rather than merely incomparable.
        for text in ("hello world", "the quick brown fox", "don't stop"):
            self.assertEqual(self.n(text), normalize.basic_normalize(text), text)

    def test_the_spelling_map_is_pinned(self):
        self.assertTrue(normalize.LOCK_PATH.exists(), "english.json is not pinned")


if __name__ == "__main__":
    unittest.main()
