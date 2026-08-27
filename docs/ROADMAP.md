# Roadmap

A running log of what's built and what's next. Update this alongside any
change that moves the project from one milestone to another — don't let it
drift the way the README status line did.

## Done

- **Foundations** — tier 0 contracts, `keke-core` turn loop, `keke exec`,
  wire formats (chat_completions/responses/messages), API-key auth, ACP
  server, TUI with slash commands and shift-tab approval cycling, runtime
  plugins (skills/commands/hooks/MCP), reasoning-effort ladder, `keke-mcp`,
  `keke-skills`, `keke resume`. Full detail in `git log`, not here.
- **`keke-provider-codex`** — OpenAI/ChatGPT provider on top of `keke-wire`,
  paired with `keke-auth-codex`'s OAuth flow.
- **Live turn clock and token count** in the TUI status bar.
- **Ollama provider**, with model-list caching for it and other declared
  providers.
- **In-session model switching** — `/model` picker, model tied to its
  provider in config, switch drives a `new_session` reset.
- **Subagents** (`keke-subagent`) — isolated child sessions the model can
  spawn for one task and collect one answer from; cannot themselves spawn
  subagents.
- **Markdown rendering** for assistant responses in the TUI.

## In progress / next

- **MCP tool-call closure, end to end** — install a real MCP plugin
  (GitHub or filesystem server), drive it from the TUI so the agent actually
  triggers a tool call, and confirm `ApprovalReviewContributor`/`ToolGuard`
  intercept as expected (invariant 7) and the resulting `SessionEvent`s are
  complete (invariant 6). The transport and tool-call plumbing exist
  (`keke-mcp`); what's missing is a verified real-plugin run.

## How to use this file

- When a milestone lands, move it from "next" to "done" in the same PR.
- Keep entries one or two lines — this is a status board, not a changelog;
  `git log` is the changelog.
- If something here contradicts `git log` or the code, trust the code and
  fix this file.
