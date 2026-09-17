#!/usr/bin/env /usr/bin/python3
"""Grade an echo-cancellation pass against the original recording.

Usage:
    scripts/check_aec.py recordings/<dir>

The directory needs mic.wav, system.wav and mic_aec.wav. meta.json is read when
present, to show what the pass thought it did next to what it actually did.

Two numbers decide it, and the second is the one that matters:

  ERLE          how much echo came out, over stretches where the remote side was
                talking and the user was not.
  near-end      how much of the *user's own voice* was lost, over stretches
                where only they were talking. An echo canceller that quietly
                eats the near end scores beautifully on ERLE alone, which is why
                a pass is never graded on ERLE alone.

An independent implementation of the same measurements, this one: the Rust side
computes its own figures and writes them to meta.json, and two implementations
agreeing is worth far more than one implementation agreeing with itself.

Note: use /usr/bin/python3 explicitly. The `python3` on PATH here is a homebrew
alias pointing at a binary that no longer exists. No numpy either — hence the
hand-rolled band filtering below.
"""
import json
import math
import os
import struct
import sys
import wave

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from analyze_wav import frame_levels  # noqa: E402

# Frames shorter than this smear speech and echo together; longer than this and
# a single frame spans both a remote word and the user's reply.
FRAME_MS = 100

# The far end keeps counting as active for this long after it stops, because its
# echo is still arriving. Without it, frames during a pause between remote words
# look near-only while the echo tail decays, and the canceller correctly
# removing that tail reads as damage. Mirrors FAR_HANGOVER_MS in process.rs — if
# the two ever disagree, this script stops being an independent check.
FAR_HANGOVER_MS = 200

# Absolute floor, in i16 rms, below which a frame carries no signal.
FLOOR_RMS = 300.0

# --- The gate ---------------------------------------------------------------
# 10 dB: AEC3 measured 20.8 dB on the reference recording, so this leaves ample
# headroom for a quieter room while still being a threshold nothing broken can
# reach — the predecessor scored 1.2 dB on the same files.
MIN_ERLE_DB = 10.0
# -1.0 dB: below where transcription accuracy moves, and the reference recording
# came in at -0.4 dB.
MAX_NEAR_LOSS_DB = -1.0


