"""Turning a scored run into something a person can act on.

Two outputs, for two readers. The JSON is the record: complete, diffable,
committed under `benchmarks/results/` so a later run can be compared against it
rather than against somebody's memory. The Markdown is the argument: the
headline figure, its error bar, where the errors are, and what it cost.

The one rule enforced here is about comparability. A run normalised in `basic`
mode is not comparable to a published leaderboard figure, so the report refuses
to print it next to one — it says so instead. Getting that wrong is the easiest
way to end up quoting a number that is not what it claims to be.
"""

from __future__ import annotations

import json
import platform
import subprocess
from datetime import datetime, timezone
from pathlib import Path

from . import score as scoring


def provenance(extra: dict | None = None) -> dict:
    """Everything needed to reproduce a run, gathered at the moment it happened."""
    return {
        "when": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "git_sha": _git_sha(),
        "git_dirty": _git_dirty(),
        "host": {
            "platform": platform.platform(),
            "machine": platform.machine(),
            "processor": platform.processor() or platform.machine(),
            "python": platform.python_version(),
        },
        **(extra or {}),
    }


def _git_sha() -> str:
    try:
        return subprocess.run(
            ["git", "rev-parse", "HEAD"],
            capture_output=True,
            text=True,
            check=True,
            cwd=Path(__file__).resolve().parent,
        ).stdout.strip()
    except (subprocess.CalledProcessError, FileNotFoundError):
        return "unknown"


def _git_dirty() -> bool:
    """A result measured from uncommitted code cannot be reproduced from a sha,
    and the report should say so rather than imply otherwise."""
    try:
        out = subprocess.run(
            ["git", "status", "--porcelain"],
            capture_output=True,
            text=True,
            check=True,
            cwd=Path(__file__).resolve().parent,
        ).stdout.strip()
        return bool(out)
    except (subprocess.CalledProcessError, FileNotFoundError):
        return False


def build(
    corpus: str,
    result: scoring.Result,
    normalizer,
    bench_provenance: dict,
    extra: dict | None = None,
) -> dict:
    low, high = scoring.bootstrap_interval(result)
    counts = result.counts
    return {
        "corpus": corpus,
        "wer": result.wer,
        "wer_ci95": [low, high],
        "cer": scoring.character_rate(result),
        "counts": {
            "utterances": len(result.utterances),
            "reference_words": counts.reference_length,
            "hits": counts.hits,
            "substitutions": counts.substitutions,
            "deletions": counts.deletions,
            "insertions": counts.insertions,
        },
        "speed": {
            "audio_secs": result.audio_secs,
            "elapsed_secs": result.elapsed_secs,
            "real_time_factor": result.rtf,
        },
        "normalizer": normalizer.provenance(),
        "comparable_to_published": normalizer.comparable,
        "bench": bench_provenance,
        "run": provenance(extra),
        # Every utterance's counts, not just the worst ones: without these a
        # later `bench compare` cannot run a paired test, and re-running six
        # hours of inference to recover them is not a reasonable ask. Four
        # small integers per utterance, so even AMI stays a modest file.
        "per_utterance": [
            {
                "id": u.id,
                "counts": {
                    "hits": u.counts.hits,
                    "substitutions": u.counts.substitutions,
                    "deletions": u.counts.deletions,
                    "insertions": u.counts.insertions,
                },
            }
            for u in result.utterances
        ],
        "worst": [
            {
                "id": u.id,
                "errors": u.counts.errors,
                "wer": u.rate,
                "reference": u.reference,
                "hypothesis": u.hypothesis,
            }
            for u in result.worst(10)
        ],
    }


