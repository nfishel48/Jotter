#!/usr/bin/env bash
# Create the benchmark harness's virtualenv, and fetch the pieces that make its
# numbers comparable to published ones.
#
# Separate from `scripts/`, which is deliberately stdlib-only — see the note in
# scripts/analyze_wav.py. Nothing here ever enters the shipped binary.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
venv="$here/.venv"

# Pick an interpreter the pinned wheels actually exist for, rather than whatever
# `python3` happens to be. numpy 2.2.6 and scipy 1.15.3 ship no cp314 wheels, so
# on a 3.14 default pip falls back to building them from source and scipy dies
# looking for a Fortran compiler — leaving a venv containing only pip, and a
# confusing "no module named numpy" at the first `bench` run.
#
# Loosening the pins would fix it too, but requirements.txt is explicit that
# moving them is a decision to re-measure. Pin the interpreter instead.
python=""
for candidate in python3.13 python3.12 python3; do
  command -v "$candidate" >/dev/null 2>&1 || continue
  case "$("$candidate" -c 'import sys; print("%d.%d" % sys.version_info[:2])')" in
    3.12|3.13) python="$candidate"; break ;;
  esac
done

if [ -z "$python" ]; then
  echo "No suitable Python found. The pinned harness dependencies need 3.12 or 3.13" >&2
  echo "(3.14 has no numpy/scipy wheels at these versions and would build from source)." >&2
  echo "Install one, e.g.: brew install python@3.13" >&2
  exit 1
fi

echo "Using $python ($("$python" --version))"

# A venv built by a different interpreter can't be re-pointed in place; `venv`
# would leave the old version's symlinks behind. Rebuild when the version moved.
want="$("$python" -c 'import sys; print("%d.%d" % sys.version_info[:2])')"
if [ -f "$venv/pyvenv.cfg" ] && ! grep -q "^version = ${want}\." "$venv/pyvenv.cfg"; then
  echo "Existing venv was built by a different Python; recreating."
  rm -rf "$venv"
fi

"$python" -m venv "$venv"
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
