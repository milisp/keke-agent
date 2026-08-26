//! The overlay a person chooses a model in.
//!
//! `/model <id>` still switches without opening anything — a name typed in
//! full is an instruction, not a question. The overlay is for the other case:
//! bare `/model`, where the person is asking what there is. Printing that list
//! into the transcript made the answer scroll away with the conversation and
//! left them retyping an id they had just read; a list that takes the keyboard
//! for as long as the question is open does not.

/// Which model is highlighted, and what has been typed to narrow the list.
#[derive(Debug, Default)]
pub struct ModelPicker {
    /// Substring typed to filter, matched against both id and display name.
    query: String,
    /// An index into the *filtered* list, clamped by [`ModelPicker::selected`]
    /// rather than reset, so one more letter does not throw the highlight back
    /// to the top of a list somebody is moving through.
    selected: usize,
}

impl ModelPicker {
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Which of `count` rows is highlighted, clamped to what is on screen.
    #[must_use]
    pub fn selected(&self, count: usize) -> usize {
        if count == 0 {
            0
        } else {
            self.selected.min(count - 1)
        }
    }

    pub fn move_selection(&mut self, delta: isize, count: usize) {
        if count == 0 {
            return;
        }
        let current = self.selected(count) as isize;
        let count = count as isize;
        self.selected = (current + delta).rem_euclid(count) as usize;
    }

    pub fn push(&mut self, ch: char) {
        self.query.push(ch);
    }

    pub fn backspace(&mut self) {
        self.query.pop();
    }

    /// Whether `model` survives the filter. Case-insensitive over both the id
    /// and the display name: a person reading "Grok 4.6" on screen must be
    /// able to type what they read, not the id it happens to have.
    #[must_use]
    pub fn matches(&self, model: &keke_provider_api::ModelInfo) -> bool {
        let query = self.query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        model.id.to_lowercase().contains(&query)
            || model.display_name.to_lowercase().contains(&query)
    }
}
