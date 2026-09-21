"""The two files that sit between a corpus and a score.

`jotter-bench` reads audio and writes text; it never sees a reference and has no
idea what a word error rate is. That separation is on purpose — the binary that
produces hypotheses should not be able to see the answers, and a scoring change
should not mean re-running six hours of inference.

So a prepared corpus is two JSONL files:

    items.jsonl       {"id": ..., "audio": ...}   -> jotter-bench
    references.jsonl  {"id": ..., "reference": ...} -> the scorer

joined on `id` afterwards.
"""

from __future__ import annotations

import json
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Iterable, Iterator


@dataclass
class Item:
    """One scorable unit: a clip and what was actually said in it."""

    id: str
    audio: Path
    reference: str
    #: Who is speaking, where the corpus says. Unused for read-speech corpora;
    #: the two-track meeting benchmark needs it to score attribution.
    speaker: str | None = None
    #: Which meeting a clip came from, for corpora that have them.
    meeting: str | None = None
    extra: dict = field(default_factory=dict)


def write(items: Iterable[Item], items_path: Path, references_path: Path) -> int:
    """Write both files, returning how many items were written."""
    items_path.parent.mkdir(parents=True, exist_ok=True)
    references_path.parent.mkdir(parents=True, exist_ok=True)

    count = 0
    with items_path.open("w") as audio_out, references_path.open("w") as ref_out:
        for item in items:
            # Absolute, because `jotter-bench` is run from wherever the user
            # happens to be and a relative path would resolve against the wrong
            # directory in a way that looks like a missing corpus.
            audio_out.write(
                json.dumps({"id": item.id, "audio": str(Path(item.audio).resolve())}) + "\n"
            )
            reference = {"id": item.id, "reference": item.reference}
            if item.speaker is not None:
                reference["speaker"] = item.speaker
            if item.meeting is not None:
                reference["meeting"] = item.meeting
            if item.extra:
                reference["extra"] = item.extra
            ref_out.write(json.dumps(reference) + "\n")
            count += 1
    return count


def read_references(path: Path) -> dict[str, dict]:
    return {row["id"]: row for row in _rows(path)}


def read_hypotheses(path: Path) -> tuple[dict, dict[str, dict]]:
    """Split `jotter-bench` output into its provenance line and the items.

    Raises rather than guessing if the provenance line is missing: a hypothesis
    file whose model and build are unknown cannot be scored into anything
    quotable, and silently scoring it anyway is how an unreproducible number
    ends up in a README.
    """
    provenance: dict | None = None
    items: dict[str, dict] = {}
    for row in _rows(path):
        if row.get("record") == "provenance":
            provenance = row
        else:
            items[row["id"]] = row

    if provenance is None:
        raise ValueError(
            f"{path} has no provenance line — it was not written by jotter-bench, "
            "or was truncated before the first flush"
        )
    return provenance, items


def _rows(path: Path) -> Iterator[dict]:
    with Path(path).open() as handle:
        for number, line in enumerate(handle, start=1):
            if not line.strip():
                continue
            try:
                yield json.loads(line)
            except json.JSONDecodeError as e:
                raise ValueError(f"{path}:{number}: {e}") from e
