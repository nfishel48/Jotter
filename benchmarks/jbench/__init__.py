"""Jotter's benchmark harness.

Measures how accurate Jotter's transcription actually is, on the corpora the
speech-recognition field uses and with the scoring the published leaderboards
use, so the answer is a number other people's numbers can be set beside.

See `docs/BENCHMARKS.md` for the method and `benchmarks/README.md` for how to
run it.
"""

__all__ = ["audio", "corpora", "fetch", "manifest", "normalize", "report", "score"]
