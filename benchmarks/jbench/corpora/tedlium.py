"""TED-LIUM 3, from openslr.org.

Prepared talks: one confident speaker, a good microphone, a live audience. It
sits between LibriSpeech's read audiobooks and AMI's crossfire, and it is on the
Open ASR Leaderboard, so it is worth having for comparability.

**Licence: CC BY-NC-ND 3.0.** Non-commercial and no derivatives. Measuring with
it is fine; redistributing it, or shipping anything derived from it, is not.
That is why it is opt-in rather than part of the default set — see
`docs/BENCHMARKS.md`.

The archive holds several versions of the data. This uses `legacy/test`, which
is the split published figures are computed on.

References come as STM: one segment per line,

    <file> <channel> <speaker> <start> <end> <label> <transcript>

with a marker for stretches the corpus authors say not to score. Those are
skipped — scoring them would measure the harness's willingness to read a spec.
"""

from __future__ import annotations

from pathlib import Path
from typing import Iterator

from .. import fetch
from ..manifest import Item

URL = "https://www.openslr.org/resources/51/TEDLIUM_release-3.tgz"

#: The STM label the corpus uses for segments that must not be scored.
IGNORE = "ignore_time_segment_in_scoring"


def prepare(data_dir: Path) -> Path:
    archive = fetch.download(URL, data_dir / "TEDLIUM_release-3.tgz", key="tedlium-release-3")
    root = fetch.extract(archive, data_dir / "tedlium", marker="TEDLIUM_release-3")
    return root / "TEDLIUM_release-3" / "legacy" / "test"


def items(root: Path) -> Iterator[Item]:
    """One item per STM segment.

    Per-segment rather than per-talk because the STM's own boundaries are what
    the reference is defined against; re-cutting them would mean re-aligning
    the reference, which is a different project.
    """
    root = Path(root)
    stm_dir = root / "stm"
    sph_dir = root / "sph"
    if not stm_dir.exists():
        raise FileNotFoundError(f"{stm_dir} is missing — run `bench fetch --corpus tedlium-test`")

    for stm in sorted(stm_dir.glob("*.stm")):
        talk = stm.stem
        audio = _audio_for(sph_dir, talk)
        if audio is None:
            raise FileNotFoundError(f"{stm} has no matching audio in {sph_dir}")

        for number, line in enumerate(stm.read_text(errors="replace").splitlines()):
            parts = line.split(maxsplit=6)
            if len(parts) < 7:
                continue
            _, _, speaker, start, end, label, text = parts
            if IGNORE in label or IGNORE in text:
                continue
            text = text.strip()
            if not text:
                continue

            yield Item(
                id=f"{talk}-{number:04d}",
                audio=audio,
                reference=text,
                speaker=speaker,
                extra={"start": float(start), "end": float(end)},
            )


def _audio_for(sph_dir: Path, talk: str) -> Path | None:
    """TED-LIUM ships SPHERE; some mirrors repackage it as WAV."""
    for suffix in (".sph", ".wav"):
        candidate = sph_dir / f"{talk}{suffix}"
        if candidate.exists():
            return candidate
    return None
