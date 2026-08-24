# keke

[中文文档](README.zh-CN.md) | [Architecture](docs/architecture.md) | [Roadmap](docs/ROADMAP.md)

keke is a local terminal coding agent built for zero-vendor lock-in. 
Works with subscriptions you already have, standard API keys, or self-hosted local models.

## Why keke?

- **Protocol: ACP for every client**
  Speaks the open Agent Client Protocol for both external client integrations and its internal TUI/agent seam.
- **Script & CI First (`keke exec`)**
  Supports one-shot execution out of the box for non-interactive scripting and automated CI pipelines.
- **Vendor-Isolated Engine**
  No vendor-specific logic inside `keke-core`. Adding standard model endpoints requires zero engine code changes — just a quick entry in `config.toml`.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/milisp/keke-agent/main/scripts/install.sh | sh
```

This downloads the latest prebuilt binary for your platform into
`~/.local/bin` (override with `KEKE_INSTALL_DIR`).

You can also grab a binary directly from the
[latest release](https://github.com/milisp/keke-agent/releases/latest), or build
from source with `cargo build --release`.

## Try it

```sh
# Sign in with a subscription you already pay for
keke login codex
keke login grok

# ...or bring a key
export OPENAI_API_KEY=sk-...

keke exec "what does this project do?"   # one-shot, for scripts and CI
keke                                     # interactive TUI
keke resume                              # pick the last conversation back up
keke doctor                              # which providers and logins resolve
```

## What you can point it at

| Provider | How you authenticate | Notes |
| --- | --- | --- |
| OpenAI / ChatGPT | `keke login codex`, or `OPENAI_API_KEY` | OAuth flow ported from codex |
| xAI Grok | `keke login grok`, or `XAI_API_KEY` | Built in; the default provider |
| Anthropic | `env-key` in `config.toml` | Declare with `wire = "messages"` |
| Local (Ollama, vLLM, …) | none | Your code never leaves the machine |
| Any OpenAI-compatible gateway | `env-key` in `config.toml` | Company proxies, NVIDIA NIM, routers |

Anything not built in is a few lines of `$KEKE_HOME/config.toml`, not a code
change:

```toml
[providers.ollama]
base-url = "http://localhost:11434/v1"
default-model = "gpt-oss:20b"

[providers.anthropic]
base-url = "https://api.anthropic.com"
env-key = "ANTHROPIC_API_KEY"
wire = "messages"
```

`wire` picks the request format — `chat-completions` (the default),
`responses`, or `messages` — so a new endpoint is a config entry rather than a release.

## Status

Usable day to day. `keke exec`, the TUI, and the ACP server all run real
sessions end to end; runtime plugins (skills, commands, hooks, MCP servers)
install in the Claude Code format, and repository-provided ones stay inert
until you approve them. Switching models inside a running session is not
implemented yet — see [`docs/ROADMAP.md`](docs/ROADMAP.md).

## Where it comes from

keke is a fresh implementation built by studying three open agent repos:
OpenAI's **codex**, xAI's **grok-build**, and **deepseek-harness** — whose
seam-first architecture (a hard boundary between engine and vendor, not a
convention) is the idea keke leans on hardest. Some pieces are ported
outright and attributed in the crate that carries them (codex's OAuth login
flow, for instance); most of it is keke's own code, shaped by what worked and
what didn't in those three. What keke does differently — and why that
matters enough to enforce in CI — is in
[`docs/architecture.md`](docs/architecture.md); the invariants themselves are
in [`AGENTS.md`](AGENTS.md).

## License

Apache-2.0. See [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE); code ported from
other projects is attributed in the `THIRD_PARTY_NOTICES.md` of the crate that
contains it.
