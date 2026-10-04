//! Subagent rows, expandable-cell toggles, and clipboard actions.
//!
//! Split out of `mod.rs` because these three things share one trait: each
//! answers "what did this frame draw where" so a later click or keypress
//! knows what it hit.

use std::time::Instant;

use crate::transcript::Cell;

use super::App;

#[derive(Default)]
pub(crate) struct Recording {
    pub error: Option<String>,
    pub scroll: crate::scroll::Scrollback,
    pub transcript: crate::transcript::Transcript,
    pub expanded: std::collections::HashSet<usize>,
    pub compact: bool,
    loaded: bool,
    pending:
        Option<tokio::sync::oneshot::Receiver<Result<Vec<keke_protocol::SessionEvent>, String>>>,
    refreshed: Option<Instant>,
}

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
        let completed_while_open = self.open_subagent().is_some_and(|agent| {
            agent.status.is_none()
                && rows
                    .iter()
                    .any(|row| row.id == agent.id && row.status.is_some())
        });
        if completed_while_open {
            self.close_subagent();
        }
        // A replacement parent can retire all of its children at once; the
        // old clocks and inspector must leave with those rows.
        self.subagent_since
            .retain(|id, _| rows.iter().any(|row| &row.id == id));
        if let Some(open) = &self.subagent_detail
            && !rows.iter().any(|row| &row.id == open)
        {
            self.close_subagent();
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

    /// The subagent whose recorded transcript is open, if one is.
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
        if self.subagent_detail.as_ref() == Some(&id) {
            self.close_subagent();
        } else {
            self.show_subagent(id);
        }
        true
    }

    /// Close the subagent transcript, reporting whether one was open — so escape can
    /// fall through to whatever it means when none is.
    pub fn close_subagent(&mut self) -> bool {
        let open = self.subagent_detail.take().is_some();
        if open {
            self.subagent_recording = Recording::default();
            self.selection.clear();
        }
        open
    }

    fn show_subagent(&mut self, id: String) {
        self.subagent_history = None;
        self.subagent_detail = Some(id);
        self.subagent_recording = Recording::default();
        self.selection.clear();
        self.tick_subagent_recording();
    }

    /// Open the first child, or cycle through all children while inspecting one.
    pub(crate) fn cycle_subagent(&mut self) {
        let candidates: Vec<_> = self
            .subagents
            .iter()
            .filter(|agent| agent.status.is_none())
            .map(|agent| agent.id.clone())
            .collect();
        if candidates.is_empty() {
            self.set_flash("no running subagents — /subagents opens history");
            return;
        }
        let next = self
            .subagent_detail
            .as_ref()
            .and_then(|id| candidates.iter().position(|agent| agent == id))
            .map_or(0, |index| (index + 1) % candidates.len());
        self.show_subagent(candidates[next].clone());
    }

    pub(crate) fn subagents_command(&mut self, arguments: &str) {
        let id = arguments.trim();
        if !id.is_empty() {
            if self.subagents.iter().any(|agent| agent.id == id) {
                self.show_subagent(id.to_string());
            } else {
                self.set_flash(format!("unknown subagent: {id}"));
            }
        } else if self.subagents.is_empty() {
            self.set_flash("no subagents recorded");
        } else {
            self.subagent_history = Some(0);
        }
    }

    pub(crate) fn move_subagent_history(&mut self, delta: isize) {
        if let Some(index) = &mut self.subagent_history {
            *index = index
                .saturating_add_signed(delta)
                .min(self.subagents.len().saturating_sub(1));
        }
    }

    pub(crate) fn select_subagent_history(&mut self) {
        if let Some(index) = self.subagent_history
            && let Some(agent) = self.subagents.get(index)
        {
            self.show_subagent(agent.id.clone());
        }
    }

    pub(crate) fn subagent_needs_poll(&self) -> bool {
        self.open_subagent()
            .is_some_and(|agent| agent.status.is_none() || !self.subagent_recording.loaded)
    }

    /// Read the durable log off the UI thread and refresh while it is open.
    pub(crate) fn tick_subagent_recording(&mut self) {
        let Some(id) = self.subagent_detail.clone() else {
            return;
        };
        if let Some(pending) = &mut self.subagent_recording.pending {
            match pending.try_recv() {
                Ok(result) => {
                    match result {
                        Ok(events) => {
                            self.subagent_recording.transcript =
                                crate::transcript::Transcript::default();
                            self.subagent_recording.transcript.replay_recorded(&events);
                            self.subagent_recording.error = None;
                        }
                        Err(error) => self.subagent_recording.error = Some(error),
                    }
                    self.subagent_recording.pending = None;
                    self.subagent_recording.loaded = true;
                    self.subagent_recording.refreshed = Some(Instant::now());
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return,
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    self.subagent_recording.pending = None;
                    self.subagent_recording.error = Some("transcript reader stopped".to_string());
                    self.subagent_recording.refreshed = Some(Instant::now());
                }
            }
        }
        let status = self.open_subagent().and_then(|agent| agent.status.clone());
        if self.subagent_recording.loaded && status.is_some() {
            return;
        }
        if self
            .subagent_recording
            .refreshed
            .is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(1))
        {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let conversation = std::sync::Arc::clone(&self.conversation);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.subagent_recording.pending = Some(receiver);
        runtime.spawn(async move {
            let result = conversation
                .subagent_transcript(id)
                .await
                .map_err(|error| error.to_string());
            let _ = sender.send(result);
        });
    }

    pub(crate) fn visible_transcript(&self) -> &crate::transcript::Transcript {
        if self.open_subagent().is_some() {
            &self.subagent_recording.transcript
        } else {
            &self.transcript
        }
    }

    pub(crate) fn visible_scroll(&self) -> &crate::scroll::Scrollback {
        if self.open_subagent().is_some() {
            &self.subagent_recording.scroll
        } else {
            &self.scroll
        }
    }

    pub(crate) fn visible_scroll_mut(&mut self) -> &mut crate::scroll::Scrollback {
        if self.open_subagent().is_some() {
            &mut self.subagent_recording.scroll
        } else {
            &mut self.scroll
        }
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
        let Some(key) = self.visible_transcript().last_expandable() else {
            self.set_flash("nothing to expand");
            return;
        };
        self.toggle_expanded(key);
    }

    fn toggle_expanded(&mut self, key: usize) {
        let expanded = if self.open_subagent().is_some() {
            &mut self.subagent_recording.expanded
        } else {
            &mut self.expanded
        };
        if !expanded.remove(&key) {
            expanded.insert(key);
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
            .visible_transcript()
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
