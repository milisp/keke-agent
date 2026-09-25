# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [Unreleased]

## [0.1.29] - 2026-09-24

### Added
- OS-enforced command sandboxing on macOS (Seatbelt) and Linux (Landlock and
  seccomp), with explicit failure when the selected mode cannot be enforced.
- `bash_unsandboxed`, which lets a person approve a command outside the sandbox
  after reviewing its justification.
- Interactive session picker.

### Changed
- Background tasks run in the command sandbox too; read-only mode also refuses
  tools that would write outside it.
- Sandbox settings are documented, and repository configuration may tighten but
  not loosen the active sandbox.
- Agent-message wrapping now follows terminal resizes correctly.

### Fixed
- Sandbox metadata stays read-only, and missing workspace metadata is reported.
- Session cleanup, startup rendering, and sandbox CI workflow.

## [0.1.28] - 2026-09-22

### Added
- Independently visible command transcript entries.
- Per-provider-route model preferences and in-place provider switching in a
  fresh session.
- User messages are tinted to suit the terminal theme.

### Fixed
- Session cleanup checks only the session being closed; startup draws its first
  frame before measuring the banner's git diff.

## [0.1.27] - 2026-09-17

### Added
- Guards deny access to credential files and `.env` files, while allowing
  environment file templates.

### Changed
- Tool writes are contained to the workspace without restricting reads.

## [0.1.26] - 2026-09-16

### Fixed
- Project plugins are contained within their package root; other plugin scopes
  retain their existing behavior.

## [0.1.25] - 2026-09-15

### Added
- Guardian review: a model-backed approval reviewer.

## [0.1.24] - 2026-09-12

### Changed
- Subagent collection is bounded and returns the first completed result.

## [0.1.23] - 2026-09-12

### Added
- Startup prefers cached model catalogs; plan files live under keke home.

### Fixed
- Terminal cleanup on TUI input and exit.

## [0.1.22] - 2026-09-11

### Added
- Initial and early stream-read provider failures are retried.

### Fixed
- Turn state resets for new sessions, duplicate failed-turn updates are avoided,
  and transcript rendering is improved.

## [0.1.21] - 2026-09-09

### Added
- Word-based cursor movement and atomic multi-file patching with the ApplyPatch
  tool.

## [0.1.20] - 2026-09-07

### Fixed
- ACP tool-call descriptions now match the shape expected by clients.

## [0.1.19] - 2026-09-07

### Added
- Startup banner reports startup duration and tool/skill counts; configurable
  persona instructions.

## [0.1.18] - 2026-09-05

### Fixed
- Plugins from another harness's home directory are no longer auto-trusted.

## [0.1.17] - 2026-09-03

### Added
- Model-scheduled standing prompts and opt-in startup checkpoint tracing.
- Lazily opened, bounded per-session checkpoint indexes for git snapshots.

## [0.1.16] - 2026-09-01

### Added
- `/loop` repeats a prompt on an interval; background shell commands run as
  managed tasks.
- Hosted web search for Codex and Grok, with configurable access and session
  logging.
- `/export` writes transcript cells as Markdown.
- `/fast` selects a Codex service tier and can be changed mid-conversation.

### Fixed
- Rewind, transcript grouping, slash-command ordering, and empty-session cleanup.

## [0.1.15] - 2026-08-31

### Added
- Skills are available as slash commands, with deployment-level controls.
- Double-Escape rewind snapshots the working tree and lets a person choose what
  to restore.

## [0.1.14] - 2026-08-30

### Added
- MCP server enable/disable controls.

### Changed
- Architecture documentation now includes the crate dependency tiers.

## [0.1.13] - 2026-08-30

### Added
- Exact-match text replacement via the edit tool; tool-call output is easier to
  inspect and resume commands are printed after a session exits.

## [0.1.12] - 2026-08-29

### Added
- Plan mode across the core, ACP, and TUI, including plan review and approval.
- OpenRouter request attribution and keyed gateway presets.

### Changed
- Session storage and TUI command implementations were reorganized; crates use
  short names.

## [0.1.11] - 2026-08-28

### Added
- Remote MCP transports (streamable HTTP and HTTP+SSE) with OAuth login.
- Windows ARM64 release support.
- Session summaries and richer TUI status, picker, and banner.

