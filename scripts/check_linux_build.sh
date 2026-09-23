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
#   scripts/check_linux_build.sh ci       # what ci.yml runs on ubuntu-latest
#   scripts/check_linux_build.sh package  # what release.yml's build-linux does:
#                                         # Debian 12 release build -> dist/

set -euo pipefail

# `ci` mirrors exactly what .github/workflows/ci.yml runs on ubuntu-latest, so
# the Linux half of the matrix can be validated before pushing.
CMD="${1:-check}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if ! docker info >/dev/null 2>&1; then
  echo "Docker is not running. Start Docker Desktop and retry." >&2
  exit 1
fi

# The release build, reproduced: Debian 12 rather than trixie (glibc baseline;
# see linux_release_deps.sh), and packaged into dist/. The target directory and
# cargo registry live in named volumes so a second run does not start cold.
# The packages come out at the host's architecture (arm64 on Apple Silicon),
# which exercises the same packaging as the x86_64 release.
if [ "$CMD" = "package" ]; then
  echo "==> release build and packages for Linux (Debian 12 baseline)"
  docker run --rm -t \
    -v "$ROOT":/src \
    -v jotter-bookworm-target:/target \
    -v jotter-bookworm-cargo:/usr/local/cargo/registry \
    -w /src \
    -e CARGO_TARGET_DIR=/target \
    -e VERSION \
    rust:1-bookworm \
    bash -c '
      set -e
      scripts/linux_release_deps.sh
      rustc --version
      cargo build --release --locked -p jotter-cli --bin jotter
      scripts/package_linux.sh
    '
  exit 0
fi

# The container runs at the host's architecture — aarch64 on Apple Silicon —
# which is fine for a compile check: nothing here is arch-specific.
echo "==> cargo $CMD for Linux (pipewire feature enabled)"

# Deps for: the pipewire host, the alsa fallback, `aec`'s WebRTC build (meson,
# ninja, clang), aws-lc-sys under posthog-rs (cmake) and sherpa-onnx-sys's
# build script (liblzma, via zip -> xz2 -> lzma-sys). The rust image already
# carries several of these; they are listed anyway so this stays the same list
# CI installs on a bare runner. Nothing graphical: Jotter has no window, so no
# toolkit or display-server headers.
#
# trixie rather than bookworm for meson: webrtc-audio-processing-sys runs
# `meson setup --reconfigure` on a fresh build directory, which bookworm's
# meson 1.0 rejects as "not a valid build tree" and newer releases accept.
# CI's ubuntu-latest ships a new enough one.
docker run --rm -t \
  -v "$ROOT":/src \
  -w /src \
  -e CARGO_TARGET_DIR=/tmp/target \
  rust:1-trixie \
  bash -c '
    set -e
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
      pkg-config clang libclang-dev \
      cmake meson ninja-build \
      libpipewire-0.3-dev libspa-0.2-dev \
      libasound2-dev liblzma-dev \
      >/dev/null 2>&1
    echo "--- toolchain ---"
    rustc --version
    if [ "'"$CMD"'" = "ci" ]; then
      export RUSTFLAGS="-D warnings"
      rustup component add rustfmt clippy >/dev/null 2>&1
      echo "--- fmt ---";    cargo fmt --all --check
      echo "--- clippy ---"; cargo clippy --workspace --all-targets --locked --color always 2>&1 | tail -20
      echo "--- test ---";   cargo test --workspace --all-targets --locked --color always 2>&1 | grep -E "test result|^test |error" | tail -20
      echo "--- build ---";  cargo build --workspace --locked --color always 2>&1 | tail -5
      echo "--- features ---"
      cargo check --locked -p jotter --no-default-features --color always 2>&1 | tail -5
      cargo check --locked -p jotter-cli --no-default-features --color always 2>&1 | tail -5
    else
      echo "--- cargo '"$CMD"' --workspace --all-targets ---"
      cargo '"$CMD"' --workspace --all-targets --color always 2>&1 | tail -40
    fi
  '
