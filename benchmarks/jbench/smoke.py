"""Does the harness work at all?

Deliberately says nothing about accuracy. It runs on generated audio, which has
no words in it, so any word error rate it produced would be meaningless — and
that is the point: this is the check that can run in CI, where there is no
corpus, no network and no 660 MB model.

What it actually establishes:

1. `recording.py` writes a `meta.json` that Jotter parses and finds audio in —
   run through `jotter transcribe --dry-run`, which reads the recording and
   reports on it without needing a model. This is the guard on the format
   duplication that the meeting benchmark depends on.
2. The manifest round-trips: what `manifest.write` produces is what
   `manifest.read_*` reads back.
3. The scorer and report run end to end and produce the expected arithmetic on
   known input.
4. If the speech model happens to be downloaded, `jotter-bench` runs for real
   over the generated clips. Skipped, loudly, when it is not — a CI machine
   without the model should say so rather than appear to have tested more than
   it did.
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
from pathlib import Path

import numpy as np

from . import manifest, normalize, paths, recording, report
from . import score as scoring


def run_smoke() -> int:
    failures: list[str] = []
    with tempfile.TemporaryDirectory(prefix="jotter-smoke-") as tmp:
        tmp = Path(tmp)
        clips = _generate_clips(tmp / "clips")

        failures += _check_recording_dir(tmp, clips)
        failures += _check_manifest(tmp, clips)
        failures += _check_scoring(tmp)
        failures += _check_bench_binary(tmp, clips)

    if failures:
        print("\n".join(f"FAIL {f}" for f in failures), file=sys.stderr)
        return 1
    print("smoke: ok")
    return 0


def _generate_clips(directory: Path, count: int = 3, rate: int = 16_000) -> list[Path]:
    """Speech-shaped noise: a few formant-ish tones under an amplitude envelope.

    Not speech, and not trying to be. It only has to be something the WAV
    reader, the resampler and — where it runs — the voice activity detector
    will accept as audio rather than reject as silence.
    """
    rng = np.random.default_rng(0)  # seeded: a flaky smoke test is worse than none
    paths_out = []
    for index in range(count):
        seconds = 1.5 + index * 0.5
        t = np.linspace(0, seconds, int(rate * seconds), endpoint=False)
        signal = sum(
            np.sin(2 * np.pi * f * t) / (i + 2)
            for i, f in enumerate((140.0, 700.0, 1220.0, 2600.0))
        )
        # Syllable-rate envelope, so it is not a continuous tone.
        envelope = 0.5 * (1 + np.sin(2 * np.pi * 3.5 * t))
        samples = 0.25 * envelope * signal + 0.01 * rng.standard_normal(len(t))

        path = directory / f"clip-{index:02d}.wav"
        from .audio import write_mono

        write_mono(path, samples.astype(np.float32), rate)
        paths_out.append(path)
    return paths_out


def _check_recording_dir(tmp: Path, clips: list[Path]) -> list[str]:
    """The important one: Jotter must accept what `recording.py` writes."""
    from .audio import read_mono

    samples, rate = read_mono(clips[0])
    directory = recording.write(tmp / "recording", samples, samples, rate)

    try:
        jotter = paths.cargo_binary("jotter")
    except FileNotFoundError as e:
        return [f"recording dir unverified: {e}"]

    result = subprocess.run(
        [str(jotter), "transcribe", str(directory), "--dry-run"],
        capture_output=True,
        text=True,
    )
    output = result.stdout + result.stderr
    if result.returncode != 0:
        return [f"`jotter transcribe --dry-run` exited {result.returncode}: {output.strip()}"]
    # The decline this is allowed to hit is a missing model. "No audio" means
    # the meta.json or the WAV is wrong, which is exactly what this checks.
    if "no_audio" in output or "neither track captured any audio" in output:
        return [f"Jotter found no audio in a synthesised recording: {output.strip()}"]
    return []


def _check_manifest(tmp: Path, clips: list[Path]) -> list[str]:
    items = [
        manifest.Item(id=p.stem, audio=p, reference=f"reference for {p.stem}") for p in clips
    ]
    items_path, refs_path = tmp / "items.jsonl", tmp / "references.jsonl"
    written = manifest.write(items, items_path, refs_path)

    if written != len(clips):
        return [f"manifest wrote {written} items, expected {len(clips)}"]

    references = manifest.read_references(refs_path)
    if set(references) != {p.stem for p in clips}:
        return ["manifest ids did not round-trip"]

    lines = [json.loads(line) for line in items_path.read_text().splitlines()]
    if not all(Path(line["audio"]).is_absolute() for line in lines):
        return ["manifest wrote a relative audio path"]
    return []


def _check_scoring(tmp: Path) -> list[str]:
    """Known input, known answer — including the report and its JSON."""
    normalizer = normalize.load("basic")
    result = scoring.Result()
    result.utterances.append(
        scoring.score_pair(
            "a",
            "the quick brown fox jumps over the lazy dog",
            "the quick brown fox jumped over a lazy dog",
            normalizer,
            audio_secs=4.0,
            elapsed_secs=1.0,
        )
    )
    if abs(result.wer - 2 / 9) > 1e-9:
        return [f"scorer gave {result.wer}, expected {2 / 9}"]

    summary = report.build("smoke", result, normalizer, {"segmentation": "none"})
    if summary["comparable_to_published"]:
        return ["basic normalisation was reported as leaderboard-comparable"]
    if "per_utterance" not in summary:
        return ["report omitted per-utterance counts, so `bench compare` cannot work"]

    text = report.to_markdown(summary)
    if "Not comparable" not in text:
        return ["report did not warn that a basic-normalised run is not comparable"]

    json_path, _ = report.write(summary, tmp / "results", "smoke")
    json.loads(json_path.read_text())
    return []


def _check_bench_binary(tmp: Path, clips: list[Path]) -> list[str]:
    try:
        bench = paths.cargo_binary()
    except FileNotFoundError as e:
        print(f"skip: {e}", file=sys.stderr)
        return []

    items_path = tmp / "bench-items.jsonl"
    items_path.write_text(
        "".join(json.dumps({"id": p.stem, "audio": str(p)}) + "\n" for p in clips)
    )
    out = tmp / "bench-out.jsonl"
    result = subprocess.run(
        [str(bench), "--manifest", str(items_path), "--out", str(out),
         "--segmentation", "none"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        message = (result.stderr or result.stdout).strip()
        if "models pull" in message or "not downloaded" in message:
            print(f"skip: speech model not downloaded ({message})", file=sys.stderr)
            return []
        return [f"jotter-bench exited {result.returncode}: {message}"]

    provenance, hypotheses = manifest.read_hypotheses(out)
    if provenance.get("segmentation") != "none":
        return ["jotter-bench provenance did not record the segmentation it ran"]
    if set(hypotheses) != {p.stem for p in clips}:
        return ["jotter-bench did not emit one record per manifest item"]
    return []
