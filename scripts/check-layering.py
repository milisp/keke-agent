#!/usr/bin/env python3
"""Enforce the crate tiering described in docs/architecture.md.

The failure this guards against is the one codex-rs documented in its own
AGENTS.md ("resist adding code to codex-core") and then suffered anyway: the
engine slowly accumulating dependencies on vendor plugins until no part of it
can be replaced. A rule that lives only in prose does not hold, so it lives
here and runs in CI.
"""

from __future__ import annotations

import json
import subprocess
import sys

# Ranks, not just tiers: a crate may only depend on a strictly lower rank, so
# the ordering *inside* the contract tier is enforced too. Without that, two
# contract crates could grow a mutual dependency and the tier check would still
# pass.
RANK = {
    # tier 0 - contract crates
    "keke-paths": 0,
    "keke-protocol": 1,
    "keke-tool": 2,
    "keke-config-types": 3,
    "keke-provider-api": 3,
    "keke-auth-api": 3,
    "keke-plugin-api": 4,
    # tier 0.5 - shared wire implementation, above the contracts and below the
    # vendor plugins that configure it
    "keke-wire": 5,
    # tier 1 - engine
    "keke-config": 10,
    "keke-credentials": 10,
    "keke-workspace": 11,
    "keke-core": 12,
    # test support sits beside the plugins: it may use the contracts, and
    # anything may depend on it as a dev-dependency
    "keke-test-support": 15,
    # tier 1.5 - the runtime-plugin manifest layer. Below the plugins because
    # the extension crate for each contribution kind (skills, hooks, MCP) reads
    # a resolved `PluginSet` and registers it through the ordinary contributor
    # traits. It depends on `keke-paths` and nothing else, so a manifest can be
    # parsed and listed without linking the engine.
    "keke-plugin": 16,
    # tier 3 - surfaces
    "keke-acp": 30,
    "keke-tui": 31,
    "keke-cli": 32,
}
# Anything else in the workspace is a plugin: above the engine, below surfaces.
PLUGIN_RANK = 20

# The engine must not know about any specific vendor. `keke-cli` is the only
# crate allowed to name a vendor plugin, because it is the composition root.
VENDOR_PREFIXES = ("keke-provider-", "keke-auth-")
VENDOR_FREE = {"keke-core", "keke-config", "keke-workspace", "keke-acp", "keke-tui"}


def rank(name: str) -> int:
    """Every workspace crate has a rank; unlisted ones are plugins."""
    return RANK.get(name, PLUGIN_RANK)


def main() -> int:
    meta = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--no-deps"],
            capture_output=True,
            check=True,
            text=True,
        ).stdout
    )
    members = {pkg["name"] for pkg in meta["packages"]}
    failures: list[str] = []

    for pkg in meta["packages"]:
        name = pkg["name"]
        for dep in pkg["dependencies"]:
            dep_name = dep["name"]
            if dep_name not in members:
                continue

            if rank(dep_name) >= rank(name):
                failures.append(
                    f"{name} (rank {rank(name)}) depends on "
                    f"{dep_name} (rank {rank(dep_name)}): dependencies point strictly downward"
                )

            if name in VENDOR_FREE and dep_name.startswith(VENDOR_PREFIXES):
                # `keke-provider-api` / `keke-auth-api` are the seams, not vendors.
                if not dep_name.endswith("-api"):
                    failures.append(
                        f"{name} depends on vendor plugin {dep_name}: "
                        "only keke-cli may name a vendor"
                    )

    for failure in sorted(failures):
        print(f"layering violation: {failure}", file=sys.stderr)

    if failures:
        print(f"\n{len(failures)} layering violation(s)", file=sys.stderr)
        return 1
    print(f"layering ok: {len(members)} crates checked")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
