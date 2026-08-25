//! Fuzzy file search over a directory tree, for `@`-completion in the composer.
//!
//! The implementation is ported wholesale from grok-build's
//! `xai-fuzzy-file-search`; see `src/ported/grok_build`.

mod ported;

pub use ported::grok_build::*;
