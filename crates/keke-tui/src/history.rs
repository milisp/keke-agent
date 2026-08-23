//! Bringing back what was typed before.
//!
//! The entries come from the host — the interface has no idea where a home
//! directory is — and go back out through [`PromptRecorder`], so the surface
//! stays written against seams rather than against the engine.

use std::sync::Arc;

/// Where a submitted prompt goes to be remembered across runs.
///
/// Recording is fire-and-forget on purpose: a history file that cannot be
/// written must not fail the turn a person just started.
pub trait PromptRecorder: Send + Sync {
    fn record(&self, prompt: &str);
}

/// Past prompts and where the arrow keys are within them.
///
/// `cursor` is `None` when the person is typing rather than browsing. The text
/// they had in the box when they started browsing is kept as `draft`, so
/// arrowing past the newest entry gives it back instead of losing it.
#[derive(Default)]
pub struct PromptHistory {
    entries: Vec<String>,
    recorder: Option<Arc<dyn PromptRecorder>>,
    cursor: Option<usize>,
    draft: String,
}

impl PromptHistory {
    /// `entries` is oldest first, the order [`keke_core::PromptHistory::load`]
    /// hands back.
    #[must_use]
    pub fn new(entries: Vec<String>) -> Self {
        Self {
            entries,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_recorder(mut self, recorder: Arc<dyn PromptRecorder>) -> Self {
        self.recorder = Some(recorder);
        self
    }

    #[must_use]
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Whether the arrow keys are currently walking the history.
    #[must_use]
    pub fn is_browsing(&self) -> bool {
        self.cursor.is_some()
    }

    /// Remember a submitted prompt, and stop browsing.
    ///
    /// Slash commands are remembered too: `/model gpt-5` is as much a thing
    /// somebody wants back as a sentence is.
    pub fn submit(&mut self, prompt: &str) {
        self.cursor = None;
        self.draft.clear();
        if prompt.trim().is_empty() {
            return;
        }
        if let Some(recorder) = &self.recorder {
            recorder.record(prompt);
        }
        if self.entries.last().map(String::as_str) != Some(prompt) {
            self.entries.push(prompt.to_string());
        }
    }

    /// Step one prompt further back, or `None` at the oldest one.
    ///
    /// `current` is what is in the box; it is kept only on the first step, so
    /// walking back through five prompts still returns the person's own draft
    /// when they walk forward again.
    pub fn older(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let at = match self.cursor {
            None => {
                self.draft = current.to_string();
                self.entries.len() - 1
            }
            Some(0) => return None,
            Some(at) => at - 1,
        };
        self.cursor = Some(at);
        self.entries.get(at).cloned()
    }

    /// Step one prompt forward, ending at the draft that was interrupted.
    pub fn newer(&mut self) -> Option<String> {
        let at = self.cursor?;
        if at + 1 < self.entries.len() {
            self.cursor = Some(at + 1);
            return self.entries.get(at + 1).cloned();
        }
        self.cursor = None;
        Some(std::mem::take(&mut self.draft))
    }

    /// Leave browsing where it is; the box is the person's again.
    pub fn stop_browsing(&mut self) {
        self.cursor = None;
        self.draft.clear();
    }
}
