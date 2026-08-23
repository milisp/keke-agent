# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [Unreleased]

### Added
- `^Y` / `/copy` puts the last reply on the clipboard, via OSC 52 so it works
  over ssh and inside a multiplexer.
- A prompt taller than the composer scrolls inside it instead of hiding the
  cursor.
- A count of what is below, centred under the transcript while the reader has
  scrolled back, and clickable to get back to the tail.
- `/mouse` gives the mouse back to the terminal, for terminals with no bypass
  modifier for drag-select.
- A status-bar flash for what keke just did — copied, resumed — which expires
  instead of accumulating in the transcript.

### Changed
- The wheel scrolls the conversation, by mouse capture where the terminal
  takes it and by alternate scroll mode where it does not — an empty composer
  gives the arrow keys to the transcript.
- Prompt history moved to Ctrl-P / Ctrl-N. The arrows could not be relied on
  for it once the wheel started arriving as arrow keys.
- Resuming a session says so in the status bar instead of opening the
  transcript with a line that reads as something the agent said.
- An answered approval no longer keeps its key list on screen, and only
  "always allowed" is spelled out — the ✓ and ⊘ markers already say the rest.

### Removed
- The status bar no longer captions its own key bindings.

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
