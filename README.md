# keke

A multi-vendor terminal coding agent, built so that vendor-specific behavior
lives in replaceable plugins rather than in a monolith.

> Status: `keke exec` works end to end — a turn runs, tools execute, and the
> session is recorded in a replayable log. The xAI provider and its OAuth flow
> are in; ChatGPT, the ACP server, and the TUI are next.

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
export XAI_API_KEY=...        # or: cargo run -p keke-cli -- login
cargo run -p keke-cli -- exec "what does this project do?"
cargo run -p keke-cli -- doctor
```

## Layout

```
crates/
  keke-paths  keke-protocol  keke-tool                 # tier 0: contracts
  keke-config-types  keke-provider-api  keke-auth-api
  keke-plugin-api
  keke-core  keke-config  keke-credentials  keke-workspace   # tier 1: engine
  keke-provider-xai  keke-auth-xai  keke-tools               # tier 2: plugins
  keke-cli                                                   # tier 3: surfaces
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
