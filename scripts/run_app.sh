#!/usr/bin/env bash
# Build Jotter and run one `jotter` command the way it can actually capture.
#
# macOS goes through build/Jotter.app: a bare cargo binary has no bundle
# identifier, so macOS cannot grant it system-audio access and silently records
# digital zeros (see docs/AUDIO_CAPTURE.md). Linux has no such gate and runs the
# binary directly.
#
# Usage:
#   scripts/run_app.sh                       # record 10s into ~/Documents/Jotter
#   scripts/run_app.sh record --duration 60  # any jotter arguments
#   scripts/run_app.sh --logs                # output of the last bundled run
#
# A bundled run has no terminal on stdin, so `record` needs `--duration`:
# "press Enter to stop" would read end-of-file and stop at once.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=platform.sh
source "$ROOT/scripts/platform.sh"

APP="$ROOT/build/Jotter.app"
OUT=/tmp/jotter.out
ERR=/tmp/jotter.err

cd "$ROOT"

# Keep in sync with recordings_root() in crates/jotter/src/config.rs.
RECORDINGS="${XDG_DOCUMENTS_DIR:-$HOME/Documents}/Jotter"

if [[ "${1:-}" == "--logs" ]]; then
  echo "--- stdout ---"; cat "$OUT" 2>/dev/null
  echo "--- stderr ---"; cat "$ERR" 2>/dev/null
  exit 0
fi

ARGS=("$@")
[[ ${#ARGS[@]} -eq 0 ]] && ARGS=(record --duration 10)

jotter_check_linux_audio

if [[ "$JOTTER_OS" == "macos" ]]; then
  echo "==> bundling"
  ./scripts/bundle.sh >/dev/null
  echo "==> running: jotter ${ARGS[*]}"
  rm -f "$OUT" "$ERR"
  # -n so a Jotter.app that is already running (another recording) does not
  # swallow these arguments; -W so the output below is complete.
  open -n -W -a "$APP" --stdout "$OUT" --stderr "$ERR" --args "${ARGS[@]}"
  cat "$OUT"
  [[ -s "$ERR" ]] && { echo "--- stderr ---"; cat "$ERR"; }
else
  echo "==> building"
  cargo build --quiet -p jotter-cli --bin jotter
  echo "==> running: jotter ${ARGS[*]}"
  ./target/debug/jotter "${ARGS[@]}"
fi

cat <<EOF

Recordings: $RECORDINGS

Check the newest one's tracks with:

  scripts/analyze_wav.py "\$(ls -dt "$RECORDINGS"/* | head -1)"
EOF
