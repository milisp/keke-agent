# keke

[中文文档](README.zh-CN.md) | [Architecture](docs/architecture.md) | [Config](docs/config.md) | [Roadmap](docs/ROADMAP.md)

**keke** is a local terminal coding agent built in Rust for zero vendor lock-in.  
**7 MB download · zero external runtime dependencies · instant startup.**

[![asciicast](https://asciinema.org/a/eUqMzR5n59Pfsta5.svg)](https://asciinema.org/a/eUqMzR5n59Pfsta5)

## Why keke?

- **Use what you already pay for**  
  Sign in with ChatGPT (Codex) or Grok subscriptions, or drop in any API key. No need to switch providers just to try a different model.

- **Multi-account, per-directory routing**  
  Log in to work and personal accounts once. keke automatically picks the right one based on the directory you’re in.

- **Script- and CI-friendly**  
  `keke exec "..."` runs one-shot tasks non-interactively — ready for scripts and pipelines.

- **Vendor-isolated core**  
  `keke-core` contains zero vendor-specific logic. Point it at any OpenAI-compatible endpoint with a few lines in `config.toml`.

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

- **Sandboxing & approvals** — `approval_policy` and `sandbox_mode` are
  configurable to match how you run it ([`docs/config.md`](docs/config.md)).
- **Plugin trust** — repository-provided plugins (hooks, MCP servers) never
  execute on `git clone` alone; a person must approve them, keyed to their
  exact contents, not their path. There's no flag to turn that gate off.

## License

Apache-2.0. See [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE). Design rationale
and source attribution are documented in
[`docs/architecture.md`](docs/architecture.md#why-it-is-shaped-this-way) and
`THIRD_PARTY_NOTICES.md` files in the relevant crates.
