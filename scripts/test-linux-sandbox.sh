#!/usr/bin/env bash
# Test kernel confinement without sharing the host home, credentials or build tree.
set -euo pipefail

cd "$(dirname "$0")/.."
snapshot=$(mktemp -d /tmp/keke-linux-source.XXXXXX)
trap 'rm -rf "$snapshot"' EXIT
git ls-files --cached --others --exclude-standard -z |
    tar --null -T - -cf - | tar -xf - -C "$snapshot"

# Tests need native Linux paths: virtiofs is only an input, not the workspace
# Landlock will confine. --locked prevents dependency drift during validation.
container run --rm --name "keke-linux-tests-$$" --cpus 6 --memory 8G \
    --volume "$snapshot:/source:ro" rust:1.96-bookworm sh -c '
        set -eu
        uname -a
        mkdir /work
        cp -a /source/. /work/
        cd /work
        rustc --version
        cargo test --locked -p keke-sandbox
        cargo test --locked -p keke-cli --test sandbox
    '
