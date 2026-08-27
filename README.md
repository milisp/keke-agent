# keke

[中文文档](README.zh-CN.md) | [Architecture](docs/architecture.md) | [Config](docs/config.md) | [Roadmap](docs/ROADMAP.md)

keke is a local terminal coding agent built for zero-vendor lock-in. 
Works with subscriptions you already have, standard API keys, or self-hosted local models.

**Status: early (v0.1.x), usable day to day.** Good fit for personal daily
driving, scripted/CI use, and small-team trials. Not yet the right choice for
a regulated or uninterruptible production pipeline — see
[Production & safety](#production--safety).

## Why keke?

- **Protocol: ACP for every client**
  Speaks the open Agent Client Protocol for both external client integrations and its internal TUI/agent seam.
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

## What you can point it at

| Provider | How you authenticate | Notes |
| --- | --- | --- |
| OpenAI / ChatGPT | `keke login codex`, or `OPENAI_API_KEY` | OAuth flow ported from codex |
| Anthropic Claude | `ANTHROPIC_API_KEY` | Built in; the default provider. API key only — no subscription login |
| xAI Grok | `keke login grok`, or `XAI_API_KEY` | Built in |
| Local (Ollama, vLLM, …) | none | Your code never leaves the machine |
| Any OpenAI-compatible gateway | `env_key` in `config.toml` | Company proxies, NVIDIA NIM, routers |

Anything not built in is a few lines of `$KEKE_HOME/config.toml`, not a code
change:

```toml
[providers.ollama]
base_url = "http://localhost:11434/v1"
default_model = "gpt-oss:20b"
```

Corporate proxies, TLS-intercepting gateways, custom headers, and the full
field reference live in [`docs/config.md`](docs/config.md) — the shape is the
same declaration, just with more fields filled in.

## Status

Usable day to day. `keke exec`, the TUI, and the ACP server all run real
sessions end to end; runtime plugins (skills, commands, hooks, MCP servers)
install in the Claude Code format, and repository-provided ones stay inert
until you approve them. `/model` switches models inside a running session
(tied to their provider so config can't persist an invalid pairing), and the
agent can spawn subagents — isolated child sessions given one task that
report back a single answer instead of their whole search trace. See
[`docs/ROADMAP.md`](docs/ROADMAP.md) for what's next.

## Production & safety

Fine for personal use, local/self-hosted models, and CI one-shots today.
Before trusting it with anything you can't afford to break, weigh in:

- **Sandboxing & approvals** — default `approval_policy` and `sandbox_mode`
  are conservative, but you should confirm they match what you're running
  ([`docs/config.md`](docs/config.md)).
- **Plugin trust** — repository-provided plugins (hooks, MCP servers) never
  execute on `git clone` alone; a person must approve them, keyed to their
  exact contents, not their path. There's no flag to turn that gate off.
- **Maturity** — early (v0.1.x), single-maintainer, no formal security audit
  or SLA. Remaining roadmap gaps (full end-to-end MCP tool-call verification)
  are tracked in [`docs/ROADMAP.md`](docs/ROADMAP.md).

**Good fit:** you want no vendor lock-in, script/CI-friendly one-shot runs,
local models, or ACP integration into your own editor/client.
**Not yet a fit:** you need turnkey OAuth for every vendor, an established
plugin ecosystem, or vendor support contracts.

## Where it comes from

keke is a fresh implementation informed by OpenAI's **codex**, xAI's
**grok-build**, and **deepseek-harness**. A few pieces are ported and
attributed in-crate; most of the code is original. Design rationale and
CI-enforced invariants: [`docs/architecture.md`](docs/architecture.md),
[`AGENTS.md`](AGENTS.md).

## License

Apache-2.0. See [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE); code ported from
other projects is attributed in the `THIRD_PARTY_NOTICES.md` of the crate that
contains it.
