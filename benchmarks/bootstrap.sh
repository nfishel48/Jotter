#!/usr/bin/env bash
# Create the benchmark harness's virtualenv, and fetch the pieces that make its
# numbers comparable to published ones.
#
# Separate from `scripts/`, which is deliberately stdlib-only — see the note in
# scripts/analyze_wav.py. Nothing here ever enters the shipped binary.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
venv="$here/.venv"

python3 -m venv "$venv"
"$venv/bin/pip" install --quiet --upgrade pip
"$venv/bin/pip" install --quiet -r "$here/requirements.txt"

echo "venv ready: $venv"

# The spelling map Whisper's normaliser needs. Without it the harness falls back
# to basic normalisation and refuses to claim comparability, so this is the
# difference between a number you can publish and one you cannot.
if ! "$venv/bin/python" -m jbench fetch --normalizer; then
  echo
  echo "Could not fetch the normaliser spelling map (no network?)."
  echo "The harness still runs; its results will be marked not comparable."
fi

cat <<'EOF'

Next:
  cargo build --release -p jotter-cli --features bench  # from the repository root
  benchmarks/bench list                                 # what can be measured
  benchmarks/bench smoke                                # does the harness work
EOF
