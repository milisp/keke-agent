# keke

> **keke** is an unbundled, BYOK, provider-agnostic AI coding harness built in Rust. 
> Inspired by OpenAI codex, xAI grok-build, and deepseek-harness — 
> featuring strict CI-enforced layering where vendor-specific behavior lives in 
> replaceable plugins rather than a monolith.

## Status

`keke exec`, the ACP server, and the TUI all work end to end, with runtime plugin operations gated behind consent. Check [`docs/PROGRESS.md`](docs/PROGRESS.md) for the active roadmap.

## Design in one paragraph

Adding a vendor should mean adding two small crates (a model provider and an auth
provider) plus one line in the composition root, with no change to the engine.
Three seams make that possible — model providers, authentication, and tools —
each defined by a dependency-light contract crate that vendors implement and the
engine consumes. A layering check in CI keeps the engine from learning that any
particular vendor exists.

See [`docs/architecture.md`](docs/architecture.md) for the reasoning, and
[`AGENTS.md`](AGENTS.md) for the invariants contributors must hold.

## Try it

Works out of the box with your existing API keys, local models, or CLI logins:

```sh
# 1. Quick start with environment variables
export ANTHROPIC_API_KEY=sk-ant-...  # or OPENAI_API_KEY, XAI_API_KEY
keke exec "what does this project do?"

# 2. Interactive logins
keke login codex
keke login grok

# 3. Resume & inspect sessions
keke resume                          # pick up the last conversation
keke doctor                          # inspect resolved providers & credentials


Point it at any endpoint without touching the code — declare
it in `$KEKE_HOME/config.toml`:

```toml
[providers.ollama]
base-url = "http://localhost:11434/v1"
default-model = "gpt-oss:20b"
```

`wire = "chat-completions" | "responses" | "messages"` picks the format; the
default is chat completions.

## Layout

```
crates/
  keke-paths  keke-protocol  keke-tool                 # tier 0: contracts
  keke-config-types  keke-provider-api  keke-auth-api
  keke-plugin-api
  keke-core  keke-config  keke-credentials  keke-workspace   # tier 1: engine
  keke-wire                                                  # the three wire formats
  keke-provider-grok                                          # tier 2: plugins
  keke-auth-grok  keke-auth-codex  keke-tools
  keke-cli                                                    # tier 3: surfaces
```

## Development

```sh
cargo fmt --all --check
RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets
cargo test --workspace
python3 scripts/check-layering.py
```

## License

Apache-2.0. See [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE); code ported from
other projects is attributed in the `THIRD_PARTY_NOTICES.md` of the crate that
contains it.
