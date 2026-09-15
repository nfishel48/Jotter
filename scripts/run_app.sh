#!/usr/bin/env bash
# Build and launch the Jotter tray app.
#
# macOS goes through build/Jotter.app: a bare cargo binary has no bundle
# identifier, so macOS cannot grant it system-audio access and silently records
# digital zeros (see docs/AUDIO_CAPTURE.md). Linux has no such gate and runs the
# binary directly.
#
# Usage:
#   scripts/run_app.sh          # build and launch
#   scripts/run_app.sh --stop   # quit a running instance
#   scripts/run_app.sh --logs   # show stdout/stderr of the running instance

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=platform.sh
source "$ROOT/scripts/platform.sh"

APP="$ROOT/build/Jotter.app"
OUT=/tmp/jotter.out
ERR=/tmp/jotter.err

cd "$ROOT"

# Keep in sync with recordings_root() in src/ui.rs.
RECORDINGS="${XDG_DOCUMENTS_DIR:-$HOME/Documents}/Jotter"

case "${1:-}" in
  --stop)
    killall jotter 2>/dev/null && echo "stopped" || echo "not running"
    exit 0
    ;;
  --logs)
    echo "--- stdout ---"; cat "$OUT" 2>/dev/null
    echo "--- stderr ---"; cat "$ERR" 2>/dev/null
    exit 0
    ;;
esac

jotter_check_linux_audio

# A second instance would fight the first for the tray icon and the audio
# devices, so replace rather than stack.
if pgrep -f "MacOS/jotter|target/debug/jotter" >/dev/null 2>&1; then
  echo "==> stopping running instance"
  killall jotter 2>/dev/null || true
  sleep 1
fi

if [[ "$JOTTER_OS" == "macos" ]]; then
  echo "==> bundling"
  ./scripts/bundle.sh >/dev/null
  echo "==> launching"
  rm -f "$OUT" "$ERR"
  open -a "$APP" --stdout "$OUT" --stderr "$ERR"
else
  echo "==> building"
  cargo build --quiet --bin jotter
  echo "==> launching"
  rm -f "$OUT" "$ERR"
  ./target/debug/jotter >"$OUT" 2>"$ERR" &
  disown 2>/dev/null || true
fi

sleep 3

if pgrep -f "MacOS/jotter|target/debug/jotter" >/dev/null 2>&1; then
  echo "running — look for the Jotter icon in the tray / menu bar"
else
  echo "FAILED to start:" >&2
  cat "$ERR" >&2
  exit 1
fi

cat <<EOF

  Tray menu:  Start Recording / Settings / Quit
  Recordings: $RECORDINGS

After recording, check the tracks with:

  scripts/analyze_wav.py "\$(ls -dt "$RECORDINGS"/* | head -1)"

  logs:  scripts/run_app.sh --logs
  stop:  scripts/run_app.sh --stop
EOF
