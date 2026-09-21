"""Proving the fast path is the real path.

`jotter-bench` exists to avoid reloading a 660 MB model once per utterance, and
the entire argument that its numbers mean anything is that it calls
`transcribe_track` — the same function `jotter transcribe` calls. That is a
claim about the code, and claims about code rot.

So this takes a handful of real corpus clips, puts each one through *both*
paths, and requires the text to match:

- `jotter-bench --segmentation vad` over a manifest, and
- `jotter transcribe --tracks mic` over a recording directory written by
  `recording.py`, which is the production entry point in full.

A mismatch means one of three things, all worth knowing: the bench binary has
drifted, the recording directories the harness synthesises are not what Jotter
expects, or the production path has changed. Any of them invalidates every
number the harness has produced since, which is why this is a command and not a
comment.
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
from pathlib import Path

from . import manifest, paths, recording
from .audio import read_mono


def run_verification(corpus: str, limit: int) -> int:
    items_path = paths.WORK / corpus / "items.jsonl"
    if not items_path.exists():
        print(
            f"{items_path} is missing — run `bench prepare --corpus {corpus}` first",
            file=sys.stderr,
        )
        return 1

    items = []
    with items_path.open() as handle:
        for line in handle:
            if line.strip():
                items.append(json.loads(line))
            if len(items) >= limit:
                break

    if not items:
        print(f"{items_path} is empty", file=sys.stderr)
        return 1

    jotter = _jotter_binary()
    bench = paths.cargo_binary()

    with tempfile.TemporaryDirectory(prefix="jotter-verify-") as tmp:
        tmp = Path(tmp)

        # The fast path, over exactly these items.
        subset = tmp / "items.jsonl"
        subset.write_text("".join(json.dumps(i) + "\n" for i in items))
        hypotheses = tmp / "bench.jsonl"
        result = subprocess.run(
            [str(bench), "--manifest", str(subset), "--out", str(hypotheses),
             "--segmentation", "vad"],
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            print(result.stderr, file=sys.stderr)
            return result.returncode
        _, fast = manifest.read_hypotheses(hypotheses)

        # The production path, one recording directory per clip.
        mismatches = []
        for item in items:
            samples, rate = read_mono(Path(item["audio"]))
            directory = recording.write(tmp / "recordings" / item["id"], samples, None, rate)

            produced = subprocess.run(
                [str(jotter), "transcribe", str(directory), "--tracks", "mic", "--force"],
                capture_output=True,
                text=True,
            )
            if produced.returncode != 0:
                print(produced.stdout, produced.stderr, file=sys.stderr)
                return produced.returncode

            real_text, _ = recording.read_transcript(directory)
            fast_text = fast.get(item["id"], {}).get("text", "")
            if _squash(real_text) != _squash(fast_text):
                mismatches.append((item["id"], fast_text, real_text))

    for item_id, fast_text, real_text in mismatches:
        print(f"\nMISMATCH {item_id}")
        print(f"  jotter-bench:      {fast_text}")
        print(f"  jotter transcribe: {real_text}")

    if mismatches:
        print(
            f"\n{len(mismatches)} of {len(items)} clips disagree. "
            "Benchmark numbers from the fast path do not describe the shipped "
            "pipeline until this is resolved.",
            file=sys.stderr,
        )
        return 1

    print(f"{len(items)} clips: jotter-bench and jotter transcribe agree exactly.")
    return 0


def _squash(text: str) -> str:
    """Compare on words, not whitespace. The two paths join segments the same
    way today; a difference in spacing is still not a difference in what was
    recognised, and failing on one would be a false alarm."""
    return " ".join(text.split())


def _jotter_binary() -> Path:
    return paths.cargo_binary("jotter")
