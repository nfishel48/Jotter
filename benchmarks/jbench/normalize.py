"""Text normalisation, which is what decides whether a WER means anything.

Word error rate is not a property of a transcript on its own. "Mr. Smith paid
$5" against "mister smith paid five dollars" is either 0% or 100% depending
entirely on what you did to the strings first, and every published ASR figure
has made that choice silently. Comparing Jotter's number against a leaderboard
number computed under a different normaliser is not a comparison at all.

The HuggingFace Open ASR Leaderboard — where the published Parakeet, Whisper and
Canary figures live — uses Whisper's `EnglishTextNormalizer`. So that is what
this module runs, and it runs the *real* one rather than a copy:

- `whisper` mode imports `EnglishTextNormalizer` from `transformers` and feeds it
  OpenAI's own `english.json` British-to-American spelling map. This is the only
  mode whose numbers may be set beside someone else's.
- `basic` mode is a deliberately simple fallback for when that is unavailable —
  offline machines, and CI, which has no network and no business asserting a
  WER anyway. It lowercases, strips brackets and punctuation, expands the same
  contraction table, and stops there.

Reimplementing Whisper's normaliser from memory was considered and rejected. Its
number handling ("twenty twenty three" -> "2023", "a quarter of a million") is
several hundred lines of special cases, and a version that is subtly wrong is
worse than no version: it produces a plausible WER that quietly is not the one
everybody else is quoting. A fallback that is *obviously* different is honest;
a near-copy that is 0.3% off is not.

Which mode ran is recorded in every result, and `report.py` refuses to print the
leaderboard comparison column for a run normalised in `basic` mode.
"""

from __future__ import annotations

import json
import re
import unicodedata
from dataclasses import dataclass
from pathlib import Path

# Where `fetch.py` puts OpenAI's spelling map, and the lock file pinning it.
SPELLING_PATH = Path(__file__).with_name("english.json")
LOCK_PATH = Path(__file__).with_name("normalizer.lock.json")

# Filler the leaderboard's normaliser drops before scoring. Kept identical to
# Whisper's `ignore_patterns` so `basic` mode at least agrees with `whisper` on
# what is not a word.
FILLER = r"\b(hmm|mm|mhm|mmm|uh|um)\b"

# Whisper's contraction table, which `basic` mode reuses. Order matters: the
# multi-word entries have to fire before the general `n't` / `'s` rules below
# them, or "won't" becomes "won not".
CONTRACTIONS: dict[str, str] = {
    r"\bwon't\b": "will not",
    r"\bcan't\b": "can not",
    r"\blet's\b": "let us",
    r"\bain't\b": "aint",
    r"\by'all\b": "you all",
    r"\bwanna\b": "want to",
    r"\bgotta\b": "got to",
    r"\bgonna\b": "going to",
    r"\bi'ma\b": "i am going to",
    r"\bimma\b": "i am going to",
    r"\bwoulda\b": "would have",
    r"\bcoulda\b": "could have",
    r"\bshoulda\b": "should have",
    r"\bma'am\b": "madam",
    r"\bmr\b": "mister ",
    r"\bmrs\b": "missus ",
    r"\bst\b": "saint ",
    r"\bdr\b": "doctor ",
    r"\bprof\b": "professor ",
    r"\bcapt\b": "captain ",
    r"\bgov\b": "governor ",
    r"\bald\b": "alderman ",
    r"\bgen\b": "general ",
    r"\bsen\b": "senator ",
    r"\brep\b": "representative ",
    r"\bpres\b": "president ",
    r"\brev\b": "reverend ",
    r"\bhon\b": "honorable ",
    r"\basst\b": "assistant ",
    r"\bassoc\b": "associate ",
    r"\blt\b": "lieutenant ",
    r"\bcol\b": "colonel ",
    r"\bjr\b": "junior ",
    r"\bsr\b": "senior ",
    r"\besq\b": "esquire ",
    r"'d been\b": " had been",
    r"'s been\b": " has been",
    r"'d gone\b": " had gone",
    r"'s gone\b": " has gone",
    r"'d done\b": " had done",
    r"'s got\b": " has got",
    r"n't\b": " not",
    r"'re\b": " are",
    r"'s\b": " is",
    r"'d\b": " would",
    r"'ll\b": " will",
    r"'t\b": " not",
    r"'ve\b": " have",
    r"'m\b": " am",
}


