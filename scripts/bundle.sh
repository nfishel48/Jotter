#!/usr/bin/env bash
# Assemble build/Jotter.app so macOS will actually grant it audio permissions.
#
# A bare `cargo build` binary is a linker-signed Mach-O with no bundle
# identifier. TCC cannot register such a process in the Privacy lists, so it
# never prompts and silently feeds the system-audio tap digital zeros. Wrapping
# the binary in a bundle with a stable CFBundleIdentifier and the usage-
# description keys is what makes the permission grantable at all.
#
# Usage:
#   scripts/bundle.sh
#   PROFILE=release scripts/bundle.sh
#
# The bundle's executable is the `jotter` command itself. It has no window;
# the bundle exists only to carry the identity and the usage strings TCC needs,
# and since the bundle id is fixed, one permission grant covers every
# subcommand run through it.

set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "bundle.sh is macOS-only — .app bundles exist to satisfy TCC." >&2
  echo "On Linux run the binary directly: target/debug/jotter" >&2
  exit 1
fi

EXEC="jotter"
PROFILE="${PROFILE:-debug}"
BUNDLE_ID="com.nfishel.jotter"
# Read from Cargo.toml rather than hardcoded, so released bundles report the
# version CI actually tagged — the release job applies the bump to its own
# checkout before packaging. An explicit VERSION wins, for packaging a binary
# built from some other checkout.
VERSION="${VERSION:-$("$(dirname "${BASH_SOURCE[0]}")/bump_version.sh" --current)}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Deliberately not under target/: `cargo clean` would wipe the bundle, and TCC
# keys partly on path — losing it means re-granting permissions.
APP="$ROOT/build/Jotter.app"

cd "$ROOT"

# SKIP_BUILD packages the binary already at target/$PROFILE/ — in CI, the one
# the release job built with `--locked` and its telemetry key — rather than
# rebuilding it here with neither.
if [[ "${SKIP_BUILD:-0}" == "1" ]]; then
  echo "==> using existing binary in target/$PROFILE"
else
  echo "==> building ($PROFILE)"
  if [[ "$PROFILE" == "release" ]]; then
    cargo build --release -p jotter-cli --bin "$EXEC"
  else
    cargo build -p jotter-cli --bin "$EXEC"
  fi
fi

echo "==> assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp "target/$PROFILE/$EXEC" "$APP/Contents/MacOS/$EXEC"
# Not decoration: with no Dock icon, this is the picture System Settings shows
# beside Jotter in the Privacy lists, which is where a user goes to grant it.
[[ -f assets/icon.png ]] && cp assets/icon.png "$APP/Contents/Resources/icon.png"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>$BUNDLE_ID</string>
    <key>CFBundleName</key>
    <string>Jotter</string>
    <key>CFBundleDisplayName</key>
    <string>Jotter</string>
    <key>CFBundleExecutable</key>
    <string>$EXEC</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
    <key>CFBundleIconFile</key>
    <string>icon.png</string>

    <!-- cpal's CoreAudio loopback uses process taps, added in macOS 14.4 and
         only reliable past 14.6. -->
    <key>LSMinimumSystemVersion</key>
    <string>14.6</string>

    <!-- No Dock icon: the process has no window, and a command recording in
         the background should not sit in the Dock and the Cmd-Tab switcher for
         the length of a meeting. LSUIElement hides it from both and changes
         nothing else; LSBackgroundOnly would go further than that needs. -->
    <key>LSUIElement</key>
    <true/>

    <!-- The strings the permission dialogs show. Without the matching key for
         a service, macOS kills the process instead of prompting. -->
    <key>NSMicrophoneUsageDescription</key>
    <string>Jotter records your microphone so it can transcribe your side of a meeting.</string>
    <key>NSAudioCaptureUsageDescription</key>
    <string>Jotter records audio from other meeting participants so it can transcribe what they say.</string>
    <key>NSScreenCaptureUsageDescription</key>
    <string>Jotter captures system audio output to record other meeting participants. No video is recorded.</string>
    <!-- Recordings are written to ~/Documents/Jotter so they are easy to find.
         Documents is TCC-gated, so this string appears on the first save. -->
    <key>NSDocumentsFolderUsageDescription</key>
    <string>Jotter saves your meeting recordings to a Jotter folder in Documents.</string>
</dict>
</plist>
PLIST

# Ad-hoc, because `security find-identity` reports no signing identities on
# this machine. A stable --identifier keeps the bundle id constant, but ad-hoc
# signatures are keyed by cdhash, so a rebuild can still invalidate an existing
# TCC grant and re-prompt. A self-signed certificate would fix that properly.
echo "==> signing (ad-hoc)"
codesign --force --deep --sign - --identifier "$BUNDLE_ID" "$APP"
codesign -dv --verbose=2 "$APP" 2>&1 | grep -E "Identifier|Signature" || true

# Register with LaunchServices so `open -b` / `open -a` resolve it.
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
  -f "$APP" 2>/dev/null || true

echo
echo "built $APP"
echo
echo "Run it through LaunchServices so TCC attributes the request to the bundle"
echo "rather than to your terminal:"
echo
echo "  open -a \"$APP\" --stdout /tmp/jotter.out --stderr /tmp/jotter.err --args record --duration 10"
echo
echo "or use: scripts/check_audio.sh --bundled"
