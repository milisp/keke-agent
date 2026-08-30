# Third-party notices

## grok-build

`src/ported/grok_build/plan_mode.rs` is ported from
`crates/codegen/xai-grok-shell/src/session/plan_mode.rs` in grok-build,
licensed Apache-2.0 — the plan-mode state machine and the reminder prose.
The persistence snapshot, the `PromptMode` mirror, and the MiniJinja templates
were dropped; the plan file path moved out of the tracker.
