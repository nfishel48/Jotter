#!/usr/bin/env bash
# Install what the Linux release build needs, on a Debian 12 (bookworm) base.
#
# Run as root inside `rust:1-bookworm` — by release.yml's build-linux job and by
# `scripts/check_linux_build.sh package`, so the two cannot drift.
#
# Why bookworm and not the runner's Ubuntu: a binary needs at least the glibc it
# was linked against. Built on ubuntu-latest (24.04, glibc 2.39) it refuses to
# start on Debian 12 (2.36); built here it runs on Debian 12, Ubuntu 24.04 and
# anything newer. Debian 12 is the oldest release Jotter supports — it is the
# first Debian whose desktop runs PipeWire as the sound server, which system
# capture needs anyway.
#
# The one thing bookworm cannot supply is meson. Its 1.0 rejects the
# `meson setup --reconfigure` that webrtc-audio-processing-sys runs on a fresh
# build directory ("not a valid build tree"), so meson comes from PyPI, pinned
# to the version Debian 13 ships.

set -euo pipefail

MESON_VERSION="1.7.0"

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
# The -dev packages are the same list ci.yml installs; see the comments there.
# python3-venv is for meson.
apt-get install -y -qq --no-install-recommends \
  pkg-config clang libclang-dev cmake ninja-build \
  libpipewire-0.3-dev libspa-0.2-dev libasound2-dev liblzma-dev \
  python3-venv \
  >/dev/null

python3 -m venv /opt/meson
/opt/meson/bin/pip install --quiet "meson==$MESON_VERSION"
ln -sf /opt/meson/bin/meson /usr/local/bin/meson
meson --version
