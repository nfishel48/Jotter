"""The two-track meeting benchmark — the measurement only Jotter can make.

Every other local transcription tool records one stream and then tries to work
out who was speaking. Jotter records two: `mic.wav` is you, `system.wav` is
everyone else, and "was this me?" is answered by which file the audio came out
of rather than by a diarization model. `docs/ARCHITECTURE.md` makes that
argument; this measures whether it holds.

**The synthesis.** AMI gives one headset microphone per participant, which is
exactly the raw material. For each meeting, and each participant A in turn:

- `system.wav` is a clean mix of the *other* participants' headsets. That is
  what the operating system's audio tap captures: a digital copy, no room.
- `mic.wav` is A's own headset plus that mix pushed through a synthetic room
  (`room.py`) and attenuated. That is what a real microphone picks up: you,
  plus your speakers bleeding back in.

Then the whole production pipeline runs over it — `jotter process` to cancel the
echo, `jotter transcribe` to produce a transcript — and three things get scored:

1. **Word error rate per track.** How good is the transcript, in a meeting.
2. **Speaker attribution.** For every moment where exactly one side is really
   speaking, did the transcript put it on the right track? This is the claim
   under test, and it is measured in time rather than words so it does not
   depend on getting an alignment right first.
3. **What the echo canceller bought.** The same meetings with the AEC pass
   skipped, so the improvement is a number rather than an assumption.

**Caveat, stated plainly.** The bleed is synthetic: a generated impulse
response, not a measured room, and no acoustic coupling of A's own voice back
through the speakers. It is a fair test of the two-track *idea* and a
comparable test between builds. It is not a substitute for measuring a real
laptop in a real room.
"""

from __future__ import annotations

import hashlib
import json
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

import numpy as np

from . import paths, recording, room
from .audio import read_mono
from .corpora import ami

#: Frame for the attribution measurement. 10 ms is the usual resolution for
#: speech activity scoring, and fine enough that a boundary error costs a frame
#: rather than a verdict.
FRAME_SECS = 0.01

#: How far below the near-end voice the bleed sits, in dB. Speakers at a
#: conversational level a metre from a laptop microphone land around here.
DEFAULT_BLEED_DB = 12.0


@dataclass
class Attribution:
    """Frame counts for the "which track" question.

    Only frames where exactly one side is genuinely speaking are counted.
    Overlap is not a wrong answer to this question — it is a different question,
    and folding it in would let a tool that guesses "both" always score well.
    """

    mic_correct: int = 0
    mic_total: int = 0
    system_correct: int = 0
    system_total: int = 0
    #: Frames where the reference has someone speaking and the transcript has
    #: nothing at all. Not an attribution error — a miss — but it belongs in
    #: the same table, because a tool can look perfectly accurate by
    #: transcribing almost nothing.
    missed: int = 0

    @property
    def accuracy(self) -> float:
        total = self.mic_total + self.system_total
        return (self.mic_correct + self.system_correct) / total if total else 0.0

    @property
    def mic_recall(self) -> float:
        return self.mic_correct / self.mic_total if self.mic_total else 0.0

    @property
    def system_recall(self) -> float:
        return self.system_correct / self.system_total if self.system_total else 0.0

    def as_dict(self) -> dict:
        return {
            "accuracy": self.accuracy,
            "mic_recall": self.mic_recall,
            "system_recall": self.system_recall,
            "mic_frames": self.mic_total,
            "system_frames": self.system_total,
            "missed_frames": self.missed,
            "frame_secs": FRAME_SECS,
        }


def synthesise(
    root: Path,
    into: Path,
    bleed_db: float = DEFAULT_BLEED_DB,
    rt60: float = 0.3,
    delay_ms: float = 8.0,
    rir: Path | None = None,
    limit: int | None = None,
) -> list[Path]:
    """Build one recording directory per (meeting, participant).

    Returns the directories written. Each also gets a `reference.json` holding
    the per-speaker word timings, so scoring never has to go back to the corpus.
    """
    root = Path(root)
    written: list[Path] = []

    for meeting in ami.meetings():
        channels = _channels(root, meeting)
        if len(channels) < 2:
            print(f"skip {meeting}: needs at least two headset channels", file=sys.stderr)
            continue

        for channel in channels:
            if limit is not None and len(written) >= limit:
                return written
            written.append(
                _synthesise_one(
                    root, into, meeting, channel, channels, bleed_db, rt60, delay_ms, rir
                )
            )
    return written


