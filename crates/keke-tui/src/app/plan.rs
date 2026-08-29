//! Reviewing the plan the agent proposes.
//!
//! Leaving plan mode is not a mode switch the surface performs: the agent asks
//! to leave by calling `exit_plan_mode`, and that arrives as an ordinary
//! permission request. So there is no second protocol here — the review is a
//! different way of *drawing* one prompt, and every answer it offers is one of
//! the three answers a permission already has.

use keke_acp::PermissionAnswer;
use keke_protocol::ToolCall;

use super::App;

/// The tool the agent calls to ask out of plan mode. Recognized by name
/// because that is all the seam carries: a surface across a pipe sees a tool
/// call, not the engine's idea of what plan mode is.
pub(crate) const EXIT_PLAN_MODE: &str = "exit_plan_mode";

/// The plan awaiting a person's answer, and where they have scrolled to in it.
pub struct PlanReview {
    /// The plan as the agent wrote it. Empty when it proposed none, which is a
    /// state of its own rather than an error: a person can still approve, ask
    /// for changes, or drop the plan.
    text: String,
    scroll: usize,
}

impl PlanReview {
    /// The plan text, empty when the agent wrote none.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether there is a plan to read at all, so the surface can say so
    /// instead of drawing an empty box.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    #[must_use]
    pub fn scroll(&self) -> usize {
        self.scroll
    }
}

/// The plan out of an `exit_plan_mode` call's arguments.
///
/// Several spellings are accepted because the argument name belongs to
/// whichever tool pack ships the tool, and a surface that recognized only one
/// of them would draw an empty review for a plan that was right there.
pub(crate) fn plan_text(call: &ToolCall) -> String {
    if let Some(text) = call.arguments.as_str() {
        return text.to_string();
    }
    for field in ["plan", "plan_text", "content", "text"] {
        if let Some(text) = call.arguments.get(field).and_then(|value| value.as_str()) {
            return text.to_string();
        }
    }
    String::new()
}

impl App {
    /// The plan review, while one is open. `None` is the ordinary state.
    #[must_use]
    pub fn plan_review(&self) -> Option<&PlanReview> {
        self.plan.as_ref()
    }

    pub(super) fn open_plan_review(&mut self, call: &ToolCall) {
        self.plan = Some(PlanReview {
            text: plan_text(call),
            scroll: 0,
        });
    }

    /// Approve the plan: the agent may start building.
    ///
    /// The mode is not cleared here. Plan mode ends when the agent says it has
    /// ended, over [`keke_acp::Update::ModeChanged`] — a surface that turned
    /// its own flag off on approval would be drawing an outcome it had only
    /// asked for.
    pub fn approve_plan(&mut self) {
        self.answer_permission(PermissionAnswer::Allow);
        self.plan = None;
    }

    /// Send the agent back to planning.
    ///
    /// The denial is the whole answer the agent gets, so the review closes and
    /// the composer takes the keyboard back: the revision notes are an ordinary
    /// prompt, typed where every other prompt is typed.
    pub fn request_plan_changes(&mut self) {
        self.answer_permission(PermissionAnswer::Deny);
        self.plan = None;
        self.set_flash("plan sent back — type what should change");
    }

    /// Abandon the plan and leave plan mode.
    ///
    /// Unlike requesting changes, this asks for the mode itself to end: a
    /// person who is done planning wants the next prompt answered, not planned.
    pub fn quit_plan(&mut self) {
        self.answer_permission(PermissionAnswer::Deny);
        self.plan = None;
        self.request_session_mode(keke_config_types::SessionMode::Default);
    }

    /// Put the plan on the clipboard, or say there is nothing to put there.
    pub fn copy_plan(&mut self) {
        let Some(review) = &self.plan else {
            return;
        };
        if review.is_empty() {
            self.set_flash("no plan to copy");
            return;
        }
        let text = review.text.clone();
        self.copy(text);
    }

    /// Move within the plan. Clamped by the frame that draws it, which is the
    /// only thing that knows how tall the panel came out.
    pub fn scroll_plan(&mut self, delta: isize) {
        if let Some(review) = &mut self.plan {
            review.scroll = review.scroll.saturating_add_signed(delta);
        }
    }

    /// Told by `draw` how far down the plan can go, so a held-down arrow key
    /// cannot scroll past the end and leave a blank panel behind.
    pub(crate) fn clamp_plan_scroll(&mut self, max: usize) {
        if let Some(review) = &mut self.plan {
            review.scroll = review.scroll.min(max);
        }
    }
}
