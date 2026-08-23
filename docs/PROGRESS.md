# Progress

A running log of what's built and what's next. Update this alongside any
change that moves the project from one milestone to another — don't let it
drift the way the README status line did.

## Done

- **Engine core** — tier 0 contracts (`keke-protocol`, `keke-tool`,
  `keke-provider-api`, `keke-auth-api`, `keke-config-types`, `keke-paths`),
  `keke-core` turn loop, session event log.
- **`keke exec`** — a turn runs end to end, tools execute, the session is
  replayable from `SessionEvent`s.
- **Wire formats** — chat-completions, responses, and messages implemented
  once in `keke-wire`; config-declared providers pick one via `wire = ...`.
- **Auth** — API-key auth for config-declared endpoints, credential storage
  independent of the machine keyring in tests, `keke doctor` reports
  resolved vs. missing.
- **Vendor plugins** — `keke-provider-grok`, `keke-provider-nvidia`,
  `keke-auth-grok`, `keke-auth-codex`.
- **ACP server** — serves the Agent Client Protocol over stdio; an editor
  can drive a real session through it.
- **TUI** — `keke-tui` built against the ACP seam, not the engine directly.
- **Runtime plugins** — Claude Code-format plugin install/update/remove
  (`keke-plugin`), skills/commands/hooks/MCP servers as declared manifests
  (invariant 11), repository-sourced plugins gated behind explicit consent
  keyed to their contents, not their path (invariant 12).
- **MCP** — `keke-mcp` speaks both the old and modern MCP transport eras.
- **`keke-skills`** — plugin-contributed `skills/*/SKILL.md` become a
  `ContextContributor` wired in `keke-cli/src/compose.rs`: only the
  `plugin:name — description` index line is injected up front, the body is
  read on demand by qualified name.

## In progress / next

- **MCP tool-call closure, end to end** — install a real MCP plugin
  (GitHub or filesystem server), drive it from the TUI so the agent actually
  triggers a tool call, and confirm `ApprovalReviewContributor`/`ToolGuard`
  intercept as expected (invariant 7) and the resulting `SessionEvent`s are
  complete (invariant 6). The transport and tool-call plumbing exist
  (`keke-mcp`); what's missing is a verified real-plugin run.
- **`keke-provider-chatgpt` (or similarly named OpenAI/ChatGPT provider)** —
  only `keke-provider-grok` and `keke-provider-nvidia` exist today. `keke-wire`
  already implements all three wire formats, so this is a new provider plugin
  on top of existing plumbing, validated for streaming tokens, reasoning
  models, and error frames.

## How to use this file

- When a milestone lands, move it from "next" to "done" in the same PR.
- Keep entries one or two lines — this is a status board, not a changelog;
  `git log` is the changelog.
- If something here contradicts `git log` or the code, trust the code and
  fix this file.
