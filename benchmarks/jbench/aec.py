"""Echo cancellation: a controlled test set, and the number that actually matters.

`Cargo.toml` justifies the AEC3 dependency with figures from one reference
recording — 20 dB of echo return loss, 17.6 dB through double-talk, 0.29 dB lost
off the user's own voice. Good numbers, but one recording is an anecdote, and
none of them say the thing a user cares about, which is whether the transcript
came out better.

This builds a test set where the ground truth is known by construction, sweeps
the one parameter that decides whether cancellation is easy or hopeless, and
reports both kinds of number.

**The recording.** Each item is three regimes back to back, because that is what
`process::classify` sorts frames into and what `AecStats` reports separately:

    | near only        | far only          | double talk              |
    | you speaking     | the room speaking | both at once             |

- near-only frames give `near_gain_db`: how much of *your* voice the canceller
  ate. The failure an ERLE figure cannot see.
- far-only frames give `erle_db`: the headline, how much echo it removed.
- double-talk frames give `double_talk_gain_db`, the hard case.

**The sweep.** Echo return loss — how far below your voice the speaker bleed
sits — is the parameter. At 24 dB there is barely anything to cancel; at 0 dB
the echo is as loud as you are. Running the range says where the canceller stops
working rather than asserting that it does.

**The verdict.** Every item is transcribed twice, once through `jotter process`
and once with it skipped, and the word error rates are compared. A canceller
that improves ERLE by 20 dB and the transcript by nothing has not helped.

**Run this on real speech.** The near and far signals come from a prepared
corpus for a reason. An adaptive filter finds a tonal or periodic signal
trivially predictable, so synthetic audio produces ERLE figures that swing by
tens of dB between test sets and mean nothing in either direction. Use
`--corpus librispeech-test-clean`; the generated audio in `smoke.py` is for
checking that the plumbing works, never for a number.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path

import numpy as np

from . import paths, recording, room
from .audio import read_mono

#: Seconds per regime. Long enough for AEC3's filter to converge — a two-second
#: far-only stretch would measure its adaptation time rather than its ceiling.
REGIME_SECS = 8.0

#: Echo return loss values to sweep, in dB. Spans "barely audible bleed" to
#: "the speakers are as loud as you are".
DEFAULT_SWEEP = (0.0, 6.0, 12.0, 18.0, 24.0)


@dataclass
class Condition:
    """One synthesised echo scenario."""

    item_id: str
    erl_db: float
    directory: Path
    reference: str


def build(
    clips: list[tuple[str, Path, str]],
    into: Path,
    sweep: tuple[float, ...] = DEFAULT_SWEEP,
    rt60: float = 0.3,
    delay_ms: float = 8.0,
    rir: Path | None = None,
) -> list[Condition]:
    """Build the sweep from `(id, audio_path, transcript)` triples.

    Needs four clips per item — two near, two far — so speech in the double-talk
    regime is not a repeat of speech the recogniser has already seen, which
    would make that regime artificially easy.
    """
    conditions: list[Condition] = []
    for index in range(0, len(clips) - 3, 4):
        near_a, near_b, far_a, far_b = clips[index : index + 4]
        for erl_db in sweep:
            conditions.append(
                _build_one(near_a, near_b, far_a, far_b, erl_db, into, rt60, delay_ms, rir)
            )
    return conditions


def _build_one(near_a, near_b, far_a, far_b, erl_db, into, rt60, delay_ms, rir) -> Condition:
    near_1, rate = read_mono(near_a[1])
    near_2, _ = read_mono(near_b[1])
    far_1, _ = read_mono(far_a[1])
    far_2, _ = read_mono(far_b[1])

    span = int(REGIME_SECS * rate)
    near_1, near_2 = _fit(near_1, span), _fit(near_2, span)
    far_1, far_2 = _fit(far_1, span), _fit(far_2, span)

    response = room.load_response(rir, rate) if rir else room.impulse_response(
        rate, rt60=rt60, delay_ms=delay_ms, seed=0
    )
    silence = np.zeros(span, dtype=np.float32)

    # The two bleed signals are scaled against the *near* speech, so the sweep
    # parameter means the same thing in both regimes. Scaling each against its
    # own far signal instead would make the far-only stretch's level depend on
    # how loudly that particular clip happened to be recorded.
    bleed_1 = room.scale_to_ratio(room.apply(far_1, response), near_1, erl_db)
    bleed_2 = room.scale_to_ratio(room.apply(far_2, response), near_2, erl_db)

    mic = np.concatenate([near_1, bleed_1, near_2 + bleed_2])
    system = np.concatenate([silence, far_1, far_2])

    item_id = f"{near_a[0]}+{far_a[0]}"
    directory = into / f"{item_id}@{erl_db:g}dB"
    recording.write(directory, mic=mic, system=system, rate=rate, device_suffix="aec-sweep")

    # Only the near speaker's words: the far speaker is echo, and a transcript
    # of the mic track that contains it has failed, not succeeded.
    reference = f"{near_a[2]} {near_b[2]}"
    (directory / "reference.json").write_text(
        json.dumps({"reference": reference, "erl_db": erl_db, "regime_secs": REGIME_SECS})
    )
    return Condition(item_id=item_id, erl_db=erl_db, directory=directory, reference=reference)


def _fit(signal: np.ndarray, span: int) -> np.ndarray:
    """Trim or pad a clip to exactly one regime's length.

    Padding with silence rather than looping the clip: a looped clip gives the
    adaptive filter a periodic signal to lock onto, which flatters it.
    """
    if len(signal) >= span:
        return signal[:span].astype(np.float32)
    return np.concatenate([signal, np.zeros(span - len(signal), dtype=np.float32)]).astype(
        np.float32
    )


def measure(condition: Condition) -> dict:
    """Run the echo pass and read back what it recorded about itself."""
    import subprocess

    jotter = paths.cargo_binary("jotter")
    result = subprocess.run(
        [str(jotter), "process", str(condition.directory), "--force"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(f"jotter process failed:\n{result.stdout}{result.stderr}")

    meta = json.loads((condition.directory / "meta.json").read_text())
    aec = meta.get("aec") or {}
    return {
        "id": condition.item_id,
        "erl_db": condition.erl_db,
        # The two figures `AecInfo` persists. `double_talk_gain_db` exists in
        # `AecStats` but is not written to meta.json, so it cannot be reported
        # from here — the double-talk column below is seconds, not gain.
        "erle_db": aec.get("erle_db"),
        "near_gain_db": aec.get("near_gain_db"),
        "reported_delay_ms": aec.get("reported_delay_ms"),
        # The census. Not decoration: it is what explains a bypass, and
        # without it a pass that declined is indistinguishable from one that
        # found nothing to remove.
        "near_only_secs": aec.get("near_only_secs"),
        "far_only_secs": aec.get("far_only_secs"),
        "double_talk_secs": aec.get("double_talk_secs"),
        "bypassed": aec.get("bypassed"),
        # Whether the pass's own verdict cleared the bar in `meta.rs` — the
        # decision that actually decides which track gets transcribed.
        "output": aec.get("path"),
    }


def summarise(rows: list[dict]) -> str:
    """The sweep as a table: where cancellation works, and where it stops."""
    lines = [
        "| ERL (dB) | ERLE (dB) | near-end damage (dB) | far-only (s) | double-talk (s) "
        "| delay (ms) | used |",
        "| ---: | ---: | ---: | ---: | ---: | ---: | :--- |",
    ]
    for row in sorted(rows, key=lambda r: r["erl_db"]):
        lines.append(
            f"| {row['erl_db']:g} "
            f"| {_fmt(row['erle_db'])} "
            f"| {_fmt(row['near_gain_db'])} "
            f"| {_fmt(row['far_only_secs'])} "
            f"| {_fmt(row['double_talk_secs'])} "
            f"| {_fmt(row['reported_delay_ms'], '.0f')} "
            f"| {'yes' if row.get('output') else (row.get('bypassed') or 'no')} |"
        )
    lines += [
        "",
        "`ERL` is how far the speaker bleed sits below the near-end voice — low is hard.",
        "`ERLE` is echo removed, measured on far-only frames; higher is better.",
        "`near-end damage` is level lost off the user's own voice on near-only frames;",
        "near zero is the requirement, and `src/audio/meta.rs` rejects the cancelled",
        "track below -1 dB however good the ERLE looks.",
        "`used` says whether the cancelled track was kept, or names the reason the",
        "pass bypassed.",
        "",
        "**Read the far-only column before the ERLE column.** The activity classifier",
        "(`process::classify`) labels frames by energy, per track, so when the bleed is",
        "loud enough the far-only stretch is labelled double-talk instead — and the pass",
        "bypasses with `no_far_only_windows` because it has nowhere to learn the echo",
        "path. That is a real property of an energy-based classifier and not a fault in",
        "the test set: at low ERL there is genuinely no stretch where the microphone is",
        "quiet while the room is loud. A row with `far-only 0` and no ERLE is that",
        "situation, and it is a result rather than a gap.",
    ]
    return "\n".join(lines)


def _fmt(value, spec: str = ".1f") -> str:
    return "—" if value is None else format(value, spec)