def _channels(root: Path, meeting: str) -> list[str]:
    present = []
    for index, letter in enumerate(ami.CHANNELS):
        audio = root / "amicorpus" / meeting / "audio" / f"{meeting}.Headset-{index}.wav"
        words = root / "annotations" / "words" / f"{meeting}.{letter}.words.xml"
        if audio.exists() and words.exists():
            present.append(letter)
    return present


def _synthesise_one(
    root: Path,
    into: Path,
    meeting: str,
    speaker: str,
    channels: list[str],
    bleed_db: float,
    rt60: float,
    delay_ms: float,
    rir: Path | None,
) -> Path:
    near, rate = read_mono(_audio_path(root, meeting, speaker))

    # The others, summed. Headset channels of one meeting share a clock and a
    # start, so they line up sample for sample without any alignment step —
    # which is the whole reason AMI can stand in for a two-track capture.
    far = np.zeros_like(near)
    for other in channels:
        if other == speaker:
            continue
        signal, other_rate = read_mono(_audio_path(root, meeting, other))
        if other_rate != rate:
            raise ValueError(f"{meeting}: channels disagree on sample rate")
        length = min(len(far), len(signal))
        far[:length] += signal[:length]

    response = room.load_response(rir, rate) if rir else room.impulse_response(
        rate, rt60=rt60, delay_ms=delay_ms, seed=_room_seed(meeting)
    )
    bleed = room.scale_to_ratio(room.apply(far, response), near, bleed_db)

    directory = into / f"{meeting}.{speaker}"
    recording.write(directory, mic=near + bleed, system=far, rate=rate, device_suffix="ami")

    # Reference word timings for both sides, so scoring is self-contained.
    reference = {
        "meeting": meeting,
        "speaker": speaker,
        "bleed_db": bleed_db,
        "mic_words": ami.read_words(_words_path(root, meeting, speaker)),
        "system_words": [
            word
            for other in channels
            if other != speaker
            for word in ami.read_words(_words_path(root, meeting, other))
        ],
    }
    (directory / "reference.json").write_text(json.dumps(reference))
    return directory


def _room_seed(meeting: str) -> int:
    """A different room per meeting, but the *same* different room every run.

    This was `abs(hash(meeting))`, and `hash()` of a str is randomised per
    process — so every invocation built a different room, two runs of this
    benchmark were never comparable, and an A/B across them measured the
    furniture rather than the change under test. `room.py` is written around
    the opposite guarantee ("the same seed gives the same room on every
    machine"); `hashlib` is what actually delivers it.
    """
    digest = hashlib.sha256(meeting.encode()).digest()
    return int.from_bytes(digest[:4], "big")


def _audio_path(root: Path, meeting: str, channel: str) -> Path:
    index = ami.CHANNELS.index(channel)
    return root / "amicorpus" / meeting / "audio" / f"{meeting}.Headset-{index}.wav"


def _words_path(root: Path, meeting: str, channel: str) -> Path:
    return root / "annotations" / "words" / f"{meeting}.{channel}.words.xml"


def transcribe(directory: Path, with_aec: bool = True) -> None:
    """Run the production pipeline over one synthesised recording."""
    jotter = paths.cargo_binary("jotter")

    if with_aec:
        # The echo pass writes its own verdict into meta.json, and the
        # transcription pass reads that verdict to choose a mic track. Running
        # them in this order is not a convenience — it is the pipeline.
        _run([str(jotter), "process", str(directory), "--force"])

    _run([str(jotter), "transcribe", str(directory), "--tracks", "both", "--force"])


