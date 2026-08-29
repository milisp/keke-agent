//! Subagent rows, expandable-cell toggles, and clipboard actions.
//!
//! Split out of `mod.rs` because these three things share one trait: each
//! answers "what did this frame draw where" so a later click or keypress
//! knows what it hit.

use std::time::Instant;

use crate::transcript::Cell;

use super::App;

impl App {
    /// The subagents to draw, oldest first.
    #[must_use]
    pub fn subagents(&self) -> &[keke_acp::SubagentView] {
        &self.subagents
    }

    /// Fold in a snapshot, keeping the start times of the agents that survive.
    pub(crate) fn set_subagents(&mut self, rows: Vec<keke_acp::SubagentView>) {
        let now = Instant::now();
        for row in &rows {
            self.subagent_since.entry(row.id.clone()).or_insert(now);
        }
        // An agent that left the snapshot has been collected: its result is in
        // the transcript now, so the row, its clock, and any popup opened on it
        // all go together.
        self.subagent_since
            .retain(|id, _| rows.iter().any(|row| &row.id == id));
        if let Some(open) = &self.subagent_detail
            && !rows.iter().any(|row| &row.id == open)
        {
            self.subagent_detail = None;
        }
        self.subagents = rows;
    }

    /// How long a subagent has been on screen.
    #[must_use]
    pub fn subagent_elapsed(&self, id: &str) -> Option<std::time::Duration> {
        self.subagent_since.get(id).map(Instant::elapsed)
    }

    /// Told by `draw` which rows this frame's subagents landed on.
    pub(crate) fn set_subagent_rows(&mut self, rows: Vec<(u16, String)>) {
        self.subagent_rows = rows;
    }

    /// The subagent whose task is open in full, if one is.
    #[must_use]
    pub fn open_subagent(&self) -> Option<&keke_acp::SubagentView> {
        let open = self.subagent_detail.as_ref()?;
        self.subagents.iter().find(|row| &row.id == open)
    }

    /// Open the subagent drawn at `row`, or close it if it is already open.
    ///
    /// Reported so the caller knows the click was spent here and must not also
    /// be read as a click on the transcript underneath.
    pub fn open_subagent_at(&mut self, row: u16) -> bool {
        let Some((_, id)) = self.subagent_rows.iter().find(|(at, _)| *at == row) else {
            return false;
        };
        let id = id.clone();
        self.subagent_detail = if self.subagent_detail.as_ref() == Some(&id) {
            None
        } else {
            Some(id)
        };
        true
    }

    /// Close the subagent popup, reporting whether one was open — so escape can
    /// fall through to whatever it means when none is.
    pub fn close_subagent(&mut self) -> bool {
        self.subagent_detail.take().is_some()
    }

    /// Told by `draw` which rows this frame's expandable headers landed on.
    pub(crate) fn set_toggles(&mut self, toggles: Vec<(u16, usize)>) {
        self.toggles = toggles;
    }

    /// Open or close the header drawn at `row`, if a click landed on one.
    ///
    /// The whole row answers, not just the marker: a one-cell target is a
    /// thing people miss, and there is nothing else on that row to hit.
    pub fn toggle_at(&mut self, row: u16) -> bool {
        let Some((_, key)) = self.toggles.iter().find(|(at, _)| *at == row).copied() else {
            return false;
        };
        self.toggle_expanded(key);
        true
    }

    /// Open or close the last thing that can be opened.
    ///
    /// The keyboard's answer to the click: what a person wants right after a
    /// run of calls scrolls past is that run, not one chosen from a list.
    pub fn toggle_last_expandable(&mut self) {
        let Some(key) = self.transcript.last_expandable() else {
            self.set_flash("nothing to expand");
            return;
        };
        self.toggle_expanded(key);
    }

    fn toggle_expanded(&mut self, key: usize) {
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
    }

    /// Whether a click at these coordinates hit the jump-to-bottom button.
    pub fn hit_follow_button(&self, column: u16, row: u16) -> bool {
        self.follow_button.is_some_and(|(x, y, width)| {
            row == y && column >= x && column < x.saturating_add(width)
        })
    }

    ///
    /// The transcript has no cursor, so there is nothing else it could mean:
    /// what a person reaches for after reading an answer is that answer.
    pub fn copy_last_reply(&mut self) {
        let reply = self
            .transcript
            .cells()
            .iter()
            .rev()
            .find_map(|cell| match cell {
                Cell::Assistant(text) => Some(text.clone()),
                _ => None,
            });
        match reply {
            Some(text) if !text.trim().is_empty() => {
                self.copy(text);
            }
            _ => self.set_flash("nothing to copy yet"),
        }
    }

    /// Put `text` on the clipboard and say so.
    pub(super) fn copy(&mut self, text: String) {
        let lines = text.lines().count();
        self.set_flash(format!("copied {lines} lines"));
        self.pending_copy = Some(text);
    }

    /// Taken by the event loop, which owns the terminal this has to reach.
    /// Put a dragged selection on the clipboard.
    pub(crate) fn copy_selection(&mut self, text: String) {
        let lines = text.lines().count();
        self.pending_copy = Some(text);
        self.set_flash(if lines == 1 {
            "copied the selection".to_string()
        } else {
            format!("copied {lines} lines")
        });
    }

    pub fn take_pending_copy(&mut self) -> Option<String> {
        self.pending_copy.take()
    }
}
