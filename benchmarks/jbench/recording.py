"""Writing a recording directory Jotter will accept.

Several parts of the harness need to drive the *real* pipeline — `jotter
process` and `jotter transcribe` — rather than the fast bench path: the
verification that the two agree, and the whole two-track meeting benchmark.
Both commands take a recording directory, not a WAV, so the harness has to
produce one.

That makes this module a second implementation of a format Jotter owns, which
is a thing to be nervous about: a `meta.json` that drifts out of shape would
make the meeting benchmark quietly measure a decline path instead of a
transcript. Two things hold it honest — `Meta` is `Deserialize`, so Jotter
rejects a malformed file outright rather than guessing, and `bench smoke` runs
`jotter transcribe --dry-run` over a directory written here on every CI run,
which fails loudly if the shape has moved.

The fields are `src/audio/meta.rs`. The ones that are not obvious:

- `first_callback_nanos` is what `Meta::track_offset_secs` subtracts to line the
  two tracks up. Synthesised recordings are aligned by construction, so both
  tracks get the same value — an offset invented here would show up as the
  echo canceller looking for a delay that is not there.
- `frames` must match the WAV. The transcription pass reports durations from
  `meta.json` before it opens any audio, so a wrong count is a report that
  disagrees with the recording.
- `stream_errors` is 0. A non-zero count marks a track as suspect, and a
  synthetic track is not suspect, it is synthetic.
"""

from __future__ import annotations

import json
import time
from pathlib import Path

import numpy as np

from .audio import write_mono

#: Arbitrary but stable, so two runs of the same synthesis are byte-identical
#: apart from timestamps.
BASE_CALLBACK_NANOS = 1_000_000_000


def write(
    directory: Path,
    mic: np.ndarray | None,
    system: np.ndarray | None,
    rate: int,
    device_suffix: str = "benchmark",
) -> Path:
    """Write a recording directory with the tracks given, and return it.

    Either track may be `None`: a one-track directory is what the verification
    path uses, since a corpus clip has no system audio to go with it.
    """
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)

    meta: dict = {
        "started_at": time.time(),
        "ended_at": time.time(),
        "mic": None,
        "system": None,
    }

    longest = 0
    for name, samples in (("mic", mic), ("system", system)):
        if samples is None:
            continue
        path = directory / f"{name}.wav"
        write_mono(path, samples, rate)
        longest = max(longest, len(samples))
        meta[name] = {
            "path": f"{name}.wav",
            "device_name": f"{name} ({device_suffix})",
            "device_id": None,
            "sample_rate": rate,
            "channels": 1,
            "source_channels": 1,
            "frames": int(len(samples)),
            # The same instant for both: these tracks start together by
            # construction, and claiming otherwise would hand the echo
            # canceller a delay that is not in the audio.
            "first_callback_nanos": BASE_CALLBACK_NANOS,
            "stream_errors": 0,
        }

    # `duration_secs` is `ended_at - started_at`, and a recording whose stated
    # length disagrees with its audio is the kind of inconsistency that turns
    # into an unexplainable benchmark result later.
    meta["ended_at"] = meta["started_at"] + (longest / rate if rate else 0.0)

    (directory / "meta.json").write_text(json.dumps(meta, indent=2) + "\n")
    return directory


def read_transcript(directory: Path) -> tuple[str, list[dict]]:
    """The text Jotter produced, and the segments behind it.

    Segments are joined with a space to match what `jotter-bench` emits, so the
    two are comparable without either side re-deciding what a word boundary is.
    """
    path = Path(directory) / "transcript.json"
    if not path.exists():
        raise FileNotFoundError(f"{path} was not written — the pass declined; see its output")
    data = json.loads(path.read_text())
    segments = data.get("segments", [])
    return " ".join(s.get("text", "") for s in segments), segments
