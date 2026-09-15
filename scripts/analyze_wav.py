#!/usr/bin/env /usr/bin/python3
"""Report duration / peak / RMS for each WAV in a recording directory.

The important verdict this produces is the difference between an EMPTY file
(no callbacks — wrong device, or the output device was idle) and a SILENT one
(callbacks arrived but every sample is digital zero — the macOS system-audio
permission denial signature).

Usage:
    scripts/analyze_wav.py recordings/<dir>

Note: use /usr/bin/python3 explicitly. The `python3` on PATH here is a
homebrew alias pointing at a binary that no longer exists.
"""
import math
import os
import struct
import sys
import wave


def tone_purity(samples, rate, freq=440.0):
    """Fraction of total energy sitting at `freq`, via the Goertzel algorithm.

    This is what distinguishes a digital tap from a room recording. A loopback
    capture of a sine wave is nearly pure (>0.5); the same tone picked up by a
    microphone through the air arrives with room noise and reflections and
    scores far lower. Without this, speaker bleed makes a swapped-track bug
    look like a working one — both files would simply "have audio".

    No numpy: /usr/bin/python3 ships without it.
    """
    n = len(samples)
    if n == 0:
        return 0.0
    k = round(n * freq / rate)
    w = 2 * math.pi * k / n
    coeff = 2 * math.cos(w)
    s_prev = s_prev2 = 0.0
    total = 0.0
    for x in samples:
        v = x / 32768.0
        total += v * v
        s = v + coeff * s_prev - s_prev2
        s_prev2, s_prev = s_prev, s
    power = s_prev2 * s_prev2 + s_prev * s_prev - coeff * s_prev * s_prev2
    if total <= 0:
        return 0.0
    # Parseval: sum|x[n]|^2 = (1/N) sum|X_k|^2. A real signal splits the energy
    # for one frequency across bins k and N-k, so the energy at `freq` is
    # 2|X_k|^2/N. Dividing by the total gives a fraction that is 1.0 for a pure
    # sine and near 0 for broadband noise.
    return min(1.0, 2.0 * power / (n * total))


def analyze(path):
    with wave.open(path) as w:
        frames, rate, channels = w.getnframes(), w.getframerate(), w.getnchannels()
        if frames == 0:
            return (
                f"{os.path.basename(path):12} EMPTY — 0 frames captured.\n"
                f"{'':12} No audio callbacks arrived at all. Either the device was "
                f"idle (nothing was playing) or the wrong device was tapped."
            )
        raw = w.readframes(frames)

    samples = struct.unpack("<%dh" % (len(raw) // 2), raw)
    peak = max(abs(s) for s in samples)
    rms = math.sqrt(sum(s * s for s in samples) / len(samples))
    dbfs = 20 * math.log10(rms / 32768) if rms > 0 else float("-inf")

    line = (
        f"{os.path.basename(path):12} {frames / rate:6.2f}s  {rate}Hz {channels}ch  "
        f"peak={peak:6d}  rms={dbfs:7.1f}dBFS  -> "
    )

    if peak == 0:
        return (
            line + "DIGITAL SILENCE\n"
            f"{'':12} Frames arrived but every sample is exactly zero. For "
            f"system.wav this is the macOS TCC denial signature — the tap runs "
            f"and is fed silence rather than failing. See docs/AUDIO_CAPTURE.md."
        )
    if dbfs < -60:
        return line + "near-silent (check input routing / gain)"
    if dbfs < -45:
        return line + "very quiet — usable but consider normalizing"
    return line + "audio present"


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 1
    target = sys.argv[1]
    wavs = sorted(f for f in os.listdir(target) if f.endswith(".wav"))
    if not wavs:
        print(f"no .wav files in {target}")
        return 1
    for name in wavs:
        print(analyze(os.path.join(target, name)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
