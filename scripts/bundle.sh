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
# There is one binary: `jotter` opens the tray app when run with no arguments
# and takes the CLI subcommands otherwise, so the same bundle — and so the same
# permission grant, since the bundle id is fixed — covers both.

set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "bundle.sh is macOS-only — .app bundles exist to satisfy TCC." >&2
  echo "On Linux run the binary directly: scripts/run_app.sh" >&2
  exit 1
fi

EXEC="jotter"
PROFILE="${PROFILE:-debug}"
BUNDLE_ID="com.nfishel.jotter"
# Read from Cargo.toml rather than hardcoded, so released bundles report the
# version CI actually tagged. CI overrides it: the packaging job assembles the
# .app from binaries built elsewhere and never applies the version bump to its
# own checkout, so its Cargo.toml still reads the previous version.
VERSION="${VERSION:-$("$(dirname "${BASH_SOURCE[0]}")/bump_version.sh" --current)}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Deliberately not under target/: `cargo clean` would wipe the bundle, and TCC
# keys partly on path — losing it means re-granting permissions.
APP="$ROOT/build/Jotter.app"

cd "$ROOT"

# SKIP_BUILD lets CI drop in a universal binary (lipo of arm64 + x86_64) at
# target/$PROFILE/ first — building here would overwrite it with a single-arch
# one.
if [[ "${SKIP_BUILD:-0}" == "1" ]]; then
  echo "==> using existing binary in target/$PROFILE"
else
  echo "==> building ($PROFILE)"
  if [[ "$PROFILE" == "release" ]]; then
    cargo build --release --bin "$EXEC"
  else
    cargo build --bin "$EXEC"
  fi
fi

echo "==> assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp "target/$PROFILE/$EXEC" "$APP/Contents/MacOS/$EXEC"
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

    <!-- Dock icon shown. Set to <true/> for a tray-only app with no Dock
         presence or menu bar of its own — but note that also removes Cmd-Q
         and Cmd-Tab, leaving the tray menu as the only way to quit. -->
    <key>LSUIElement</key>
    <false/>

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
echo "rather than to your terminal — with no --args it opens the tray app:"
echo
echo "  open -a \"$APP\" --stdout /tmp/jotter.out --stderr /tmp/jotter.err --args devices"
echo
echo "or use: scripts/check_audio.sh --bundled"
