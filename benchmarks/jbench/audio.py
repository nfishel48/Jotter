"""Getting corpus audio into a file Jotter will read.

Deliberately does as little as possible. Two decisions worth stating:

**No resampling.** Corpora arrive at whatever rate they were recorded at, and
the obvious move is to resample everything to the 16 kHz the model wants. Doing
that here would mean the benchmark measures *scipy's* resampler, while real
recordings go through `sherpa_onnx::LinearResampler` inside the transcription
pass. Those are not the same filter and they do not produce the same audio. So
this only decodes the container, and Jotter resamples exactly as it would for a
meeting.

**Downmix to mono.** Not a shortcut: `jotter record` writes both tracks mono
(`TrackInfo::channels` is documented as always 1), so a stereo corpus file fed
in as-is would be audio of a kind the pipeline never sees.
"""

from __future__ import annotations

from pathlib import Path

import numpy as np
import soundfile as sf


def decode_to_wav(source: Path, destination: Path) -> tuple[int, float]:
    """Decode `source` to a mono 16-bit WAV at its native rate.

    Returns (sample_rate, duration_secs). Handles FLAC, WAV, OGG, MP3 and NIST
    SPHERE — whatever libsndfile was built with, which covers every corpus here.
    """
    data, rate = sf.read(str(source), dtype="float32", always_2d=True)
    mono = data.mean(axis=1)
    destination.parent.mkdir(parents=True, exist_ok=True)
    sf.write(str(destination), to_int16(mono), rate, subtype="PCM_16")
    return rate, len(mono) / rate if rate else 0.0


def to_int16(samples: np.ndarray) -> np.ndarray:
    """Float in [-1, 1] to int16, clipped.

    Clipping rather than rescaling: normalising per file would change the level
    relationship between the two tracks of a synthesised meeting, which is the
    one thing the echo canceller is most sensitive to.
    """
    return np.clip(samples * 32767.0, -32768, 32767).astype(np.int16)


def read_mono(path: Path) -> tuple[np.ndarray, int]:
    """A whole file as float32 mono, plus its rate."""
    data, rate = sf.read(str(path), dtype="float32", always_2d=True)
    return data.mean(axis=1), rate


def write_mono(path: Path, samples: np.ndarray, rate: int) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    sf.write(str(path), to_int16(samples), rate, subtype="PCM_16")


def duration_secs(path: Path) -> float:
    info = sf.info(str(path))
    return info.frames / info.samplerate if info.samplerate else 0.0
