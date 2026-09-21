"""LibriSpeech, from openslr.org.

The reference corpus for English ASR and the first one to run: read audiobook
speech, close-talking, almost no disfluency. A figure far off the published one
means the harness is wrong, not the model — which is exactly what makes it
useful as the harness's own acceptance test.

Layout inside the archive:

    LibriSpeech/test-clean/<speaker>/<chapter>/
        <speaker>-<chapter>-<utt>.flac
        <speaker>-<chapter>.trans.txt      "<id> THE TRANSCRIPT IN CAPITALS"

References are unpunctuated upper case, which the normaliser handles.
"""

from __future__ import annotations

from pathlib import Path
from typing import Iterator

from .. import fetch
from ..manifest import Item

BASE_URL = "https://www.openslr.org/resources/12"


def prepare(data_dir: Path, split: str) -> Path:
    """Download and unpack one split, returning its root."""
    archive = fetch.download(
        f"{BASE_URL}/{split}.tar.gz",
        data_dir / f"librispeech-{split}.tar.gz",
        key=f"librispeech-{split}.tar.gz",
    )
    root = fetch.extract(archive, data_dir / "librispeech", marker=f"LibriSpeech/{split}")
    return root / "LibriSpeech" / split


def items(root: Path, split: str) -> Iterator[Item]:
    root = Path(root)
    if not root.exists():
        raise FileNotFoundError(f"{root} is missing — run `bench fetch --corpus librispeech-{split}`")

    # Sorted so a `--limit` run is the same subset every time. An unstable
    # subset makes two "first 50 utterances" runs incomparable for no reason.
    for transcript in sorted(root.rglob("*.trans.txt")):
        for line in transcript.read_text().splitlines():
            if not line.strip():
                continue
            item_id, _, text = line.partition(" ")
            audio = transcript.parent / f"{item_id}.flac"
            if not audio.exists():
                raise FileNotFoundError(f"{transcript} names {item_id}, but {audio} is missing")
            speaker = item_id.split("-")[0]
            yield Item(id=item_id, audio=audio, reference=text, speaker=speaker)
