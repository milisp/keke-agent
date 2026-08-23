# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [0.1.1] - 2026-08-23

### Fixed
- Auth token refresh now actually reaches the issuer, holding the credential
  lock across the whole refresh instead of racing it.
- Grok login now spends its subscription auth at the subscription surface
  instead of the paid API, and asks for the scope the subscription proxy
  requires.
- Stopped reporting an out-of-credits account as an auth failure.
- Codex's authorize flow is now ported from upstream instead of re-derived.
- Removed stale `enter`/`slash` key instructions from the UI footer.

### Changed
- CI skips doc-only changes and cancels stale runs.
- CI release toolchain is pinned to `rust-toolchain.toml`'s channel.
- The `tui` subcommand is hidden from `--help`.

### Docs
- README updated with a release download link and a Chinese translation.
- README documents MCP 2026-07-28 and ACP 2.0 support, and was rewritten to
  clarify architecture with updated quick-start examples.
- `PROGRESS.md` renamed to `ROADMAP.md`.

## [0.1.0] - Initial development

Foundational engine, TUI, provider wiring, and plugin system:

- Core session engine with a seam every surface (CLI, TUI, ACP) talks through.
- Declarative provider configuration and per-vendor auth (codex, grok), with
  the three model wire formats (Responses, Chat Completions, Anthropic)
  implemented once in `keke-wire`.
- Runtime plugin installation (install/update/remove) gated by explicit
  consent — cloning a repository is never enough to run what it ships.
- MCP server support across both the legacy and modern protocol eras, and an
  ACP server over stdio so editors can drive keke directly.
- TUI features: slash commands, live approval mode, session resume with
  elapsed time and running cost, readline-style input editing, and
  mid-conversation reasoning-effort control.
- History summarization to keep long sessions inside the context window.
- `docs/PROGRESS.md` / `docs/ROADMAP.md` tracking crate status and next steps.

[0.1.1]: https://github.com/milisp/keke/releases/tag/v0.1.1
