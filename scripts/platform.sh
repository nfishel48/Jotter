#!/usr/bin/env bash
# Shared platform detection and audio helpers, sourced by the other scripts.
#
# macOS and Linux differ in three ways that matter to the debug tooling:
#   1. macOS needs the .app bundle for permissions; Linux runs the binary.
#   2. Tone playback is afplay vs pw-play/paplay/aplay.
#   3. Muting during a test is safe on macOS but not assumed on Linux.

# Deliberately no `set -e` here: this file is sourced, and killing the caller's
# shell on a failed probe would be surprising.

case "$(uname -s)" in
  Darwin) JOTTER_OS=macos ;;
  Linux)  JOTTER_OS=linux ;;
  *)      JOTTER_OS=unknown ;;
esac
export JOTTER_OS

# Python: on this machine `python3` is a homebrew alias to a missing binary, so
# macOS pins the system one. Linux just uses whatever is on PATH.
if [[ "$JOTTER_OS" == "macos" && -x /usr/bin/python3 ]]; then
  JOTTER_PY=/usr/bin/python3
else
  JOTTER_PY="$(command -v python3 || echo python3)"
fi
export JOTTER_PY

# Pick a tone player. PipeWire's pw-play first: if the PipeWire host is what
# cpal is capturing through, playing through the same daemon is the honest test.
jotter_find_player() {
  case "$JOTTER_OS" in
    macos) command -v afplay ;;
    linux) command -v pw-play || command -v paplay || command -v aplay ;;
    *) return 1 ;;
  esac
}

# Play a WAV in the background. Echoes nothing; caller kills via jotter_stop_tone.
jotter_play_tone() {
  local file="$1" player
  player="$(jotter_find_player)" || return 1
  "$player" "$file" >/dev/null 2>&1 &
  disown 2>/dev/null
  return 0
}

jotter_stop_tone() {
  case "$JOTTER_OS" in
    macos) killall afplay 2>/dev/null ;;
    linux) killall pw-play paplay aplay 2>/dev/null ;;
  esac
  return 0
}

# Muting during a capture test.
#
# Only done on macOS, where CoreAudio process taps are verified to capture
# *before* device volume — output at 0 still records at full amplitude. No
# equivalent guarantee is assumed for a PipeWire sink monitor, where muting
# could plausibly record silence and turn a passing test into a confusing
# failure. So on Linux the tone is audible; use headphones.
jotter_mute_output() {
  [[ "$JOTTER_OS" == "macos" ]] || return 1
  JOTTER_ORIG_VOL="$(osascript -e 'output volume of (get volume settings)' 2>/dev/null)" || return 1
  [[ "$JOTTER_ORIG_VOL" =~ ^[0-9]+$ ]] || return 1
  osascript -e 'set volume output volume 0' 2>/dev/null
  export JOTTER_ORIG_VOL
  return 0
}

jotter_restore_output() {
  [[ "$JOTTER_OS" == "macos" && -n "${JOTTER_ORIG_VOL:-}" ]] || return 0
  osascript -e "set volume output volume $JOTTER_ORIG_VOL" 2>/dev/null
  unset JOTTER_ORIG_VOL
  return 0
}

# Report anything obviously wrong with the Linux audio stack before a test
# spends eight seconds recording silence.
jotter_check_linux_audio() {
  [[ "$JOTTER_OS" == "linux" ]] || return 0
  if command -v pactl >/dev/null && pactl info 2>/dev/null | grep -qi "PipeWire"; then
    echo "==> PipeWire is running"
  elif systemctl --user is-active --quiet pipewire 2>/dev/null; then
    echo "==> PipeWire service is active"
  else
    echo "WARNING: PipeWire does not appear to be running." >&2
    echo "         cpal will fall back to ALSA, where sinks are not capturable" >&2
    echo "         and system-audio capture cannot work." >&2
    echo "         Check: systemctl --user status pipewire && wpctl status" >&2
  fi
  return 0
}
