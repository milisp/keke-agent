//! The overlay a person chooses a model or a provider in.
//!
//! `/model <id>` still switches without opening anything — a name typed in
//! full is an instruction, not a question. The overlay is for the other case:
//! bare `/model`, where the person is asking what there is. Printing that list
//! into the transcript made the answer scroll away with the conversation and
//! left them retyping an id they had just read; a list that takes the keyboard
//! for as long as the question is open does not.
//!
//! `/provider` asks the same question about a different list, so it gets the
//! same overlay rather than a second one: filtering, clamping, and what enter
//! and esc mean are answers about the *question*, not about models, and two
//! copies of them would be two chances to answer differently.

/// Something the overlay can list.
///
/// Two names, because a person reading "Grok 4.6" or "xAI (API key)" on screen
/// must be able to type what they read, while what actually gets switched to is
/// the key — the model id, the provider route.
pub trait Choice {
    fn key(&self) -> &str;
    fn label(&self) -> &str;
}

impl Choice for keke_provider_api::ModelInfo {
    fn key(&self) -> &str {
        &self.id
    }

    fn label(&self) -> &str {
        &self.display_name
    }
}

/// One provider instance a session could be pointed at.
///
/// A route rather than a vendor: `[providers.grok]` and `[providers.xai]` can
/// both be the same vendor reached two different ways, and choosing between
/// them is exactly what this list is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderChoice {
    /// The registry key, which is what config.toml and `/provider <name>` say.
    pub route: String,
    pub display_name: String,
}

impl Choice for ProviderChoice {
    fn key(&self) -> &str {
        &self.route
    }

    fn label(&self) -> &str {
        &self.display_name
    }
}

/// Which list the open overlay is showing. Held on the picker rather than
/// beside it so there is no way to be open on one list and reading the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerKind {
    Model,
    Provider,
}

/// Which row is highlighted, and what has been typed to narrow the list.
#[derive(Debug)]
pub struct Picker {
    kind: PickerKind,
    /// Substring typed to filter, matched against both key and label.
    query: String,
    /// An index into the *filtered* list, clamped by [`Picker::selected`]
    /// rather than reset, so one more letter does not throw the highlight back
    /// to the top of a list somebody is moving through.
    selected: usize,
}

impl Picker {
    #[must_use]
    pub fn new(kind: PickerKind) -> Self {
        Self {
            kind,
            query: String::new(),
            selected: 0,
        }
    }

    #[must_use]
    pub fn kind(&self) -> PickerKind {
        self.kind
    }

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

    /// Whether `choice` survives the filter. Case-insensitive over both the key
    /// and the label: a person reading "Grok 4.6" on screen must be able to
    /// type what they read, not the id it happens to have.
    #[must_use]
    pub fn matches<C: Choice + ?Sized>(&self, choice: &C) -> bool {
        let query = self.query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        choice.key().to_lowercase().contains(&query)
            || choice.label().to_lowercase().contains(&query)
    }
}
