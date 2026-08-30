//! `@`-file-search sync and slash-command completion.

use super::App;

impl App {
    /// Recompute `@`-completion from the current line and cursor. Called
    /// after every edit that could have typed, or typed past, an `@`-token.
    pub(crate) fn sync_file_search(&mut self) {
        let cursor = self.input.cursor_byte();
        let line = self.input.current_line().to_string();
        self.file_search.update(&line, cursor);
    }

    /// Poll the fuzzy daemon; called on every timer tick. Returns whether the
    /// dropdown's contents changed, so the caller knows to redraw — though the
    /// event loop redraws every tick regardless, so the return value is
    /// informational only.
    pub(crate) fn tick_file_search(&mut self) -> bool {
        self.file_search.poll()
    }

    /// The completions for what is being typed, or nothing.
    ///
    /// Only while the name is still being typed: once there is a space, the
    /// person is writing arguments and a menu over their text is in the way.
    #[must_use]
    pub fn completions(&self) -> Vec<&crate::slash::SlashCommand> {
        if self.input.lines().len() > 1 {
            return Vec::new();
        }
        let text = self.input.text();
        let Some(prefix) = text.strip_prefix('/') else {
            return Vec::new();
        };
        if prefix.contains(char::is_whitespace) {
            return Vec::new();
        }
        self.commands.matching(prefix)
    }

    /// Which completion is highlighted, clamped to what is on screen.
    #[must_use]
    pub fn completion(&self) -> usize {
        let count = self.completions().len();
        if count == 0 {
            0
        } else {
            self.completion.min(count - 1)
        }
    }

    pub fn select_next_completion(&mut self) {
        let count = self.completions().len();
        if count > 0 {
            self.completion = (self.completion() + 1) % count;
        }
    }

    pub fn select_previous_completion(&mut self) {
        let count = self.completions().len();
        if count > 0 {
            self.completion = (self.completion() + count - 1) % count;
        }
    }

    /// Put the highlighted completion in the input box, ready for arguments.
    pub fn complete(&mut self) {
        let Some(name) = self
            .completions()
            .get(self.completion())
            .map(|entry| entry.name.clone())
        else {
            return;
        };
        self.input.clear();
        for ch in format!("/{name} ").chars() {
            self.input.insert_char(ch);
        }
        self.completion = 0;
    }
}
