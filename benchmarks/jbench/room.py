"""Synthetic room acoustics, for putting speaker bleed into a microphone.

Both the meeting benchmark and the echo-cancellation sweep need the same thing:
audio that came out of a speaker and back into a microphone, having been through
a room on the way. Real recordings of that are what the AEC Challenge sets
provide and they are gated; these are generated instead, which buys
reproducibility — the same seed gives the same room on every machine, so two
people's numbers are about the canceller rather than about their furniture.

The model is the standard cheap one: a direct path, then exponentially decaying
Gaussian noise. It is *not* a measured impulse response and does not claim to
be — there are no real early reflections in it, so absolute ERLE figures here
will be kinder than a real room. What it is good for is comparison: the same
room applied to every condition, so a change in the canceller shows up as a
change in the number.

Pass `--rir <file>` anywhere this is used to substitute a measured response
(openslr.org/28 is the usual source) when an absolute figure is wanted.
"""

from __future__ import annotations

import numpy as np
from scipy.signal import fftconvolve

#: Loudest reflection, relative to the direct path. -6 dB: a room that is
#: audibly reverberant without the direct sound getting lost in it, which is
#: what a laptop a metre from its own speakers sounds like.
REVERB_GAIN = 0.5


def impulse_response(
    rate: int,
    rt60: float = 0.3,
    delay_ms: float = 8.0,
    seed: int = 0,
) -> np.ndarray:
    """A room impulse response: direct path at `delay_ms`, decaying to `rt60`.

    `rt60` is the time for the tail to fall 60 dB, which is what a room's
    reverberation is normally quoted as — 0.3 s is a small meeting room.

    The delay matters as much as the decay: it is the reason the echo canceller
    has a delay estimator at all, and a synthesis that put the echo at lag zero
    would quietly skip the hardest part of the problem.
    """
    rng = np.random.default_rng(seed)
    length = max(int(rate * rt60 * 1.5), 1)
    delay = int(rate * delay_ms / 1000.0)

    # -60 dB over rt60 seconds.
    decay = np.exp(-6.9078 * np.arange(length) / max(rate * rt60, 1.0))
    tail = rng.standard_normal(length) * decay

    # Normalise the tail before adding the direct path, so the direct path is
    # always the loudest thing in the response. Without this the tail's first
    # few samples — Gaussian, so occasionally above 1.0 while the decay is
    # still near 1 — can outrank it, and the delay estimator is then chasing a
    # reflection that the synthesis never meant to put there.
    peak = np.abs(tail).max()
    if peak:
        tail = tail * (REVERB_GAIN / peak)

    response = np.zeros(delay + length, dtype=np.float32)
    response[delay:] = tail
    response[delay] += 1.0  # the direct path

    peak = np.abs(response).max()
    return (response / peak).astype(np.float32) if peak else response


def apply(signal: np.ndarray, response: np.ndarray) -> np.ndarray:
    """Convolve, trimmed back to the input length.

    Trimming rather than keeping the tail: these signals are two tracks of one
    recording and they have to stay sample-aligned, and a track that grew by
    the length of the impulse response would not be.
    """
    return fftconvolve(signal, response)[: len(signal)].astype(np.float32)


def rms(signal: np.ndarray) -> float:
    return float(np.sqrt(np.mean(np.square(signal)))) if len(signal) else 0.0


def scale_to_ratio(signal: np.ndarray, reference: np.ndarray, ratio_db: float) -> np.ndarray:
    """Rescale `signal` to sit `ratio_db` below `reference` in RMS.

    This is the knob the AEC sweep turns: echo return loss is exactly how much
    quieter the speaker bleed is than the voice it is mixed with, and it is the
    single number that decides whether cancellation is easy or hopeless.
    """
    signal_rms, reference_rms = rms(signal), rms(reference)
    if signal_rms == 0 or reference_rms == 0:
        return signal
    target = reference_rms * (10.0 ** (-ratio_db / 20.0))
    return (signal * (target / signal_rms)).astype(np.float32)


def load_response(path, rate: int) -> np.ndarray:
    """A measured impulse response from disk, for absolute figures."""
    from .audio import read_mono

    response, response_rate = read_mono(path)
    if response_rate != rate:
        raise ValueError(
            f"the impulse response is {response_rate} Hz but the audio is {rate} Hz — "
            "resampling it here would change the acoustics being measured"
        )
    peak = np.abs(response).max()
    return (response / peak).astype(np.float32) if peak else response
