"""Mozilla Common Voice, English test split.

The corpus that answers the question the other three cannot: does this work for
someone who does not sound like a professional narrator? Crowdsourced clips
recorded on whatever hardware the contributor had, across every accent that
volunteered. Word error rates here are the ones a real user recognises.

**Downloaded by hand.** Common Voice is CC0, but the download is behind a form
on Mozilla's site and a versioned, time-limited URL. Automating around that
would break every release, so this adapter expects an already-extracted corpus
and says where to get one.

Layout:

    cv-corpus-<version>-<date>/en/
        test.tsv       client_id, path, sentence, ...
        clips/*.mp3
"""

from __future__ import annotations

import csv
from pathlib import Path
from typing import Iterator

from ..manifest import Item

INSTRUCTIONS = (
    "Common Voice must be downloaded by hand from "
    "https://commonvoice.mozilla.org/en/datasets (English, Common Voice Corpus). "
    "Extract it, then point the harness at the language directory:\n"
    "  bench prepare --corpus common-voice-test --root <...>/cv-corpus-XX/en"
)


def prepare(data_dir: Path) -> Path:
    """Find an already-extracted corpus under `data_dir`, or explain."""
    root = data_dir / "common-voice"
    candidates = sorted(root.glob("cv-corpus-*/en")) if root.exists() else []
    if not candidates:
        raise FileNotFoundError(INSTRUCTIONS)
    # The newest version present, so adding a corpus does not silently keep
    # scoring against the old one.
    return candidates[-1]


def items(root: Path) -> Iterator[Item]:
    root = Path(root)
    tsv = root / "test.tsv"
    clips = root / "clips"
    if not tsv.exists():
        raise FileNotFoundError(f"{tsv} is missing.\n{INSTRUCTIONS}")

    with tsv.open(newline="") as handle:
        # QUOTE_NONE: sentences contain quotation marks, and letting csv treat
        # them as field delimiters truncates references mid-sentence — which
        # would look like a recogniser inserting words.
        reader = csv.DictReader(handle, delimiter="\t", quoting=csv.QUOTE_NONE)
        for row in reader:
            sentence = (row.get("sentence") or "").strip()
            name = (row.get("path") or "").strip()
            if not sentence or not name:
                continue
            audio = clips / name
            if not audio.exists():
                continue
            yield Item(
                id=Path(name).stem,
                audio=audio,
                reference=sentence,
                speaker=(row.get("client_id") or "")[:16] or None,
            )
