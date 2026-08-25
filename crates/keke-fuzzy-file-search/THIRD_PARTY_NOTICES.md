# Third-party notices

## grok-build

`src/ported/grok_build/fuzzy_file_search.rs` is ported from
`crates/codegen/xai-fuzzy-file-search/src/lib.rs` in grok-build, licensed
Apache-2.0. The Unix `RLIMIT_NPROC` re-exec regression test was dropped: it
depends on `xai-tty-utils`, which keke does not vendor.