def to_markdown(summary: dict) -> str:
    counts = summary["counts"]
    speed = summary["speed"]
    low, high = summary["wer_ci95"]
    bench = summary.get("bench", {})
    run = summary.get("run", {})

    lines = [
        f"# {summary['corpus']}",
        "",
        f"**WER {summary['wer']:.2%}**  (95% CI {low:.2%}–{high:.2%})",
        f"CER {summary['cer']:.2%} · {counts['utterances']} utterances · "
        f"{counts['reference_words']} reference words",
        "",
    ]

    if not summary["comparable_to_published"]:
        lines += [
            "> **Not comparable to published figures.** This run used the fallback "
            f"normaliser: {summary['normalizer']['detail']}. "
            "Leaderboard numbers are computed with Whisper's `EnglishTextNormalizer`; "
            "run `benchmarks/bootstrap.sh` and `bench fetch --normalizer` to match them.",
            "",
        ]

    if run.get("git_dirty"):
        lines += [
            "> **Working tree was dirty.** The recorded commit does not describe the "
            "code that produced this number.",
            "",
        ]

    lines += [
        "## Where the errors are",
        "",
        "| | count | share of errors |",
        "| --- | ---: | ---: |",
    ]
    errors = counts["substitutions"] + counts["deletions"] + counts["insertions"]
    for label in ("substitutions", "deletions", "insertions"):
        share = counts[label] / errors if errors else 0.0
        lines.append(f"| {label} | {counts[label]} | {share:.1%} |")

    lines += [
        "",
        "Deletions dominating points at segmentation — speech the detector never "
        "passed on. Substitutions dominating points at the acoustic model.",
        "",
        "## Cost",
        "",
        f"- {speed['audio_secs'] / 3600:.2f} h of audio in {speed['elapsed_secs'] / 60:.1f} min",
        f"- real-time factor **{speed['real_time_factor']:.3f}** "
        f"({1 / speed['real_time_factor']:.0f}× faster than real time)"
        if speed["real_time_factor"]
        else "- real-time factor unavailable",
        "",
        "## What produced this",
        "",
        f"- jotter {bench.get('jotter_version', '?')} "
        f"(transcribe v{bench.get('transcribe_version', '?')}) at "
        f"`{str(run.get('git_sha', '?'))[:12]}`",
        f"- model `{bench.get('model_id', '?')}` via {bench.get('engine', '?')}, "
        f"{bench.get('threads', '?')} threads",
        f"- segmentation **{bench.get('segmentation', '?')}**"
        + (
            f" (VAD threshold {bench['vad']['threshold']}, "
            f"min silence {bench['vad']['min_silence_secs']}s, "
            f"max speech {bench['vad']['max_speech_secs']}s)"
            if bench.get("segmentation") == "vad" and "vad" in bench
            else ""
        ),
        f"- normaliser {summary['normalizer']['mode']}: {summary['normalizer']['detail']}",
        f"- host {run.get('host', {}).get('platform', '?')}",
        "",
        "## Worst utterances",
        "",
        "Ranked by absolute errors, not rate: a one-word utterance scored 100% "
        "tells you nothing, a thirty-word one scored 40% tells you a lot.",
        "",
    ]

    for item in summary["worst"]:
        lines += [
            f"**{item['id']}** — {item['errors']} errors ({item['wer']:.0%})",
            "",
            f"- ref: {item['reference']}",
            f"- hyp: {item['hypothesis']}",
            "",
        ]

    return "\n".join(lines)


def write(summary: dict, directory: Path, stem: str) -> tuple[Path, Path]:
    directory.mkdir(parents=True, exist_ok=True)
    json_path = directory / f"{stem}.json"
    md_path = directory / f"{stem}.md"
    json_path.write_text(json.dumps(summary, indent=2) + "\n")
    md_path.write_text(to_markdown(summary))
    return json_path, md_path


def compare_table(summaries: list[dict]) -> str:
    """One table across several runs — the thing to paste into a README."""
    lines = [
        "| corpus | segmentation | WER | 95% CI | CER | RTF | comparable |",
        "| --- | --- | ---: | :---: | ---: | ---: | :---: |",
    ]
    for s in summaries:
        low, high = s["wer_ci95"]
        lines.append(
            f"| {s['corpus']} "
            f"| {s.get('bench', {}).get('segmentation', '?')} "
            f"| {s['wer']:.2%} "
            f"| {low:.2%}–{high:.2%} "
            f"| {s['cer']:.2%} "
            f"| {s['speed']['real_time_factor']:.3f} "
            f"| {'yes' if s['comparable_to_published'] else 'no'} |"
        )
    return "\n".join(lines)
