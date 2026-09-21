"""AMI, the meeting corpus — the one that actually looks like Jotter's job.

A hundred hours of real multi-party meetings: people interrupting, trailing off,
talking over each other. Word error rates here are several times LibriSpeech's,
and that gap is the honest answer to "how accurate is it in a meeting?".

This adapter uses the **individual headset microphones** (IHM). That is not just
the easier condition — it is the condition Jotter records in. `mic.wav` is a
close-talking microphone on one person, which is what a headset channel is.

It also carries the per-speaker structure the two-track benchmark needs: one
headset per participant is enough to synthesise a real Jotter recording, with
one speaker as the microphone and the rest as system audio. That lives in
`meeting.py`; this module just exposes the pieces.

Two downloads, because AMI separates them:

- audio, per meeting, from the corpus mirror,
- the manual word-level annotations, as one zip.

The test split is read from `ami_test.txt` rather than hard-coded in Python, so
it can be inspected and replaced without editing code. Verify it against the
official partition before quoting any number produced with it.
"""

from __future__ import annotations

import re
import xml.etree.ElementTree as ET
from pathlib import Path
from typing import Iterator

from .. import fetch
from ..manifest import Item

AUDIO_BASE = "https://groups.inf.ed.ac.uk/ami/AMICorpusMirror/amicorpus"
ANNOTATIONS_URL = (
    "https://groups.inf.ed.ac.uk/ami/AMICorpusAnnotations/ami_public_manual_1.6.2.zip"
)
SPLIT_FILE = Path(__file__).with_name("ami_test.txt")

#: NXT namespace, used for the id attributes that link words to speakers.
NITE = "{http://nite.sourceforge.net/}"

#: Headset channels per meeting. AMI meetings are four-participant; a meeting
#: with a missing channel is skipped rather than silently scored short.
CHANNELS = "ABCD"


def meetings() -> list[str]:
    lines = [
        line.strip()
        for line in SPLIT_FILE.read_text().splitlines()
        if line.strip() and not line.startswith("#")
    ]
    if not lines:
        raise RuntimeError(f"{SPLIT_FILE} lists no meetings")
    return lines


def prepare(data_dir: Path) -> Path:
    """Fetch annotations and every headset channel of every test meeting.

    Tens of gigabytes. `fetch.download` skips what is already there, so an
    interrupted run resumes by being run again.
    """
    root = data_dir / "ami"
    annotations = fetch.download(
        ANNOTATIONS_URL, root / "ami_public_manual_1.6.2.zip", key="ami-annotations-1.6.2"
    )
    fetch.extract(annotations, root / "annotations", marker="words")

    for meeting in meetings():
        for channel_index, _ in enumerate(CHANNELS):
            name = f"{meeting}.Headset-{channel_index}.wav"
            fetch.download(
                f"{AUDIO_BASE}/{meeting}/audio/{name}",
                root / "amicorpus" / meeting / "audio" / name,
                key=f"ami-{name}",
            )
    return root


def items(root: Path) -> Iterator[Item]:
    """One item per (meeting, speaker): the whole headset channel and its words.

    Whole channels rather than per-utterance clips, deliberately. Jotter
    transcribes a meeting, not a sentence, so scoring it a sentence at a time
    would hide exactly the errors that matter — the ones the segmenter makes.
    The VAD does the cutting, as it would in production, and the reference is
    everything that speaker said.
    """
    root = Path(root)
    words_dir = root / "annotations" / "words"
    if not words_dir.exists():
        raise FileNotFoundError(f"{words_dir} is missing — run `bench fetch --corpus ami-ihm-test`")

    for meeting in meetings():
        for channel_index, channel in enumerate(CHANNELS):
            audio = root / "amicorpus" / meeting / "audio" / f"{meeting}.Headset-{channel_index}.wav"
            words_file = words_dir / f"{meeting}.{channel}.words.xml"
            if not audio.exists() or not words_file.exists():
                continue

            text = " ".join(w for _, _, w in read_words(words_file))
            if not text.strip():
                continue

            yield Item(
                id=f"{meeting}.{channel}",
                audio=audio,
                reference=text,
                speaker=f"{meeting}.{channel}",
                meeting=meeting,
            )


def read_words(path: Path) -> list[tuple[float, float, str]]:
    """(start, end, word) from one NXT words file.

    Punctuation elements are dropped — AMI marks them `punc="true"` — because
    the reference should be what was said, and the recogniser emits no
    punctuation to be scored against anyway. Vocal sounds (`<vocalsound>`,
    `<gap>`) are dropped for the same reason: scoring a model down for not
    transcribing a laugh measures nothing.
    """
    words: list[tuple[float, float, str]] = []
    for element in ET.parse(path).getroot():
        if not element.tag.endswith("w") or element.get("punc") == "true":
            continue
        text = (element.text or "").strip()
        if not text:
            continue
        start = _time(element.get("starttime"))
        end = _time(element.get("endtime"))
        words.append((start, end, text))
    return words


def _time(value: str | None) -> float | None:
    """AMI leaves times off words it could not align. Those words are still
    said, so they keep their text and lose only their position.

    `None` rather than NaN: these timings are written to `reference.json`, and
    `json.dumps(float("nan"))` emits a bare `NaN`, which Python reads back but
    is not valid JSON and which any other reader rejects.
    """
    try:
        return float(value)  # type: ignore[arg-type]
    except (TypeError, ValueError):
        return None


def normalise_tag(text: str) -> str:
    """Strip the bracketed markup AMI's transcripts carry."""
    return re.sub(r"[<\[][^>\]]*[>\]]", " ", text)
