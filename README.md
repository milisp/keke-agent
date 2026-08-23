# keke

[![CI](https://github.com/milisp/keke/actions/workflows/ci.yml/badge.svg)](https://github.com/milisp/keke/actions)

[中文文档](README.zh-CN.md)

keke is a coding agent that runs locally in your terminal and works with any
model — a subscription you already have, an API key, or a model on your own
machine.

If you want keke in your editor, it serves the Agent Client Protocol over
stdio. If you want it in a script or in CI, use `keke exec`. If you want a
model it does not know about, declare it in `config.toml` — there is no
vendor-specific code in the engine to change.

## Install

Download a prebuilt binary from the
[latest release](https://github.com/milisp/keke/releases/latest), or build
from source with `cargo build --release`.

## Try it

```sh
# Sign in with a subscription you already pay for
keke login codex
keke login grok

# ...or bring a key
export XAI_API_KEY=xai-...

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

keke is built on what three open agents already proved out: the OAuth login
flows of OpenAI's **codex** (ported, and attributed in the crate that carries
them), the model and wire coverage of xAI's **grok-build**, and the
seam-first architecture that **deepseek-harness** argues for.

What keke does differently is refuse to let vendor knowledge into the middle.
codex's own contributor guide says *"resist adding code to codex-core"*, and
`codex-core` still depends on 68 internal crates today — prose did not hold
that line. In keke, `scripts/check-layering.py` holds it and fails CI instead,
which is why adding a vendor stays a plugin plus one line in the composition
root. See [`docs/architecture.md`](docs/architecture.md) for the reasoning and
[`AGENTS.md`](AGENTS.md) for the invariants.

## License

Apache-2.0. See [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE); code ported from
other projects is attributed in the `THIRD_PARTY_NOTICES.md` of the crate that
contains it.