@dataclass(frozen=True)
class Normalizer:
    """A normaliser plus the provenance that makes its output interpretable."""

    mode: str
    """`whisper` or `basic`. Only `whisper` is leaderboard-comparable."""

    detail: str
    """Human-readable: library version, spelling-map hash, or why we fell back."""

    _fn: object

    def __call__(self, text: str) -> str:
        return self._fn(text)  # type: ignore[operator]

    @property
    def comparable(self) -> bool:
        """Whether a WER scored with this may be quoted against published figures."""
        return self.mode == "whisper"

    def provenance(self) -> dict:
        return {"mode": self.mode, "detail": self.detail, "comparable": self.comparable}


def load(prefer: str = "whisper") -> Normalizer:
    """The best normaliser available, or the fallback with a reason attached.

    Never raises for a missing dependency: a machine that cannot score
    comparably should still be able to run the harness and get a number it
    knows not to publish.
    """
    if prefer == "basic":
        return _basic("requested explicitly")

    try:
        # transformers announces on import that it cannot find PyTorch. True,
        # and irrelevant — the normaliser is pure Python string handling and
        # touches no model. The warning would otherwise land in the middle of
        # every benchmark run and look like a problem.
        import os

        os.environ.setdefault("TRANSFORMERS_VERBOSITY", "error")
        os.environ.setdefault("TRANSFORMERS_NO_ADVISORY_WARNINGS", "1")

        from transformers.models.whisper.english_normalizer import EnglishTextNormalizer
    except ImportError as e:
        return _basic(f"transformers unavailable ({e.__class__.__name__}) — run benchmarks/bootstrap.sh")

    if not SPELLING_PATH.exists():
        return _basic(f"{SPELLING_PATH.name} not fetched — run `bench fetch --normalizer`")

    mapping = json.loads(SPELLING_PATH.read_text())
    digest = _spelling_digest()
    if not _lock_matches(digest):
        return _basic(
            f"{SPELLING_PATH.name} does not match {LOCK_PATH.name} "
            "— refetch it, or delete the lock if the change is intended"
        )

    try:
        import transformers

        version = transformers.__version__
    except Exception:  # pragma: no cover - only if transformers is very unusual
        version = "unknown"

    fn = EnglishTextNormalizer(mapping)
    return Normalizer(
        mode="whisper",
        detail=f"transformers {version}, english.json sha256:{digest[:12]} ({len(mapping)} entries)",
        _fn=fn,
    )


def _spelling_digest() -> str:
    import hashlib

    return hashlib.sha256(SPELLING_PATH.read_bytes()).hexdigest()


def _lock_matches(digest: str) -> bool:
    """A spelling map nobody pinned is a spelling map that can change under you.

    `fetch.py` writes the lock on first download and it is committed, so every
    later run — and every other machine — is scoring against the same table.
    An absent lock is treated as a match so the very first fetch can create it.
    """
    if not LOCK_PATH.exists():
        return True
    return json.loads(LOCK_PATH.read_text()).get("english_json_sha256") == digest


def _basic(reason: str) -> Normalizer:
    return Normalizer(mode="basic", detail=reason, _fn=basic_normalize)


def basic_normalize(text: str) -> str:
    """Lowercase, de-punctuate, expand contractions, collapse whitespace.

    Explicitly *not* Whisper's normaliser: no number handling, no spelling
    standardisation. Good enough to tell a working pipeline from a broken one,
    and not good enough to quote.
    """
    s = text.lower()
    s = re.sub(r"[<\[][^>\]]*[>\]]", "", s)  # [noise], <unk> and friends
    s = re.sub(r"\(([^)]+?)\)", "", s)
    s = re.sub(FILLER, "", s)
    s = re.sub(r"\s+'", "'", s)

    for pattern, replacement in CONTRACTIONS.items():
        s = re.sub(pattern, replacement, s)

    s = re.sub(r"(\d),(\d)", r"\1\2", s)  # 1,000 -> 1000
    s = _strip_symbols(s)
    return re.sub(r"\s+", " ", s).strip()


def _strip_symbols(s: str) -> str:
    """Drop combining marks, turn every other symbol or punctuation into a space.

    Decomposing first (NFKD) is what makes "café" and "cafe" score equal, which
    they should: a recogniser that omits an accent has not made a word error.
    """
    return "".join(
        ""
        if unicodedata.category(c) == "Mn"
        else " "
        if unicodedata.category(c)[0] in "MSP"
        else c
        for c in unicodedata.normalize("NFKD", s)
    )