def _run(command: list[str]) -> None:
    result = subprocess.run(command, capture_output=True, text=True)
    if result.returncode != 0:
        raise RuntimeError(f"{' '.join(command)} failed:\n{result.stdout}{result.stderr}")


def score_attribution(directory: Path) -> Attribution:
    """Did each transcribed moment land on the right track?

    Measured on a 10 ms grid over the frames where exactly one side is really
    speaking. Overlapped speech is excluded rather than counted as a failure:
    when two people talk at once there is no single right answer, and including
    it would measure the corpus's overlap rate as much as the tool.
    """
    directory = Path(directory)
    reference = json.loads((directory / "reference.json").read_text())
    transcript = json.loads((directory / "transcript.json").read_text())

    mic_true = _mask(reference["mic_words"])
    system_true = _mask(reference["system_words"])
    length = max(len(mic_true), len(system_true))
    mic_true = _pad(mic_true, length)
    system_true = _pad(system_true, length)

    mic_hyp = _pad(_mask_segments(transcript["segments"], "mic"), length)
    system_hyp = _pad(_mask_segments(transcript["segments"], "system"), length)

    only_mic = mic_true & ~system_true
    only_system = system_true & ~mic_true

    result = Attribution()
    result.mic_total = int(only_mic.sum())
    result.system_total = int(only_system.sum())
    result.mic_correct = int((only_mic & mic_hyp).sum())
    result.system_correct = int((only_system & system_hyp).sum())
    result.missed = int(((only_mic | only_system) & ~(mic_hyp | system_hyp)).sum())
    return result


def _mask(words: list) -> np.ndarray:
    """A boolean speech mask on the frame grid, from (start, end, word) triples.

    Words AMI could not time-align carry NaN and are skipped: they were said,
    so they belong in the word error rate, but they cannot say *when*.
    """
    spans = [
        (start, end) for start, end, _ in words if _timed(start) and _timed(end) and end > start
    ]
    if not spans:
        return np.zeros(0, dtype=bool)

    frames = int(max(end for _, end in spans) / FRAME_SECS) + 1
    mask = np.zeros(frames, dtype=bool)
    for start, end in spans:
        mask[int(start / FRAME_SECS) : int(end / FRAME_SECS) + 1] = True
    return mask


def _timed(value) -> bool:
    """Whether a word carries a usable timestamp.

    Guards both shapes an untimed word can arrive in: `None` from
    `ami._time`, and NaN from a `reference.json` written before that returned
    `None`. `value != value` is the NaN test.
    """
    return value is not None and value == value


def _mask_segments(segments: list[dict], track: str) -> np.ndarray:
    spans = [
        (s["start"], s["end"]) for s in segments if s.get("track") == track and s["end"] > s["start"]
    ]
    if not spans:
        return np.zeros(0, dtype=bool)
    frames = int(max(end for _, end in spans) / FRAME_SECS) + 1
    mask = np.zeros(frames, dtype=bool)
    for start, end in spans:
        mask[int(start / FRAME_SECS) : int(end / FRAME_SECS) + 1] = True
    return mask


def _pad(mask: np.ndarray, length: int) -> np.ndarray:
    if len(mask) >= length:
        return mask[:length]
    return np.concatenate([mask, np.zeros(length - len(mask), dtype=bool)])


def track_text(directory: Path, track: str) -> str:
    transcript = json.loads((Path(directory) / "transcript.json").read_text())
    return " ".join(s["text"] for s in transcript["segments"] if s.get("track") == track)


def reference_text(directory: Path, side: str) -> str:
    reference = json.loads((Path(directory) / "reference.json").read_text())
    return " ".join(word for _, _, word in reference[f"{side}_words"])


def aec_numbers(directory: Path) -> dict:
    """What the echo pass recorded about itself, straight from `meta.json`.

    Read rather than recomputed: these are the figures the shipped code acts
    on — `Meta::preferred_mic_path` uses them to decide whether the cancelled
    track is worth transcribing — so they are the ones worth reporting.
    """
    meta = json.loads((Path(directory) / "meta.json").read_text())
    return meta.get("aec") or {}
