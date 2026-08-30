# Third-party notices

## grok

- Upstream: <https://github.com/xai-org/grok-cli>
- License: Apache-2.0
- Ported files:
  - `src/ported/grok/models.json` — from
    `crates/codegen/xai-grok-models/default_models.json`, reduced to the fields
    keke's `ModelInfo` carries.

This is xAI's own statement of which models its CLI defaults to and which
reasoning levels each one accepts. It is copied so that it can be updated by
comparing against upstream rather than rediscovered from a rejected request,
and it is only a floor: a catalog fetch that succeeds replaces it.
