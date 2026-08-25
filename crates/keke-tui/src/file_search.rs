//! `@`-completion for the composer: a fuzzy file/folder picker.
//!
//! Design ported from grok-build's `xai-grok-pager` file-search view (see
//! `ported::grok_build::at_context` for the token-detection half, taken
//! nearly verbatim). This module is the state machine around it, rewritten
//! against keke-tui's single-line composer rather than a whole-document
//! editor: `@`-detection runs against the current input line only, using a
//! byte cursor derived from [`crate::input::InputBox::cursor`].

use std::path::PathBuf;
use std::sync::Arc;

use keke_fuzzy_file_search::{
    FuzzyFileMatcher, FuzzyFileMatcherDaemon, FuzzyMatchResult, FuzzyMatcherDaemonResults,
};

use crate::ported::grok_build::{self as context, AtContext, normalize_display_path};

/// Top-K results to request from the fuzzy matcher.
const MATCHER_TOP_K: usize = 200;

/// Replacement to apply to the current input line after accepting a result.
#[derive(Debug, Clone)]
pub struct FileSearchReplacement {
    /// Byte range in the line to replace (excludes the `@`).
    pub range: std::ops::Range<usize>,
    /// Replacement text: the normalized path, `/`-suffixed for a directory.
    /// [`crate::input::InputBox::replace_line_range`] puts the cursor right
    /// after it.
    pub text: String,
}

/// `@`-completion state: the fuzzy matcher daemon, the current token, and the
/// dropdown's selection.
pub struct FileSearchState {
    root: PathBuf,
    /// Built lazily on first `@`-use, so a session that never opens
    /// `@`-search never pays for the walker/matcher threads.
    daemon: Option<FuzzyFileMatcherDaemon>,
    results: FuzzyMatcherDaemonResults,
    context: Option<AtContext>,
    selected: usize,
    /// Stale-result fence: a new query bumps this, and a snapshot whose
    /// generation predates it is dropped. Keeps matches from a prior query
    /// from flickering into a newer, still-empty one.
    min_generation: usize,
}

impl FileSearchState {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            daemon: None,
            results: FuzzyMatcherDaemonResults::default(),
            context: None,
            selected: 0,
            min_generation: 0,
        }
    }

    fn ensure_daemon(&mut self) -> &mut FuzzyFileMatcherDaemon {
        self.daemon.get_or_insert_with(|| {
            FuzzyFileMatcherDaemon::new(FuzzyFileMatcher::new(&self.root), MATCHER_TOP_K)
        })
    }

    /// Whether the dropdown should be drawn.
    pub fn is_visible(&self) -> bool {
        self.context.is_some() && !self.results.topk.is_empty()
    }

    /// Whether an `@`-token is currently being typed, whether or not it has
    /// results yet. Drives the tick timer and Esc handling.
    pub fn is_open(&self) -> bool {
        self.context.is_some()
    }

    pub fn is_dir_mode(&self) -> bool {
        self.context.as_ref().is_some_and(AtContext::is_dir_mode)
    }

    pub fn results(&self) -> &[FuzzyMatchResult] {
        &self.results.topk
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Recompute the `@`-context from the current input line and cursor
    /// (both in bytes). Call after every edit to the line or cursor move.
    pub fn update(&mut self, line: &str, cursor_byte: usize) {
        let new_ctx = context::detect(line, cursor_byte);
        match (&self.context, &new_ctx) {
            (None, Some(ctx)) => self.start_query(ctx.matcher_query()),
            (Some(old), Some(new)) if old.is_dir_mode() != new.is_dir_mode() => {
                self.start_query(new.matcher_query());
            }
            (Some(_), Some(new)) => {
                let daemon = self.ensure_daemon();
                daemon.set_query(new.matcher_query(), new.is_dir_mode());
                self.min_generation += 1;
                self.selected = 0;
            }
            (Some(_), None) => {
                self.clear();
                return;
            }
            (None, None) => return,
        }
        self.context = new_ctx;
    }

    fn start_query(&mut self, query: &str) {
        let daemon = self.ensure_daemon();
        daemon.restart_walk(false);
        daemon.set_query(query, false);
        self.min_generation += 1;
        self.selected = 0;
    }

    /// Drop the current context (Esc, or the token was accepted).
    pub fn clear(&mut self) {
        self.context = None;
        self.results = FuzzyMatcherDaemonResults::default();
        self.selected = 0;
    }

    /// Poll the daemon for fresh results. Returns `true` if anything changed,
    /// so the caller knows to redraw.
    pub fn poll(&mut self) -> bool {
        if self.context.is_none() {
            return false;
        }
        let Some(daemon) = self.daemon.as_ref() else {
            return false;
        };
        let results = daemon.get();
        if Arc::ptr_eq(&results.topk, &self.results.topk) {
            return false;
        }
        if results.topk.is_empty() && !results.status.done {
            // Avoid flicker: skip empty intermediate results.
            return false;
        }
        if results.generation < self.min_generation {
            return false;
        }
        self.min_generation = results.generation;
        self.results = results;
        if !self.results.topk.is_empty() {
            self.selected = self.selected.min(self.results.topk.len() - 1);
        }
        true
    }

    pub fn move_selection(&mut self, delta: isize) {
        let len = self.results.topk.len();
        if len == 0 {
            return;
        }
        let max_idx = len - 1;
        let current = self.selected.min(max_idx);
        self.selected = (current as isize + delta).clamp(0, max_idx as isize) as usize;
    }

    /// Compute the replacement for accepting the selected entry. A directory
    /// is suffixed with `/`; a file gets a trailing space so typing can
    /// continue past it.
    pub fn accept(&self) -> Option<FileSearchReplacement> {
        let ctx = self.context.as_ref()?;
        let entry = self.results.topk.get(self.selected)?;
        let path = normalize_display_path(&entry.path.to_string()).to_owned();
        let range = ctx.path_range();
        let text = if entry.is_dir {
            format!("{path}/")
        } else {
            format!("{path} ")
        };
        Some(FileSearchReplacement { range, text })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_until_a_token_is_typed() {
        let mut state = FileSearchState::new(PathBuf::from("."));
        assert!(!state.is_visible());
        state.update("hello @sr", 9);
        assert!(state.context.is_some());
    }

    #[test]
    fn leaving_the_token_clears_results() {
        let mut state = FileSearchState::new(PathBuf::from("."));
        state.update("@foo", 4);
        assert!(state.context.is_some());
        state.update("@foo ", 5);
        assert!(state.context.is_none());
        assert!(!state.is_visible());
    }

    #[test]
    fn accept_appends_slash_for_a_directory_and_space_for_a_file() {
        let mut state = FileSearchState::new(PathBuf::from("."));
        state.context = Some(context::detect("@sr", 3).expect("context"));
        state.results = FuzzyMatcherDaemonResults {
            topk: Arc::from(vec![FuzzyMatchResult {
                path: nucleo::Utf32String::from("src"),
                is_dir: true,
                ..Default::default()
            }]),
            ..Default::default()
        };
        let replacement = state.accept().expect("replacement");
        assert_eq!(replacement.range, 1..3);
        assert_eq!(replacement.text, "src/");
    }
}