### Fixed
- Session resume details and startup banner behavior.

## [0.1.10] - 2026-08-27

### Added
- Local signing script; macOS Developer ID signing and notarization are now
  integrated into the release workflow.
- `--last` flag and cwd filtering for `resume`.

### Changed
- The composer wraps to its box width, and its rows register for selection.
- Selection tracks rows across multiple widgets; wide-glyph columns render
  correctly.
- Safety information reorganized and project READMEs simplified.

## [0.1.9] - 2026-08-27

### Added
- Named provider instances, multi-account credentials, and per-repo provider
  selection.

### Changed
- Configuration details moved out of the README into a dedicated config guide.

## [0.1.8] - 2026-08-27

### Added
- Subagents: isolated child sessions a model can start and collect results
  from, drawn live under the turn status.
- Markdown rendering for assistant responses.

## [0.1.7] - 2026-08-26

### Added
- Turn status line with spinner, elapsed time, and context usage; full
  reasoning ladder fallback.

### Changed
- Model configuration is tied to a specific provider, preventing invalid
  cross-provider persistence.

### Fixed
- Token usage accounting.

## [0.1.6] - 2026-08-26

### Fixed
- npm OIDC trusted publishing in CI.

## [0.1.5] - 2026-08-26

### Added
- Header bar displaying the current directory and model context window usage.
- Model list caching for declared and Ollama providers.
- Ollama provider support.
- Interactive model picker overlay for `/model`.
- ACP authentication protocol support, with a login UI for CLI connections.
- Plugin slash commands advertised over ACP `session/update`.
- `new_session` for a full agent state reset and history clearance.

### Changed
- Configuration keys and internal naming migrated from kebab-case to
  snake_case.
- `CredentialNeed` introduced to enforce credential requirements during
  provider declaration and improve key-variable suggestions.

## [0.1.4] - 2026-08-25

### Added
- Custom CA certs, proxy auth, and custom headers for declared providers.
- `--format json` option for the `exec` command, with end-to-end testing.
- Esc key interrupts busy turns and handles cancellation during text
  streaming.
- Fuzzy file search, ported from grok-build into a new `keke-fuzzy-file-search`
  crate and integrated into the TUI.

## [0.1.3] - 2026-08-24

### Added
- The active approval policy is persisted in session logs and takes priority
  over config defaults when a session is resumed.
- TUI slash command overrides persist to `config.toml` instead of living only
  in memory for the session.

### Changed
- Removed redundant transcript notifications now that overrides persist to
  config directly.

### Removed
- `/mode`, in favor of the shift-tab gesture for approval policy management.
- The legacy `/mouse` and `/thinking` slash commands.

## [0.1.2] - 2026-08-24

### Added
- `/model` lists what the session's provider serves — display name, context
  window, and the reasoning levels each model takes — and switches between
  them. A model the provider does not serve is refused where it was typed
  rather than on the next prompt.
- Model catalogs carry reasoning levels. `keke-provider-codex` is a crate of
  its own, both vendors decode their subscription backends' richer `/models`
  listings, and each ships its vendor's own catalog as a floor — so a picker is
  drawn offline, behind a proxy, and before the first login.
- Catalogs are cached under `<keke-home>/cache/models`, for
  `model-catalog-ttl-seconds` (default six hours). A vendor that cannot be
  reached falls through to the last answer it gave, and then to the
  compiled-in list.
- ACP sessions offer a `reasoning_effort` config option alongside `model`,
  populated from what the selected model actually accepts, with the model's
  own default reachable as a named choice.
- `ultra`, the rung above `max`, which the newest OpenAI models take.
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
- ACP model options are labelled with what the vendor calls the model rather
  than with its slug twice.
- `/effort` cycles the ladder the current model published, when it published
  one, instead of every rung keke knows. Switching to a model that does not
  take the level in force drops it and says so.
- The status bar names the model that is answering.
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
  mid-conversation reasoning_effort control.
- History summarization to keep long sessions inside the context window.
- `docs/PROGRESS.md` / `docs/ROADMAP.md` tracking crate status and next steps.

[0.1.2]: https://github.com/milisp/keke-agent/releases/tag/v0.1.2
[0.1.1]: https://github.com/milisp/keke-agent/releases/tag/v0.1.1
