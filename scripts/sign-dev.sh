#!/usr/bin/env bash
# Ad-hoc code-sign the local debug/release binary with a fixed identifier.
#
# macOS Keychain ties a keyring entry's ACL to the requesting binary's code
# signature. `cargo build` produces a fresh ad-hoc signature on every build,
# so without this, macOS treats each rebuild as a new app and re-prompts for
# Keychain access every run. Signing with a stable --identifier fixes the
# identity across rebuilds so "Always Allow" only needs to happen once.
#
# This is local-only ad-hoc signing (`-s -`), not the Developer ID signing
# the release workflow does for distributed builds.
set -euo pipefail

profile="${1:-debug}"
bin="target/${profile}/keke"

if [[ ! -f "$bin" ]]; then
    echo "error: $bin not found; run cargo build (or --release) first" >&2
    exit 1
fi

codesign -f -s - --identifier com.keke.cli "$bin"
echo "signed $bin with identifier com.keke.cli"
