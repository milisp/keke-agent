# keke

[中文文档](README.zh-CN.md) | [Architecture](docs/architecture.md) | [Config](docs/config.md) | [Roadmap](docs/ROADMAP.md)

keke is a local terminal coding agent built for zero-vendor lock-in. 
Works with subscriptions you already have (ChatGPT, Grok), standard API keys, or self-hosted local models.k

## Why keke?

- **Protocol: ACP for every client**
  Speaks the open Agent Client Protocol for both external client integrations and its internal TUI/agent seam.
- **Multi-Account & Per-Directory Routing**
  Login to multiple subscription accounts (e.g., ChatGPT, Grok, work/personal) and automatically route requests based on workspace directory path.
- **Script & CI First (`keke exec`)**
  Supports one-shot execution out of the box for non-interactive scripting and automated CI pipelines.
- **Vendor-Isolated Engine**
  No vendor-specific logic inside `keke-core`. Adding standard model endpoints requires zero engine code changes — just a quick entry in `config.toml`.

## Install

### Shell (recommended)

```sh
curl -fsSL https://raw.githubusercontent.com/milisp/keke-agent/main/scripts/install.sh | sh
```

This downloads the latest prebuilt binary for your platform into
`~/.local/bin` (override with `KEKE_INSTALL_DIR`). Piping a remote script into
`sh` runs it with your privileges — inspect it first if that matters to you:
`curl -fsSL .../install.sh | less`.

### npm

```sh
npm install -g @milisp/keke
```

You can also grab a binary directly from the
[latest release](https://github.com/milisp/keke-agent/releases/latest), or build
from source with `cargo build --release`.

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

- **Sandboxing & approvals** — `approval_policy` and `sandbox_mode` are
  configurable to match how you run it ([`docs/config.md`](docs/config.md)).
- **Plugin trust** — repository-provided plugins (hooks, MCP servers) never
  execute on `git clone` alone; a person must approve them, keyed to their
  exact contents, not their path. There's no flag to turn that gate off.

## Status

Usable day to day. `keke exec`, the TUI, and the ACP server all run real
sessions end to end; runtime plugins (skills, commands, hooks, MCP servers)
install in the Claude Code format, and repository-provided ones stay inert
until you approve them. `/model` switches models inside a running session
(tied to their provider so config can't persist an invalid pairing), and the
agent can spawn subagents — isolated child sessions given one task that
report back a single answer instead of their whole search trace. See
[`docs/ROADMAP.md`](docs/ROADMAP.md) for what's next.

## License

Apache-2.0. See [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE). Design rationale
and source attribution are documented in
[`docs/architecture.md`](docs/architecture.md#why-it-is-shaped-this-way) and
`THIRD_PARTY_NOTICES.md` files in the relevant crates.
