# Third-party notices

## codex

`src/ported/codex/seatbelt_base_policy.sbpl` and
`src/ported/codex/seatbelt_network_policy.sbpl` are ported unmodified from
`codex-rs/sandboxing/src/` in openai/codex, licensed Apache-2.0. codex took
parts of both from Chromium's macOS sandbox policies, which the files cite.

The list of system calls the Linux network filter refuses, in `src/linux.rs`,
follows `codex-rs/linux-sandbox/src/landlock.rs` in the same repository.
