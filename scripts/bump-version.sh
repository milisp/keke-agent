#!/usr/bin/env bash
# Bump the workspace version, refresh Cargo.lock, and tag the release.
#
# Usage:
#   scripts/bump-version.sh 0.1.3

set -euo pipefail

VERSION="${1:?usage: scripts/bump-version.sh <version>}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
  echo "error: version must be X.Y.Z, got: $VERSION" >&2
  exit 1
}

git diff --quiet --exit-code || {
  echo "error: working tree is dirty, commit or stash first" >&2
  exit 1
}

# Bump the workspace version (crates/*/Cargo.toml pull their version from
# workspace.package, so they don't need editing individually).
# Uses awk instead of `sed -E "0,/re/s//../"` because that range form is a
# GNU extension: on macOS's BSD sed it's silently accepted but never matches,
# so the version (and everything downstream) never actually changes.
awk -v ver="$VERSION" '
  !done && /^version = "/ { sub(/^version = ".*"/, "version = \"" ver "\""); done = 1 }
  { print }
' Cargo.toml > Cargo.toml.tmp
mv Cargo.toml.tmp Cargo.toml

cargo update --workspace

git add Cargo.toml Cargo.lock
git commit -m "chore: bump version to ${VERSION}"
git tag "v${VERSION}"

echo "bumped to ${VERSION} and tagged v${VERSION}"
echo "push with: git push origin main v${VERSION}"
