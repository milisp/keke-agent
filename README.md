# keke

[中文文档](README.zh-CN.md) | [Architecture](docs/architecture.md) | [Config](docs/config.md) | [Roadmap](docs/ROADMAP.md)

**keke** is a local coding agent in a ~7 MB native binary — no Node or Python
runtime. Sign in with the ChatGPT or Grok subscription you already pay for, or
bring any API key, all from one tool. The TUI, `keke exec`, and desktop apps
(over ACP) drive the same session.

Subagents, MCP, skills, Claude-compatible plugins, hooks, plan mode,
per-turn checkpoints, and OS sandboxing are built in. Memory and repository
plugins stay off until you turn them on.

[![asciicast](https://asciinema.org/a/1266474.svg)](https://asciinema.org/a/1266474)

## Why keke?

- **One session, every surface.** Interactive TUI and `keke exec` for scripts
  and CI, both on one session protocol. Concurrent subagents (bounded, and a
  child cannot spawn another), background shell tasks, and `/loop` for a prompt
  that comes back later. Plan mode can research and write a plan, not the repo.

- **Use what you already pay for.** Sign in with ChatGPT (Codex) or Grok, or
  any API key. Codex, Grok, Claude, OpenAI-compatible endpoints, Ollama, and
  vLLM — Chat Completions, Responses, or Anthropic Messages. Several accounts
  for one provider, picked by the directory you are in. A compatible endpoint
  is a few lines of `config.toml`; it does not require a new build. The engine
  crates cannot depend on any vendor plugin — CI fails the build if they do
  ([`scripts/check-layering.py`](scripts/check-layering.py)).

- **Off by default.** Skills can be disabled. Persistent memory
  is off until you set a directory. Hooks and MCP servers that ship with a
  repository don't run on `git clone` (see [Safety](#safety)).

- **Checkpoints outside your repo.** Each turn snapshots the working
  tree in a store outside your repository, so reverting the agent's edits is
  not a stash and not a commit.

## Install

### npm

```sh
npm install -g @milisp/keke
```

You can also grab a binary directly from the
[latest release](https://github.com/milisp/keke-agent/releases/latest), or build
from source with `cargo build --release`.

### Shell

```sh
curl -fsSL https://raw.githubusercontent.com/milisp/keke-agent/main/scripts/install.sh | sh
```

This downloads the latest prebuilt binary for your platform into
`~/.local/bin` (override with `KEKE_INSTALL_DIR`). Piping a remote script into
`sh` runs it with your privileges — inspect it first if that matters to you:
`curl -fsSL .../install.sh | less`.

## Try it (30 seconds)

```sh
keke doctor                              # see which providers/logins resolve, before you rely on one

# Sign in with a subscription you already pay for...
keke login codex
keke login grok
# ...or bring a key
export ANTHROPIC_API_KEY=sk-ant-...

keke exec "what does this project do?"   # one-shot, for scripts and CI
keke                                     # interactive TUI
keke resume                              # pick the last conversation back up
```

Provider routes, API keys, local models, gateways, per-directory accounts, and
all other settings are documented in [`docs/config.md`](docs/config.md).

## Safety

- **Sandboxing & approvals** — shell commands run under an OS sandbox
  (Seatbelt on macOS, Landlock + seccomp on Linux): by default they may write
  only inside the workspace and have no direct network access; on macOS, and
  on Linux with the opt-in bubblewrap backend, domains and ports can be
  approved per command. Git hooks and config are read-only
  (enforced on macOS). Stepping outside is reviewed each time — by the guardian
  in Auto mode, otherwise by you; `approval_policy = "never"` skips that
  review. On Windows, which has no sandbox, every command asks you instead. A
  repository's own config can tighten that but never loosen it. CI fails
  rather than skips if the runner cannot enforce the sandbox. See
  [`docs/config.md`](docs/config.md#sandbox).
- **Plugin trust** — repository-provided plugins (hooks, MCP servers) never
  execute on `git clone` alone; a person must approve them, keyed to their
  exact contents, not their path, and nothing they add can undo a denial.
  There's no flag to turn that gate off.

## License

Apache-2.0. See [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE). Design rationale
and source attribution are documented in
[`docs/architecture.md`](docs/architecture.md#why-it-is-shaped-this-way) and
`THIRD_PARTY_NOTICES.md` files in the relevant crates.
