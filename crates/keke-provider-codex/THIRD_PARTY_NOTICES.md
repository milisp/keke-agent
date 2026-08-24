# Third-party notices

## codex

- Upstream: <https://github.com/openai/codex>
- License: Apache-2.0
- Ported files:
  - `src/ported/codex/models.json` — the listed models from
    `codex-rs/models-manager/models.json`, reduced to the fields keke's
    `ModelInfo` carries.

This is OpenAI's own statement of which models the ChatGPT backend serves and
which reasoning levels each one accepts. It is copied so that it can be updated
by comparing against upstream rather than rediscovered from a rejected request,
and it is only a floor: a catalog fetch that succeeds replaces it.
