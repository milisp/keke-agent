# keke — agent guide

keke is a multi-vendor terminal coding agent. Vendor-specific behavior lives in
replaceable plugins; the engine does not know that ChatGPT or Grok exist.

Read `docs/architecture.md` before making structural changes.

## Invariants

These are not style preferences. Each one exists because a reference
implementation lost it and paid for it.

1. **Nothing vendor-specific in `keke-core`.** No `keke-provider-*` or
   `keke-auth-*` dependency, no vendor name in a match arm. OpenAI's codex wrote
   *"resist adding code to codex-core"* in its own guide and `codex-core` still
   ended up depending on ~50 internal crates. Prose did not hold the line, so
   `scripts/check-layering.py` holds it instead and runs in CI.

2. **Dependencies point strictly downward by rank.** Contract crates (tier 0)
   depend on nothing above them and not on each other except in the documented
   order. Adding a crate means adding it to `RANK` in the layering script.

3. **Contract crates stay dependency-light.** `keke-protocol`, `keke-tool`,
   `keke-auth-api`, `keke-provider-api`, `keke-plugin-api`, `keke-config-types`,
   and `keke-paths` may use `serde`, `futures`, `thiserror`, `schemars`, and each
   other. No HTTP client, no runtime, no engine. This is what lets a vendor
   plugin authenticate without linking the world.

4. **RPITIT over `#[async_trait]`.** Write
   `fn f(&self, ..) -> impl Future<Output = T> + Send`. Where a trait must be
   held as `dyn`, box explicitly at that one boundary — see `keke_tool::ToolDyn`
   and `keke_plugin_api::ExtFuture`. Do not reach for `#[async_trait]`.

5. **Registration returns a disposer, or is composed once and frozen.** A
   registry either hands back something whose drop removes the contribution, or
   is a builder that produces an immutable value. A contribution must never be
   able to outlive the thing that made it.

6. **Model-visible implies logged.** Anything reaching a model request must be
   reconstructable from `SessionEvent`s. Adding a new kind of model-visible
   input means adding a variant to `keke_protocol::SessionEvent` first.

7. **Denial is monotonic.** Approval reviewers may allow or deny; `ToolGuard`s
   may only deny. No ordering of extensions can turn a denial back into
   permission.

8. **Ambiguity fails loud.** Two providers claiming a route with none configured
   is an error, not a silent pick. An empty stored credential is absent
   everywhere and never counts as configured.

9. **No deployment-varying constant hidden in a plugin.** If a deployment might
   reasonably want to change it, it is a validated field in
   `keke-config-types`, not a `DEFAULT_*` in the plugin.

10. **Ported code is attributed.** Files ported from another project live under
    `src/ported/<project>/`, carry a header naming the upstream path, and are
    listed in that crate's `THIRD_PARTY_NOTICES.md`. keke never takes a path or
    git dependency on `../codex` or `../grok-build` — it must build from its own
    checkout alone.

11. **Runtime plugins are data, never code.** Rust has no stable dylib ABI, so a
    plugin ships a manifest — skills, commands, hooks, MCP servers — and never a
    library keke loads into itself. What it brings that executes runs as a child
    process, under a budget that is a validated `keke-config-types` field. There
    is no setting for "no budget": a hook runs before the tool it guards, so one
    that never returns does not slow the turn down, it stops it.

12. use parallel subagent when you should

13. **Repository content does not execute without consent.** A plugin under the
    workspace is content the repository controls, so its hooks and MCP servers
    are withheld until a person approves them — `git clone` must never be
    sufficient for execution. Approval is of contents, not of a path: the
    recorded command lines must equal what the plugin contributes now, or it is
    untrusted again. Gate execution only, since repository *text* already
    reaches the model through `AGENTS.md`, and ship no flag that turns the gate
    off — a global bypass is what a person enables once and then leaves on.

## Conventions

- Crates are named `keke-*`; the directory matches the crate name.
- Extension crates expose `pub fn install(registry: &mut ExtensionRegistryBuilder, ..)`.
  `keke-cli` is the only composition root and the only place `install` is called.
- New traits get a doc comment explaining their role and what implementers are
  expected to do — a trait without one is incomplete.
- Comments explain *why*, not *what*. A comment restating the code is noise.
- Tests assert behavior described in prose above, not implementation details.
  `a_permissive_guard_cannot_undo_a_restrictive_one` is the model.
- The runtime-plugin format is the Claude Code ecosystem's, not keke's:
  `plugin.json` plus its convention directories. A plugin system is worth what
  its catalog is worth on the day it ships, so read what is already published
  rather than inventing a schema that would start empty. keke's own strictness
  goes into how failures are reported, not into the file names.

## Checks

```sh
cargo fmt --all --check
RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets
cargo test --workspace
python3 scripts/check-layering.py
```