def active_threshold(series):
    """max(FLOOR_RMS, p90 / 10) — a floor, or 20 dB below this track's own peak.

    Relative to the track because "the remote side is talking" is a statement
    about this recording, not about an absolute number of i16 counts.
    """
    if not series:
        return FLOOR_RMS
    ordered = sorted(series)
    p90 = ordered[len(ordered) * 9 // 10]
    return max(FLOOR_RMS, p90 / 10.0)


def classify(mic, far):
    """Per-frame activity labels, with the far-end hangover applied."""
    near_on = active_threshold(mic)
    far_on = active_threshold(far)
    hangover = max(1, FAR_HANGOVER_MS // FRAME_MS)

    labels = []
    since_far = len(mic) + 1
    for i in range(min(len(mic), len(far))):
        since_far = 0 if far[i] > far_on else since_far + 1
        f = since_far <= hangover
        n = mic[i] > near_on
        labels.append(
            "double" if n and f else "far" if f else "near" if n else "silence"
        )
    return labels


def level_change_db(before, after, indices):
    """Level change over `indices`, in dB. **Negative means level was lost.**

    Same sign convention as `AecStats::near_gain_db` on the Rust side, and
    deliberately so: an earlier version of this script returned the negation,
    which inverted the near-end gate — a pass that destroyed the user's voice
    scored a large positive number and sailed through a `< -1.0 dB` check.
    One convention, stated once, used everywhere.
    """
    a = sum(before[i] ** 2 for i in indices)
    b = sum(after[i] ** 2 for i in indices)
    if a <= 0 or b <= 0:
        return None
    return 10 * math.log10(b / a)


def erle_db(before, after, indices):
    """Echo removed, in dB. Positive is good — the inverse of a level change."""
    change = level_change_db(before, after, indices)
    return None if change is None else -change


def band_energy(path, indices, bands, rate):
    """Energy per band over selected frames, via a hand-rolled biquad bandpass.

    Per-band figures are reported, never gated: they are the diagnostic that
    says *where* cancellation is working, which is what you need when the
    headline number disappoints. Low coherence at high frequencies is expected
    on a laptop speaker and is not a defect.
    """
    wanted = set(indices)
    # Stop once past the last frame of interest. The filters have to run
    # continuously up to that point — restarting them per frame would ring — but
    # there is no reason to keep filtering the rest of the recording.
    last = max(wanted, default=-1)
    with wave.open(path) as w:
        frame = max(1, rate * FRAME_MS // 1000)
        out = [0.0] * len(bands)
        # Biquad state per band, carried across frames so the filters stay
        # continuous rather than restarting and ringing on every frame.
        state = [[0.0, 0.0, 0.0, 0.0] for _ in bands]
        coeffs = []
        for lo, hi in bands:
            f0 = math.sqrt(lo * hi)
            q = f0 / max(hi - lo, 1.0)
            w0 = 2 * math.pi * f0 / rate
            alpha = math.sin(w0) / (2 * q)
            a0 = 1 + alpha
            coeffs.append(
                (
                    alpha / a0,
                    0.0,
                    -alpha / a0,
                    (-2 * math.cos(w0)) / a0,
                    (1 - alpha) / a0,
                )
            )

        index = 0
        while True:
            raw = w.readframes(frame * 64)
            if not raw:
                return out
            for i in range(0, len(raw) - frame * 2 + 1, frame * 2):
                chunk = struct.unpack("<%dh" % frame, raw[i : i + frame * 2])
                use = index in wanted
                for b, (b0, b1, b2, a1, a2) in enumerate(coeffs):
                    x1, x2, y1, y2 = state[b]
                    acc = 0.0
                    for v in chunk:
                        y = b0 * v + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2
                        x2, x1 = x1, v
                        y2, y1 = y1, y
                        if use:
                            acc += y * y
                    state[b] = [x1, x2, y1, y2]
                    out[b] += acc
                index += 1
                if index > last:
                    return out


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 1
    d = sys.argv[1]

    paths = {n: os.path.join(d, f"{n}.wav") for n in ("mic", "system", "mic_aec")}
    missing = [n for n, p in paths.items() if not os.path.exists(p)]
    if missing:
        print(f"missing {', '.join(n + '.wav' for n in missing)} in {d}")
        if "mic_aec" in missing:
            print("run:  cargo run --release -- process " + d)
        return 1

    rate, mic_levels = frame_levels(paths["mic"], FRAME_MS)
    _, far_levels = frame_levels(paths["system"], FRAME_MS)
    _, aec_levels = frame_levels(paths["mic_aec"], FRAME_MS)

    mic = [r for _, r in mic_levels]
    far = [r for _, r in far_levels]
    aec = [r for _, r in aec_levels]
    n = min(len(mic), len(far), len(aec))
    mic, far, aec = mic[:n], far[:n], aec[:n]

    labels = classify(mic, far)
    groups = {}
    for i, label in enumerate(labels):
        groups.setdefault(label, []).append(i)

    secs = FRAME_MS / 1000.0
    print(f"recording: {d}")
    print(f"  {n * secs:.0f}s at {rate} Hz, {FRAME_MS}ms frames")

    meta_path = os.path.join(d, "meta.json")
    if os.path.exists(meta_path):
        with open(meta_path) as f:
            meta = json.load(f)
        info = meta.get("aec")
        if info:
            print(
                f"  pass says: {info.get('delay_frames', 0) * 1000 / rate:.1f}ms delay"
                f" ({info.get('delay_source', '?')}),"
                f" AEC3 said {info.get('reported_delay_ms', '?')}ms,"
                f" suppressor {'on' if info.get('residual_suppression') else 'off'}"
            )
            if info.get("bypassed"):
                print(f"  pass declined: {info['bypassed']}")

    print()
    print("  activity      secs    level change   (negative = removed)")
    for label in ("far", "double", "near", "silence"):
        idx = groups.get(label, [])
        if not idx:
            continue
        change = level_change_db(mic, aec, idx)
        shown = "n/a" if change is None else f"{change:+6.2f} dB"
        print(f"  {label:<12} {len(idx) * secs:5.0f}    {shown}")

    far_only = groups.get("far", [])
    near_only = groups.get("near", [])

    erle = erle_db(mic, aec, far_only)
    near_change = level_change_db(mic, aec, near_only)

    print()
    if far_only:
        bands = [(150, 300), (300, 1000), (1000, 2000), (2000, 4000), (4000, 8000)]
        # Bounded sample: five hand-rolled biquads over a nine-minute file cost
        # about a minute of pure Python, and this figure is a diagnostic that is
        # reported and never gated. 200 frames is 20s of echo-only audio.
        sample = far_only[: min(len(far_only), 200)]
        before = band_energy(paths["mic"], sample, bands, rate)
        after = band_energy(paths["mic_aec"], sample, bands, rate)
        parts = []
        for (lo, hi), a, b in zip(bands, before, after):
            if a > 0 and b > 0:
                parts.append(f"{lo}-{hi}:{10 * math.log10(a / b):.0f}")
        print(f"  echo removed per band (Hz:dB)   {'  '.join(parts)}")

    verdict = True
    if erle is None:
        print("  FAIL  no echo-only stretches to measure — was anything playing?")
        verdict = False
    elif erle < MIN_ERLE_DB:
        print(f"  FAIL  only {erle:.1f} dB of echo removed (want >= {MIN_ERLE_DB:.0f})")
        verdict = False
    else:
        print(f"  PASS  {erle:.1f} dB of echo removed (want >= {MIN_ERLE_DB:.0f})")

    if near_change is None:
        print("  ----  no near-only stretches; near-end damage NOT verified")
    elif near_change < MAX_NEAR_LOSS_DB:
        print(
            f"  FAIL  your voice lost {-near_change:.2f} dB "
            f"(limit {MAX_NEAR_LOSS_DB:.1f}) — the canceller is cutting into it"
        )
        verdict = False
    else:
        print(
            f"  PASS  your voice moved {near_change:+.2f} dB "
            f"(limit {MAX_NEAR_LOSS_DB:.1f})"
        )

    print()
    print("VERDICT:", "PASS" if verdict else "FAIL")
    return 0 if verdict else 1


if __name__ == "__main__":
    sys.exit(main())
