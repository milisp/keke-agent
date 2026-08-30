# Third-party notices

## codex

- Upstream: <https://github.com/openai/codex>
- License: Apache-2.0
- Ported files:
  - `src/ported/codex/authorize.rs` — from `codex-rs/login/src/server.rs`
    (`build_authorize_url`, `DEFAULT_PORT`, and the redirect URI it builds).

These describe OpenAI's OAuth client registration: which redirect URI is
accepted, and which parameters the authorize endpoint requires. They are
copied so that they can be updated by comparing against upstream rather than
rediscovered from an `invalid_authorize_request`.
