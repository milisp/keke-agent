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
- **Remote MCP servers, with OAuth login** — streamable-HTTP and HTTP+SSE
  transports, `keke mcp add|list|get|remove` for configuring servers without
  authoring a plugin, and `keke mcp login <name>` / `/mcp login <name>`
  running RFC 9728 discovery + RFC 7591 client registration + PKCE, with
  token refresh on expiry and on 401. `keke-oauth` holds the PKCE and
  loopback-redirect logic once instead of once per vendor auth crate.

## In progress / next

- **MCP tool-call closure against a real server** — a real remote server
  (Vercel's) is configured, signed in via OAuth, and driven from the TUI,
  confirming the tool call itself fires end to end. Still unconfirmed:
  `ApprovalReviewContributor`/`ToolGuard` intercept it as expected
  (invariant 7) and the resulting `SessionEvent`s are complete (invariant 6).

## How to use this file

- When a milestone lands, move it from "next" to "done" in the same PR.
- Keep entries one or two lines — this is a status board, not a changelog;
  `git log` is the changelog.
- If something here contradicts `git log` or the code, trust the code and
  fix this file.
