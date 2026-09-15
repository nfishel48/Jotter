#!/usr/bin/env bash
# Bump the version in Cargo.toml (and keep Cargo.lock in step).
#
# CI runs `patch` automatically on every push to main. Run `minor` or `major`
# by hand when you want one, commit it, and the next automatic patch bump
# continues from there.
#
# Usage:
#   scripts/bump_version.sh patch    # 0.1.0 -> 0.1.1
#   scripts/bump_version.sh minor    # 0.1.3 -> 0.2.0
#   scripts/bump_version.sh major    # 0.2.7 -> 1.0.0
#   scripts/bump_version.sh --current
#
# Prints the resulting version to stdout; everything else goes to stderr so the
# output can be captured directly in a workflow.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST="$ROOT/Cargo.toml"
LOCKFILE="$ROOT/Cargo.lock"

# Only the [package] version, which is the first `version = ` in the file.
# Matching loosely would rewrite dependency versions instead.
current() {
  awk '/^\[package\]/{p=1; next} /^\[/{p=0} p && /^version *= *"/{
    match($0, /"[^"]+"/); print substr($0, RSTART+1, RLENGTH-2); exit
  }' "$MANIFEST"
}

CUR="$(current)"
[[ -n "$CUR" ]] || { echo "could not read version from $MANIFEST" >&2; exit 1; }

if [[ "${1:-}" == "--current" ]]; then
  echo "$CUR"
  exit 0
fi

KIND="${1:-patch}"
IFS=. read -r MAJOR MINOR PATCH <<<"$CUR"

if ! [[ "$MAJOR$MINOR$PATCH" =~ ^[0-9]+$ ]]; then
  echo "version '$CUR' is not a plain MAJOR.MINOR.PATCH — refusing to guess" >&2
  exit 1
fi

case "$KIND" in
  patch) PATCH=$((PATCH + 1)) ;;
  minor) MINOR=$((MINOR + 1)); PATCH=0 ;;
  major) MAJOR=$((MAJOR + 1)); MINOR=0; PATCH=0 ;;
  *) echo "usage: $0 [patch|minor|major|--current]" >&2; exit 1 ;;
esac

NEW="$MAJOR.$MINOR.$PATCH"
echo "==> $CUR -> $NEW ($KIND)" >&2

# Rewrite only the [package] version. Done with awk rather than sed so the
# section boundary is respected and a dependency pinned to the same version
# string cannot be caught by accident.
tmp="$(mktemp)"
awk -v new="$NEW" '
  /^\[package\]/ { p = 1 }
  p && /^version *= *"/ && !done { sub(/"[^"]+"/, "\"" new "\""); done = 1 }
  /^\[/ && !/^\[package\]/ { p = 0 }
  { print }
' "$MANIFEST" > "$tmp"
mv "$tmp" "$MANIFEST"

# Keep Cargo.lock consistent, or `cargo build --locked` fails in CI.
#
# Note this must resolve dependencies to rewrite the lock: `--no-deps` skips
# resolution and silently leaves the old version behind. It still won't bump
# dependency versions, unlike `generate-lockfile`, which would.
if [[ -f "$LOCKFILE" ]]; then
  (cd "$ROOT" && cargo metadata --format-version 1 >/dev/null)
fi

VERIFIED="$(current)"
[[ "$VERIFIED" == "$NEW" ]] || { echo "bump failed: manifest reads $VERIFIED" >&2; exit 1; }

# Verify rather than assume — a stale lock breaks --locked builds, and the
# failure would surface far from here.
if [[ -f "$LOCKFILE" ]]; then
  LOCKED="$(awk '/^name = "jotter"$/{getline; match($0, /"[^"]+"/);
                 print substr($0, RSTART+1, RLENGTH-2); exit}' "$LOCKFILE")"
  [[ "$LOCKED" == "$NEW" ]] || {
    echo "Cargo.lock still reads '$LOCKED' after bumping to $NEW" >&2
    exit 1
  }
fi

echo "$NEW"
