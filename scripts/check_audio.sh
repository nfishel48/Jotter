#!/usr/bin/env bash
# End-to-end capture check: play a known tone, record, and report what landed.
#
# macOS runs through build/Jotter.app by default, because a bare cargo binary
# has no bundle identifier and macOS silently feeds its tap digital zeros.
# Linux has no such gate and runs the binary directly.
#
# Usage:
#   scripts/check_audio.sh                 # both tracks
#   scripts/check_audio.sh system          # system audio only
#   scripts/check_audio.sh mic             # microphone only
#   scripts/check_audio.sh both --audible  # do not mute (macOS)
#   scripts/check_audio.sh both --direct   # bypass the bundle (macOS: expected to fail)
#
# On macOS the tone is inaudible by default: CoreAudio taps capture before
# device volume, so output is muted for the duration and the capture still
# lands at full amplitude. Muting also removes the speaker-to-mic acoustic path,
# which is what makes the swapped-track check meaningful. On Linux no
# equivalent guarantee is assumed, so the tone is audible — use headphones.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=platform.sh
source "$ROOT/scripts/platform.sh"

ONLY="both"
AUDIBLE=0
DIRECT=0
for arg in "$@"; do
  case "$arg" in
    mic|system|both) ONLY="$arg" ;;
    --audible) AUDIBLE=1 ;;
    --direct)  DIRECT=1 ;;
    *) echo "unknown argument: $arg" >&2; exit 1 ;;
  esac
done

# Linux has no bundle, so there is nothing to bypass.
[[ "$JOTTER_OS" != "macos" ]] && DIRECT=1

DURATION="${DURATION:-8}"
PY="$JOTTER_PY"
APP="$ROOT/build/Jotter.app"
TONE="$ROOT/target/tone.wav"
OUT="$ROOT/recordings/check-$(date +%s)"

cd "$ROOT"

cleanup() { jotter_stop_tone; jotter_restore_output; }
trap cleanup EXIT

jotter_check_linux_audio

if [[ $DIRECT -eq 1 ]]; then
  echo "==> building"
  cargo build --quiet -p jotter-cli --bin jotter
  RUN=(./target/debug/jotter)
else
  echo "==> building bundle"
  ./scripts/bundle.sh >/dev/null
  # -n: a new instance every time. Without it, LaunchServices hands the
  # request to any Jotter.app already running — a recording in progress —
  # and our --args are silently dropped.
  # -W: wait for it to exit, so its output is complete when we read it.
  RUN=(open -n -W -a "$APP" --stdout /tmp/jotter.out --stderr /tmp/jotter.err --args)
fi

echo "==> devices"
if [[ $DIRECT -eq 1 ]]; then
  "${RUN[@]}" devices
else
  rm -f /tmp/jotter.out; "${RUN[@]}" devices; cat /tmp/jotter.out
fi
echo

if [[ ! -f "$TONE" ]]; then
  echo "==> generating test tone"
  mkdir -p "$(dirname "$TONE")"
  "$PY" - "$TONE" <<'PYEOF'
import math, struct, sys, wave
sr, dur, amp, freq = 48000, 60, 0.25, 440
w = wave.open(sys.argv[1], "w")
w.setnchannels(2); w.setsampwidth(2); w.setframerate(sr)
w.writeframes(b"".join(
    struct.pack("<hh", v, v)
    for v in (int(amp * 32767 * math.sin(2 * math.pi * freq * t / sr))
              for t in range(sr * dur))))
w.close()
PYEOF
fi

MUTED=0
if [[ "$ONLY" != "mic" ]]; then
  if [[ $AUDIBLE -eq 0 ]] && jotter_mute_output; then
    MUTED=1
    echo "==> output muted (tap is pre-volume; will restore to $JOTTER_ORIG_VOL)"
  elif [[ "$JOTTER_OS" == "linux" ]]; then
    echo "==> tone will be AUDIBLE (muting not assumed safe for sink monitors)"
  fi

  echo "==> playing tone"
  if ! jotter_play_tone "$TONE"; then
    echo "ERROR: no tone player found." >&2
    [[ "$JOTTER_OS" == "linux" ]] && echo "       install pipewire-utils (pw-play) or pulseaudio-utils (paplay)" >&2
    exit 1
  fi
  sleep 1
fi

[[ "$ONLY" == "mic" ]] && echo "==> SPEAK NOW so the mic track has signal"

echo "==> recording ${DURATION}s (--only $ONLY)"
if [[ $DIRECT -eq 1 ]]; then
  "${RUN[@]}" record --only "$ONLY" --duration "$DURATION" --out "$OUT"
else
  rm -f /tmp/jotter.out /tmp/jotter.err
  "${RUN[@]}" record --only "$ONLY" --duration "$DURATION" --out "$OUT"
  cat /tmp/jotter.out
  [[ -s /tmp/jotter.err ]] && { echo "--- stderr ---"; cat /tmp/jotter.err; }
fi

cleanup
trap - EXIT

echo
echo "==> analysis"
"$PY" "$ROOT/scripts/analyze_wav.py" "$OUT"

# With output muted there is no acoustic path from speakers to microphone, so a
# pure 440Hz tone in mic.wav means the streams are crossed — the failure mode of
# handing cpal a duplex device for loopback. Audible playback makes the test
# meaningless (speaker bleed puts the tone in both), so it is skipped then.
if [[ "$ONLY" == "both" && $MUTED -eq 1 && -f "$OUT/mic.wav" ]]; then
  echo
  echo "==> swapped-track check"
  "$PY" - "$OUT" "$ROOT" <<'PYEOF'
import os, sys, wave, struct
sys.path.insert(0, os.path.join(sys.argv[2], "scripts"))
from analyze_wav import tone_purity

def purity(p):
    with wave.open(p) as w:
        n, sr = w.getnframes(), w.getframerate()
        if n == 0:
            return None
        raw = w.readframes(n)
    return tone_purity(struct.unpack("<%dh" % (len(raw) // 2), raw), sr)

d = sys.argv[1]
mp, sp = purity(os.path.join(d, "mic.wav")), purity(os.path.join(d, "system.wav"))
fmt = lambda v: "n/a" if v is None else f"{v:.3f}"
print(f"  system.wav 440Hz purity: {fmt(sp)}")
print(f"  mic.wav    440Hz purity: {fmt(mp)}")
if sp is None or sp < 0.3:
    print("  FAIL: system.wav is not carrying the tone.")
elif mp is not None and mp > 0.3:
    print("  FAIL: mic.wav carries the pure tone with speakers muted —")
    print("        the streams are crossed (duplex-device loopback bug).")
else:
    print("  PASS: tone is isolated to system.wav; tracks are not crossed.")
PYEOF
elif [[ "$ONLY" == "both" && "$JOTTER_OS" == "linux" ]]; then
  echo
  echo "note: swapped-track check skipped — it needs muted output, and speaker"
  echo "      bleed would put the tone in both tracks. Use headphones and"
  echo "      compare the 440Hz purity figures above by hand."
fi

echo
echo "recording dir: $OUT"
