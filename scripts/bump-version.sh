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

# Bump the workspace version and every workspace.dependencies path version
# (crates/*/Cargo.toml pull their version from workspace.package, so they
# don't need editing individually).
sed -i.bak -E "0,/^version = \".*\"/s//version = \"${VERSION}\"/" Cargo.toml
sed -i.bak -E "s/(keke-[a-z-]+ = \{ path = \"crates\/[a-z-]+\", version = \")[^\"]+(\" \})/\1${VERSION}\2/" Cargo.toml
rm -f Cargo.toml.bak

cargo update --workspace

git add Cargo.toml Cargo.lock
git commit -m "chore: bump version to ${VERSION}"
git tag "v${VERSION}"

echo "bumped to ${VERSION} and tagged v${VERSION}"
echo "push with: git push origin main v${VERSION}"
