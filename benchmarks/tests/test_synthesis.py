"""Tests for the audio synthesis behind the meeting and AEC benchmarks.

These build recordings that later get scored, so a bug here does not produce an
error — it produces a plausible number measuring the wrong thing. The
properties worth pinning are the ones that would be silently wrong: that the
two tracks stay sample-aligned, that the echo sits at the level asked for, and
that the attribution scorer counts the frames it claims to count.
"""

import json
import sys
import tempfile
import unittest
from pathlib import Path

import numpy as np

from jbench import aec, meeting, recording, room


class TestRoom(unittest.TestCase):
    def test_the_response_starts_at_the_direct_path(self):
        # The delay is the reason the canceller has a delay estimator at all.
        # A response that peaked at lag zero would skip the hard part.
        response = room.impulse_response(16_000, rt60=0.3, delay_ms=10.0)
        self.assertEqual(int(np.argmax(np.abs(response))), 160)

    def test_the_tail_decays(self):
        response = room.impulse_response(16_000, rt60=0.3, delay_ms=0.0)
        early = np.abs(response[:1000]).mean()
        late = np.abs(response[-1000:]).mean()
        self.assertLess(late, early / 10)

    def test_convolution_preserves_length(self):
        # The two tracks of a recording have to stay sample-aligned; a track
        # that grew by the impulse response length would not be.
        signal = np.random.default_rng(0).standard_normal(8000).astype(np.float32)
        response = room.impulse_response(16_000)
        self.assertEqual(len(room.apply(signal, response)), len(signal))

    def test_scaling_hits_the_requested_ratio(self):
        rng = np.random.default_rng(0)
        reference = rng.standard_normal(16_000).astype(np.float32)
        signal = (rng.standard_normal(16_000) * 7.0).astype(np.float32)

        for ratio_db in (0.0, 6.0, 20.0):
            scaled = room.scale_to_ratio(signal, reference, ratio_db)
            measured = 20 * np.log10(room.rms(reference) / room.rms(scaled))
            self.assertAlmostEqual(measured, ratio_db, places=4)

    def test_a_silent_signal_is_not_divided_by_zero(self):
        silence = np.zeros(100, dtype=np.float32)
        other = np.ones(100, dtype=np.float32)
        self.assertTrue(np.all(room.scale_to_ratio(silence, other, 6.0) == 0))


class TestRoomSeed(unittest.TestCase):
    """The room a meeting is synthesised in must not change between runs.

    It did: the seed was `abs(hash(meeting))`, and `hash()` of a str is
    randomised per process, so every invocation built a different room and no
    two `bench meeting` runs were comparable. An A/B across them measured the
    furniture. Worth a test that would have caught it.
    """

    def test_the_seed_is_stable_across_processes(self):
        # Must be a subprocess: within one interpreter `hash()` is perfectly
        # stable, so an in-process assertion would have passed on the bug.
        import subprocess

        program = (
            "from jbench.meeting import _room_seed;"
            "print(_room_seed('EN2002a'), _room_seed('IS1009b'))"
        )
        runs = {
            subprocess.run(
                [sys.executable, "-c", program],
                capture_output=True,
                text=True,
                check=True,
                cwd=Path(__file__).resolve().parent.parent,
            ).stdout.strip()
            for _ in range(3)
        }
        self.assertEqual(len(runs), 1, f"seed moved between processes: {runs}")

    def test_different_meetings_get_different_rooms(self):
        # Stability must not be bought by giving every meeting one room; the
        # point is a different room per meeting, reproducibly.
        seeds = {meeting._room_seed(m) for m in ("EN2002a", "IS1009b", "TS3003a")}
        self.assertEqual(len(seeds), 3)

    def test_the_seed_fits_numpys_range(self):
        # `np.random.default_rng` rejects anything wider than 32 bits here.
        for name in ("EN2002a", "IS1009b", "TS3003a"):
            self.assertLess(meeting._room_seed(name), 2**32)
            self.assertGreaterEqual(meeting._room_seed(name), 0)


