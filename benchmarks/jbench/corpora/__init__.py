"""Corpus adapters: turning a downloaded archive into scorable items.

Every corpus stores its audio and its transcripts differently, and none of that
belongs anywhere near the scorer. An adapter's whole job is to yield
[`manifest.Item`]s; everything downstream is corpus-agnostic, which is what
makes adding Earnings-22 or VoxPopuli later a new file rather than a change.

The shipped set is English, because the default model (NVIDIA Parakeet TDT 0.6b
v2) is English-only — see `src/models.rs`. Four corpora, chosen to disagree with
each other:

- **LibriSpeech** — read audiobooks, clean and easy. The universal reference
  point, and the number that catches a broken harness fastest.
- **AMI** — real meetings, overlapping speech, far from clean. The closest thing
  to what Jotter is actually for, and where the interesting failures are.
- **TED-LIUM** — prepared talks. Between the two, and on the leaderboard.
- **Common Voice** — crowdsourced, every accent and microphone there is. The
  one that says whether Jotter works for people who do not sound like an
  audiobook narrator.

Licences differ and matter. TED-LIUM is CC BY-NC-ND: fine for measuring, not
fine for a commercial redistribution, which is why it is opt-in rather than part
of the default set.
"""

from __future__ import annotations

from typing import Callable, Iterator

from ..manifest import Item
from . import ami, common_voice, librispeech, tedlium


class Corpus:
    """What every adapter provides."""

    def __init__(
        self,
        name: str,
        licence: str,
        url: str,
        prepare: Callable,
        items: Callable[..., Iterator[Item]],
        commercial_ok: bool = True,
        notes: str = "",
    ):
        self.name = name
        self.licence = licence
        self.url = url
        self.prepare = prepare
        self.items = items
        self.commercial_ok = commercial_ok
        self.notes = notes


REGISTRY: dict[str, Corpus] = {
    "librispeech-test-clean": Corpus(
        name="librispeech-test-clean",
        licence="CC BY 4.0",
        url="https://www.openslr.org/12",
        prepare=lambda data_dir: librispeech.prepare(data_dir, "test-clean"),
        items=lambda root: librispeech.items(root, "test-clean"),
        notes="Read audiobooks. The easy end; a bad number here means a broken harness.",
    ),
    "librispeech-test-other": Corpus(
        name="librispeech-test-other",
        licence="CC BY 4.0",
        url="https://www.openslr.org/12",
        prepare=lambda data_dir: librispeech.prepare(data_dir, "test-other"),
        items=lambda root: librispeech.items(root, "test-other"),
        notes="The deliberately harder LibriSpeech split.",
    ),
    "ami-ihm-test": Corpus(
        name="ami-ihm-test",
        licence="CC BY 4.0",
        url="https://groups.inf.ed.ac.uk/ami/corpus/",
        prepare=ami.prepare,
        items=ami.items,
        notes="Real meetings, headset microphones. What Jotter is for.",
    ),
    "tedlium-test": Corpus(
        name="tedlium-test",
        licence="CC BY-NC-ND 3.0",
        url="https://www.openslr.org/51",
        prepare=tedlium.prepare,
        items=tedlium.items,
        commercial_ok=False,
        notes="Prepared talks. Non-commercial licence — measure with it, do not ship it.",
    ),
    "common-voice-test": Corpus(
        name="common-voice-test",
        licence="CC0 1.0",
        url="https://commonvoice.mozilla.org/en/datasets",
        prepare=common_voice.prepare,
        items=common_voice.items,
        notes="Crowdsourced. Accent and microphone diversity, which the others lack.",
    ),
}


def get(name: str) -> Corpus:
    if name not in REGISTRY:
        known = "\n  ".join(sorted(REGISTRY))
        raise KeyError(f"unknown corpus {name!r}. Known:\n  {known}")
    return REGISTRY[name]
