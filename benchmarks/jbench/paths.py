"""Where everything lives.

Resolved from this file rather than the working directory: a benchmark run is
long, and discovering at the end that the results went somewhere unexpected
because of where the shell happened to be is an avoidable way to lose it.
"""

from __future__ import annotations

import os
from pathlib import Path

BENCHMARKS = Path(__file__).resolve().parent.parent
REPO = BENCHMARKS.parent

#: Downloaded corpora. Gitignored — gigabytes, and not ours to redistribute.
DATA = Path(os.environ.get("JOTTER_BENCH_DATA", BENCHMARKS / "data"))

#: Prepared manifests and raw hypotheses. Gitignored: regenerable.
WORK = Path(os.environ.get("JOTTER_BENCH_WORK", BENCHMARKS / "work"))

#: Scored summaries. Committed — this is the evidence.
RESULTS = BENCHMARKS / "results"


def cargo_binary(name: str = "jotter-bench") -> Path:
    """The built binary, release preferred.

    A debug build of an ONNX pipeline is slow enough to distort a real-time
    factor by an order of magnitude, so release is what a real run should use —
    but a debug build is accepted, because a smoke test should not need a
    release compile.
    """
    for profile in ("release", "debug"):
        candidate = REPO / "target" / profile / name
        if candidate.exists():
            return candidate
    raise FileNotFoundError(
        f"{name} is not built. From {REPO}:\n"
        f"  cargo build --release -p jotter-cli --features bench"
    )
