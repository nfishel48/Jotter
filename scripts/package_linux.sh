#!/usr/bin/env bash
# Package an already-built Linux `jotter` as a tarball, a .deb and an .rpm.
#
# Usage (after `cargo build --release --locked -p jotter-cli --bin jotter`):
#   scripts/package_linux.sh            # version read from Cargo.toml
#   VERSION=0.1.18 scripts/package_linux.sh
#
# Writes to dist/:
#   jotter-VERSION-linux-ARCH.tar.gz   the bare binary, for the AUR and by hand
#   jotter_VERSION-1_DEBARCH.deb       apt  (metadata: crates/jotter-cli/Cargo.toml)
#   jotter-VERSION-1.RPMARCH.rpm       dnf  (same file)
#
# All three carry the same binary byte for byte. In particular the packages do
# not strip it: telemetry's crash reports are symbolicated from its symbols.
#
# Builds nothing itself, so the release packages exactly the binary CI built
# with `--locked` and its telemetry key. Respects CARGO_TARGET_DIR.

set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "package_linux.sh packages a Linux build; use scripts/check_linux_build.sh package from macOS" >&2
  exit 1
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="${VERSION:-$(scripts/bump_version.sh --current)}"
TARGET_DIR="${CARGO_TARGET_DIR:-target}"
BIN="$TARGET_DIR/release/jotter"
ARCH="$(uname -m)"
DIST="$ROOT/dist"

CARGO_DEB_VERSION="3.8.0"
CARGO_GENERATE_RPM_VERSION="0.21.0"

[[ -x "$BIN" ]] || { echo "no binary at $BIN — build it first" >&2; exit 1; }

# A binary that reports another version would be packaged under a lie.
REPORTED="$("$BIN" --version)"
[[ "$REPORTED" == *" $VERSION" ]] || {
  echo "$BIN reports '$REPORTED', expected version $VERSION" >&2
  exit 1
}

install_tool() {
  local crate="$1" version="$2"
  if ! cargo install --list | grep -q "^$crate v$version:"; then
    cargo install --locked --quiet "$crate@$version"
  fi
}
install_tool cargo-deb "$CARGO_DEB_VERSION"
install_tool cargo-generate-rpm "$CARGO_GENERATE_RPM_VERSION"

rm -rf "$DIST"
mkdir -p "$DIST"

echo "==> tarball"
STAGE="jotter-$VERSION-linux-$ARCH"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/$STAGE"
cp "$BIN" "$TMP/$STAGE/jotter"
cp docs/AUDIO_CAPTURE.md "$TMP/$STAGE/README.md"
cp LICENSE "$TMP/$STAGE/LICENSE"
tar -C "$TMP" -czf "$DIST/$STAGE.tar.gz" "$STAGE"

echo "==> deb"
cargo deb -p jotter-cli --no-build --no-strip --deb-version "$VERSION-1" --output "$DIST/"

echo "==> rpm"
cargo generate-rpm -p crates/jotter-cli --set-metadata "version = \"$VERSION\"" --output "$DIST/"

ls -l "$DIST"
