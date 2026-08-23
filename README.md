# keke

A multi-vendor terminal coding agent, built so that vendor-specific behavior
lives in replaceable plugins rather than in a monolith.

> Status: `keke exec`, the ACP server, and the TUI all work end to end, with
> runtime plugin install/update/remove gated behind consent. See
> [`docs/PROGRESS.md`](docs/PROGRESS.md) for what's done and what's next.

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

```sh
keke login grok               # or: export XAI_API_KEY=...
keke exec "what does this project do?"
keke doctor                   # what got resolved, and what is missing
```

Point it at any OpenAI-compatible endpoint without touching the code — declare
it in `$KEKE_HOME/config.toml`:

```toml
[providers.ollama]
base-url = "http://localhost:11434/v1"
default-model = "gpt-oss:20b-cloud"

[providers.nvidia]
base-url = "https://integrate.api.nvidia.com/v1"
env-key = "NVIDIA_API_KEY"
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