class TestRecordingDir(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())

    def test_frames_match_the_audio(self):
        # The transcription pass reports durations from meta.json before it
        # opens any audio, so a wrong count is a report that disagrees with
        # the recording it describes.
        samples = np.zeros(12_345, dtype=np.float32)
        directory = recording.write(self.tmp / "r", samples, samples, 16_000)
        meta = json.loads((directory / "meta.json").read_text())
        self.assertEqual(meta["mic"]["frames"], 12_345)
        self.assertEqual(meta["system"]["frames"], 12_345)

    def test_duration_matches_the_audio(self):
        samples = np.zeros(16_000, dtype=np.float32)
        directory = recording.write(self.tmp / "r", samples, None, 16_000)
        meta = json.loads((directory / "meta.json").read_text())
        self.assertAlmostEqual(meta["ended_at"] - meta["started_at"], 1.0, places=3)

    def test_a_single_track_recording_is_valid(self):
        directory = recording.write(self.tmp / "r", np.zeros(800, dtype=np.float32), None, 16_000)
        meta = json.loads((directory / "meta.json").read_text())
        self.assertIsNone(meta["system"])
        self.assertFalse((directory / "system.wav").exists())

    def test_both_tracks_share_a_start(self):
        # An invented offset would show up as the echo canceller hunting for a
        # delay that is not in the audio.
        samples = np.zeros(800, dtype=np.float32)
        directory = recording.write(self.tmp / "r", samples, samples, 16_000)
        meta = json.loads((directory / "meta.json").read_text())
        self.assertEqual(meta["mic"]["first_callback_nanos"], meta["system"]["first_callback_nanos"])


