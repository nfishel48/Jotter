#!/usr/bin/env bash
# Grade echo cancellation, either on a recording you already have or on a fresh
# acoustic loop.
#
# Usage:
#   scripts/check_aec.sh <dir>     # process <dir> and grade the result
#   scripts/check_aec.sh --live    # record through the speakers, then grade it
#
# Offline mode is the gate. It touches no audio devices, needs no permissions
# and no macOS bundle, and is deterministic — so it is what you run while
# changing the canceller, and what to point at a recording that came out badly.
#
# Live mode differs from check_audio.sh in two ways that matter, and both are
# the point rather than an oversight:
#
#   1. Output is AUDIBLE. check_audio.sh mutes it deliberately, to remove the
#      speaker-to-microphone path so its swapped-track check means something.
#      That path is the entire subject here, so that script is structurally
#      incapable of testing this. Use a room you can make noise in.
#
#   2. It does NOT use the 440 Hz tone. A pure tone excites one frequency, so
#      the echo path is unidentifiable everywhere else and a tone-derived
#      cancellation figure is meaningless — it would look spectacular and tell
#      you nothing. The reference signal here is speech-shaped noise with a
#      silent gap, so the recording has the echo-only and near-only stretches
#      the measurement needs. The existing tone infrastructure actively invites
#      this mistake, which is why this says so at length.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=platform.sh
source "$ROOT/scripts/platform.sh"

PY="$JOTTER_PY"
LIVE=0
DIR=""
for arg in "$@"; do
  case "$arg" in
    --live) LIVE=1 ;;
    -*) echo "unknown option: $arg" >&2; exit 1 ;;
    *) DIR="$arg" ;;
  esac
done

if [[ $LIVE -eq 0 && -z "$DIR" ]]; then
  sed -n '2,28p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit 1
fi

cd "$ROOT"

if [[ $LIVE -eq 1 ]]; then
  DURATION="${DURATION:-25}"
  DIR="$ROOT/recordings/aec-$(date +%s)"
  NOISE="$ROOT/target/aec-reference.wav"

  jotter_check_linux_audio

  echo "==> generating the reference signal"
  mkdir -p "$(dirname "$NOISE")"
  "$PY" - "$NOISE" <<'PYEOF'
import math, random, struct, sys, wave

# Speech-shaped noise: white noise through a one-pole lowpass, which puts most
# of the energy where speech lives and where a laptop speaker can actually
# reproduce it. A silent gap in the middle gives the measurement a near-only
# stretch to compare against, which is the half that catches a canceller
# eating the user's voice.
SR, AMP = 48000, 0.35
LEAD, GAP, TAIL = 10.0, 5.0, 10.0
random.seed(7)

frames = bytearray()
state = 0.0
for section, seconds in (("noise", LEAD), ("gap", GAP), ("noise", TAIL)):
    for _ in range(int(SR * seconds)):
        if section == "gap":
            frames += struct.pack("<hh", 0, 0)
            continue
        state = 0.92 * state + 0.08 * random.uniform(-1.0, 1.0)
        v = int(max(-1.0, min(1.0, state * 6.0)) * AMP * 32767)
        frames += struct.pack("<hh", v, v)

w = wave.open(sys.argv[1], "w")
w.setnchannels(2); w.setsampwidth(2); w.setframerate(SR)
w.writeframes(bytes(frames))
w.close()
PYEOF

  echo "==> building"
  cargo build --release --locked --bin jotter

  cleanup() { jotter_stop_tone; jotter_restore_output; }
  trap cleanup EXIT

  cat <<'EOS'

  This test needs the speakers AUDIBLE — the speaker-to-microphone path is
  exactly what is being measured, so headphones would make it meaningless.

  There is a 5-second silent gap in the middle of the reference signal.
  SPEAK during that gap, and stay quiet otherwise. That gives the measurement
  a stretch of your voice alone to check against.

EOS
  read -r -p "  Press Enter when ready... " _

  echo "==> playing the reference signal"
  if ! jotter_play_tone "$NOISE"; then
    echo "ERROR: no player found." >&2
    [[ "$JOTTER_OS" == "linux" ]] && echo "       install pipewire-utils (pw-play) or pulseaudio-utils (paplay)" >&2
    exit 1
  fi
  sleep 1

  echo "==> recording ${DURATION}s"
  ./target/release/jotter record --duration "$DURATION" --out "$DIR"

  cleanup
  trap - EXIT
else
  [[ -d "$DIR" ]] || { echo "no such directory: $DIR" >&2; exit 1; }
  echo "==> building"
  cargo build --release --locked --bin jotter
fi

echo
echo "==> levels"
"$PY" "$ROOT/scripts/analyze_wav.py" "$DIR"

echo
echo "==> cancelling"
./target/release/jotter process --force "$DIR"

echo
echo "==> grading"
# The exit status is the gate's, so this script can be used in a pipeline.
"$PY" "$ROOT/scripts/check_aec.py" "$DIR"
