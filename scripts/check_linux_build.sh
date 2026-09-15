#!/usr/bin/env bash
# Type-check the Linux build (including the PipeWire host) in a container.
#
# The Linux target cannot be checked from macOS with plain cargo: the pipewire
# and alsa -sys crates need Linux system headers that no amount of
# cross-compilation flags will conjure. A container is the cheapest way to get
# a real answer instead of a guess.
#
# This verifies that it COMPILES. It says nothing about whether monitor capture
# actually works at runtime, which needs a real PipeWire session.
#
# Usage:
#   scripts/check_linux_build.sh          # cargo check
#   scripts/check_linux_build.sh build    # full cargo build (slower)

set -euo pipefail

# `ci` mirrors exactly what .github/workflows/ci.yml runs on ubuntu-latest, so
# the Linux half of the matrix can be validated before pushing.
CMD="${1:-check}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if ! docker info >/dev/null 2>&1; then
  echo "Docker is not running. Start Docker Desktop and retry." >&2
  exit 1
fi

echo "==> cargo $CMD for x86_64-unknown-linux-gnu (pipewire feature enabled)"

# Deps for: pipewire host, alsa fallback, eframe/wgpu (x11+wayland+xkb), and
# tray-icon (gtk3 + libappindicator).
docker run --rm -t \
  -v "$ROOT":/src \
  -w /src \
  -e CARGO_TARGET_DIR=/tmp/target \
  rust:1-bookworm \
  bash -c '
    set -e
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
      pkg-config clang libclang-dev \
      libpipewire-0.3-dev libspa-0.2-dev \
      libasound2-dev \
      libgtk-3-dev libayatana-appindicator3-dev libxdo-dev \
      libx11-dev libxcursor-dev libxrandr-dev libxi-dev \
      libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
      >/dev/null 2>&1
    echo "--- toolchain ---"
    rustc --version
    if [ "'"$CMD"'" = "ci" ]; then
      export RUSTFLAGS="-D warnings"
      rustup component add rustfmt clippy >/dev/null 2>&1
      echo "--- fmt ---";    cargo fmt --all --check
      echo "--- clippy ---"; cargo clippy --all-targets --locked --color always 2>&1 | tail -20
      echo "--- test ---";   cargo test --all-targets --locked --color always 2>&1 | grep -E "test result|^test |error" | tail -20
      echo "--- build ---";  cargo build --locked --color always 2>&1 | tail -5
    else
      echo "--- cargo '"$CMD"' --all-targets ---"
      cargo '"$CMD"' --all-targets --color always 2>&1 | tail -40
    fi
  '