class TestAecSynthesis(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        rng = np.random.default_rng(0)
        self.clips = []
        for i in range(4):
            path = self.tmp / f"c{i}.wav"
            from jbench.audio import write_mono

            write_mono(path, (rng.standard_normal(16_000) * 0.1).astype(np.float32), 16_000)
            self.clips.append((f"c{i}", path, f"text {i}"))

    def test_one_condition_per_sweep_point(self):
        conditions = aec.build(self.clips, self.tmp / "out", sweep=(0.0, 12.0))
        self.assertEqual(len(conditions), 2)
        self.assertEqual([c.erl_db for c in conditions], [0.0, 12.0])

    def test_the_recording_has_three_regimes(self):
        condition = aec.build(self.clips, self.tmp / "out", sweep=(6.0,))[0]
        meta = json.loads((condition.directory / "meta.json").read_text())
        expected = int(3 * aec.REGIME_SECS * 16_000)
        self.assertEqual(meta["mic"]["frames"], expected)
        self.assertEqual(meta["system"]["frames"], expected)

    def test_the_far_only_regime_has_no_near_speech(self):
        # The middle third is the stretch ERLE is measured on. Near-end speech
        # leaking into it would make the headline figure meaningless.
        condition = aec.build(self.clips, self.tmp / "out", sweep=(6.0,))[0]
        from jbench.audio import read_mono

        system, rate = read_mono(condition.directory / "system.wav")
        span = int(aec.REGIME_SECS * rate)
        self.assertEqual(room.rms(system[:span]), 0.0)  # system silent while you speak
        self.assertGreater(room.rms(system[span : 2 * span]), 0.0)

    def test_the_reference_is_only_the_near_speaker(self):
        # A transcript of the mic track containing the far speaker has failed,
        # so the far speaker must not be in the reference.
        condition = aec.build(self.clips, self.tmp / "out", sweep=(6.0,))[0]
        self.assertIn("text 0", condition.reference)
        self.assertIn("text 1", condition.reference)
        self.assertNotIn("text 2", condition.reference)
        self.assertNotIn("text 3", condition.reference)

    def _clip(self, name: str, secs: float):
        from jbench.audio import write_mono

        path = self.tmp / f"{name}.wav"
        rng = np.random.default_rng(1)
        write_mono(path, (rng.standard_normal(int(16_000 * secs)) * 0.1).astype(np.float32), 16_000)
        return (name, path, f"words of {name}")

    def test_clips_longer_than_a_regime_are_dropped(self):
        # `_fit` truncates audio at REGIME_SECS but the whole clip's transcript
        # becomes the reference, so an over-long clip puts words in the answer
        # key that were never played — a floor of deletions no canceller can
        # avoid, and an absolute WER that is fiction.
        long_clip = self._clip("toolong", aec.REGIME_SECS + 2.5)
        conditions = aec.build(
            [long_clip] + self.clips, self.tmp / "out", sweep=(6.0,)
        )
        self.assertEqual(len(conditions), 1)
        self.assertNotIn("toolong", conditions[0].reference)

    def test_clips_shorter_than_a_regime_are_kept(self):
        # Padding a short clip with silence is free: silence in the audio is
        # silence in the reference. Only truncation loses words.
        short = [self._clip(f"s{i}", 1.5) for i in range(4)]
        condition = aec.build(short, self.tmp / "out", sweep=(6.0,))[0]
        self.assertIn("words of s0", condition.reference)

    def test_too_few_usable_clips_is_an_error_not_an_empty_sweep(self):
        # Silently returning nothing would look like a corpus problem rather
        # than a test-set one.
        long_clips = [self._clip(f"L{i}", aec.REGIME_SECS + 1.0) for i in range(4)]
        with self.assertRaises(ValueError) as caught:
            aec.build(long_clips, self.tmp / "out", sweep=(6.0,))
        self.assertIn("or shorter", str(caught.exception))


class TestAttribution(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())

    def write(self, mic_words, system_words, segments):
        directory = self.tmp / "m"
        directory.mkdir(parents=True, exist_ok=True)
        (directory / "reference.json").write_text(
            json.dumps({"mic_words": mic_words, "system_words": system_words})
        )
        (directory / "transcript.json").write_text(json.dumps({"segments": segments}))
        return directory

    def test_perfect_attribution(self):
        directory = self.write(
            mic_words=[[0.0, 1.0, "hello"]],
            system_words=[[2.0, 3.0, "world"]],
            segments=[
                {"start": 0.0, "end": 1.0, "track": "mic", "text": "hello"},
                {"start": 2.0, "end": 3.0, "track": "system", "text": "world"},
            ],
        )
        result = meeting.score_attribution(directory)
        self.assertEqual(result.accuracy, 1.0)
        self.assertEqual(result.missed, 0)

    def test_swapped_tracks_score_zero(self):
        directory = self.write(
            mic_words=[[0.0, 1.0, "hello"]],
            system_words=[[2.0, 3.0, "world"]],
            segments=[
                {"start": 0.0, "end": 1.0, "track": "system", "text": "hello"},
                {"start": 2.0, "end": 3.0, "track": "mic", "text": "world"},
            ],
        )
        self.assertEqual(meeting.score_attribution(directory).accuracy, 0.0)

    def test_overlapped_speech_is_excluded(self):
        # Both speaking at once has no single right answer. Counting it would
        # measure the corpus's overlap rate as much as the tool.
        directory = self.write(
            mic_words=[[0.0, 2.0, "a"]],
            system_words=[[0.0, 2.0, "b"]],
            segments=[{"start": 0.0, "end": 2.0, "track": "mic", "text": "a"}],
        )
        result = meeting.score_attribution(directory)
        self.assertEqual(result.mic_total, 0)
        self.assertEqual(result.system_total, 0)

    def test_untranscribed_speech_is_counted_as_missed(self):
        # A tool can look perfectly accurate by transcribing almost nothing.
        directory = self.write(
            mic_words=[[0.0, 1.0, "hello"]],
            system_words=[],
            segments=[],
        )
        result = meeting.score_attribution(directory)
        self.assertGreater(result.missed, 0)
        self.assertEqual(result.accuracy, 0.0)

    def test_untimed_words_do_not_break_the_mask(self):
        # AMI leaves times off words it could not align; they still count as
        # said, but they cannot say when.
        directory = self.write(
            mic_words=[[0.0, 1.0, "hello"], [None, None, "unaligned"]],
            system_words=[],
            segments=[{"start": 0.0, "end": 1.0, "track": "mic", "text": "hello"}],
        )
        self.assertEqual(meeting.score_attribution(directory).accuracy, 1.0)


if __name__ == "__main__":
    unittest.main()
