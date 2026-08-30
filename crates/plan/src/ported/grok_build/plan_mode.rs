//! Ported from grok-build `crates/codegen/xai-grok-shell/src/session/plan_mode.rs`
//! (Apache-2.0). See `THIRD_PARTY_NOTICES.md`.
//!
//! What was kept: the four-state lifecycle, the withdrawal rollback, the
//! full/sparse reminder alternation, and the reminder prose.
//!
//! What was dropped and why: the `PlanModeSnapshot` persistence (keke has no
//! plan-mode side file — the session log is the record, and a resumed session
//! reconciles from `SessionModeSwitch` at its first turn boundary instead), the
//! `PromptMode` mirror (`keke_config_types::SessionMode` is the one coarse
//! value here), and the MiniJinja templates (keke renders these with `format!`
//! rather than taking a template engine into a plugin).
//!
//! What was moved out: the plan file path. Upstream stores it in the tracker;
//! here it is resolved from the session id, which the tracker never sees, so it
//! lives on the `PlanMode` wrapper instead.

/// Where a session is in the plan-mode lifecycle.
///
/// Finer than [`keke_config_types::SessionMode`] on purpose: the coarse bit
/// answers "is this session planning?", these states answer "and has the model
/// been told yet?", which is what a rollback needs to know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanModeState {
    /// Normal operation. No plan-mode constraints.
    Inactive,
    /// Turned on, but no turn has started since — the model has not been told,
    /// and no reminder has been injected.
    Pending,
    /// The model has been told. Edits outside the plan file are refused.
    Active,
    /// Turned off while a turn was in flight; the exit lands at the boundary.
    ExitPending,
}

/// A buffered mid-turn activation, plus what a withdrawal has to restore.
#[derive(Debug)]
struct PendingActivation {
    /// `was_previously_active` as it was before this activation. Restored on
    /// withdrawal so a rolled-back activation does not fake a reentry.
    prior_was_previously_active: bool,
}

/// The plan-mode lifecycle, as a pure state machine.
///
/// Deliberately knows nothing about sessions, tools, or I/O: every transition
/// is a total function of the state, which is what lets the prose above be
/// asserted directly in tests.
#[derive(Debug)]
pub struct PlanModeTracker {
    state: PlanModeState,
    /// Whether plan mode was active earlier in this session, so a second entry
    /// gets the shorter reentry reminder instead of the full one.
    was_previously_active: bool,
    /// Even count means the full reminder, odd the sparse one. Reset whenever
    /// the model loses the context the full one established.
    reminder_count: u32,
    /// Inject the "you have left plan mode" reminder on the next turn. Set only
    /// where the model has no in-context exit signal — a person's toggle-off,
    /// never an approved `exit_plan_mode`, whose tool result says so itself.
    pending_exit_reminder: bool,
    pending_activation: Option<PendingActivation>,
}

impl Default for PlanModeTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl PlanModeTracker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: PlanModeState::Inactive,
            was_previously_active: false,
            reminder_count: 0,
            pending_exit_reminder: false,
            pending_activation: None,
        }
    }

    #[must_use]
    pub fn state(&self) -> PlanModeState {
        self.state
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        self.state == PlanModeState::Active
    }

    /// Whether the next reminder should be the full variant.
    #[must_use]
    pub fn should_use_full_reminder(&self) -> bool {
        self.reminder_count.is_multiple_of(2)
    }

    #[must_use]
    pub fn has_pending_exit_reminder(&self) -> bool {
        self.pending_exit_reminder
    }

    /// Whether this entry is a return to plan mode within the same session.
    #[must_use]
    pub fn is_reentry(&self) -> bool {
        self.was_previously_active && self.state == PlanModeState::Pending
    }

    #[must_use]
    pub fn has_pending_activation(&self) -> bool {
        self.pending_activation.is_some()
    }

    /// Plan mode was turned on. Returns whether the state actually changed.
    ///
    /// From `ExitPending` this goes straight back to `Active`: the model still
    /// has plan-mode context, so cancelling the deferred exit is the whole
    /// transition — re-announcing it would tell the model something it knows.
    pub fn enter_pending(&mut self) -> bool {
        match self.state {
            PlanModeState::Inactive => {
                self.state = PlanModeState::Pending;
                self.pending_exit_reminder = false;
                true
            }
            PlanModeState::ExitPending => {
                self.state = PlanModeState::Active;
                self.pending_exit_reminder = false;
                true
            }
            _ => false,
        }
    }

    /// A turn is starting while `Pending` — the model is about to be told.
    pub fn activate(&mut self) -> bool {
        if self.state != PlanModeState::Pending {
            return false;
        }
        self.state = PlanModeState::Active;
        self.was_previously_active = true;
        self.reminder_count = 0;
        true
    }

    /// Activate now, mid-turn, with the reminder still undelivered.
    ///
    /// While an activation is buffered the model has *not* seen plan mode, so a
    /// toggle-off withdraws it ([`Self::user_exit`]) rather than deferring an
    /// exit from something the model never knew about.
    pub fn activate_mid_turn(&mut self) -> bool {
        if self.state != PlanModeState::Pending {
            return false;
        }
        let prior_was_previously_active = self.was_previously_active;
        self.state = PlanModeState::Active;
        self.was_previously_active = true;
        self.reminder_count = 0;
        self.pending_activation = Some(PendingActivation {
            prior_was_previously_active,
        });
        true
    }

    /// The buffered activation has been delivered.
    pub fn take_pending_activation(&mut self) -> bool {
        self.pending_activation.take().is_some()
    }

    /// The model called `enter_plan_mode` and it was approved.
    pub fn activate_from_tool(&mut self) -> bool {
        if self.state != PlanModeState::Inactive {
            return false;
        }
        self.state = PlanModeState::Active;
        self.was_previously_active = true;
        self.reminder_count = 0;
        self.pending_exit_reminder = false;
        true
    }

    /// `exit_plan_mode` was approved.
    ///
    /// Does not arm the exit reminder: the tool result the model reads already
    /// says plan mode is over, and a reminder armed here would drain a turn
    /// later, restating something stale.
    pub fn deactivate_approved(&mut self) -> bool {
        if self.state != PlanModeState::Active {
            return false;
        }
        self.state = PlanModeState::Inactive;
        self.reminder_count = 0;
        self.pending_activation = None;
        true
    }

    /// A person turned plan mode off. `turn_in_flight` defers the exit to the
    /// turn boundary, because leaving mid-step would change the rules under a
    /// model that is already reasoning about them.
    pub fn user_exit(&mut self, turn_in_flight: bool) {
        if let Some(pending) = self.pending_activation.take()
            && self.state == PlanModeState::Active
        {
            // Withdrawn before delivery: roll the activation back rather than
            // announce an exit from a mode the model was never told about.
            self.state = PlanModeState::Inactive;
            self.was_previously_active = pending.prior_was_previously_active;
            return;
        }
        match self.state {
            PlanModeState::Pending => self.state = PlanModeState::Inactive,
            PlanModeState::Active => {
                if turn_in_flight {
                    self.state = PlanModeState::ExitPending;
                } else {
                    self.state = PlanModeState::Inactive;
                    self.pending_exit_reminder = true;
                }
            }
            _ => {}
        }
    }

    /// The turn that was holding up a deferred exit has finished.
    pub fn complete_deferred_exit(&mut self) {
        if self.state != PlanModeState::ExitPending {
            return;
        }
        self.state = PlanModeState::Inactive;
        self.pending_exit_reminder = true;
    }

    /// A reminder was injected; advance the full/sparse alternation.
    pub fn record_reminder_injected(&mut self) {
        self.reminder_count += 1;
    }

    pub fn clear_pending_exit_reminder(&mut self) {
        self.pending_exit_reminder = false;
    }

    /// History was compacted, so the full reminder's context is gone with it.
    pub fn reset_after_compaction(&mut self) {
        if self.state == PlanModeState::Active {
            self.reminder_count = 0;
            self.pending_activation = None;
        }
    }
}

/// The full reminder: the plan-file rules plus how the turn is expected to end.
#[must_use]
pub(crate) fn full_reminder(plan_path: &str, plan_has_content: bool) -> String {
    let plan_file = if plan_has_content {
        format!(
            "A plan file exists at {plan_path}. You can read it and revise it with the \
             `write_file` tool."
        )
    } else {
        format!("No plan written yet. Write your plan to {plan_path} using the `write_file` tool.")
    };
    format!(
        "Plan mode is active. Do not make any edits or writes to the system.\n\n\
         ## Plan File:\n{plan_file}\n\n\
         You should build your plan by writing to or editing this file. Note that this is the \
         only file you are allowed to edit.\n\n\
         Your turn should only end with either a question to the user to clarify requirements or \
         `exit_plan_mode` to present your plan to the user."
    )
}

/// The sparse reminder, for the alternating turns. No paths and no tool names:
/// it exists to keep the constraint in view for the price of one line.
#[must_use]
pub(crate) fn sparse_reminder() -> &'static str {
    "Plan mode is still active. Do not make any edits or writes to the system except for the \
     plan file."
}

/// Injected when plan mode is entered for the second or later time in a
/// session, where the full rules are already in the transcript.
#[must_use]
pub(crate) fn reentry_reminder(plan_path: &str) -> String {
    format!(
        "## Returning to Plan Mode\n\n\
         You are entering plan mode again after having previously exited it. A plan file exists \
         at {plan_path} from your previous planning session.\n\n\
         Your turn should only end with either a question to the user to clarify requirements or \
         `exit_plan_mode` to present your plan to the user."
    )
}

/// Injected once after a person turned plan mode off, where no tool result told
/// the model that the rules changed.
#[must_use]
pub(crate) fn exit_reminder() -> &'static str {
    "You have exited plan mode. You can now make edits, run tools, and take actions."
}

/// What the model is told when it tries to edit anything but the plan file.
///
/// Names the one editable path, because a refusal that does not say what *is*
/// allowed reads as a broken tool and gets retried.
#[must_use]
pub(crate) fn edit_rejected(plan_path: &str) -> String {
    format!(
        "file edits are not allowed in plan mode - the only editable file is the plan file \
         ({plan_path})."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_person_toggling_on_then_prompting_activates_plan_mode() {
        let mut tracker = PlanModeTracker::new();
        assert_eq!(tracker.state(), PlanModeState::Inactive);
        assert!(tracker.enter_pending());
        assert_eq!(tracker.state(), PlanModeState::Pending);
        assert!(tracker.activate());
        assert!(tracker.is_active());
        assert!(tracker.deactivate_approved());
        assert_eq!(tracker.state(), PlanModeState::Inactive);
    }

    #[test]
    fn a_withdrawn_pending_activation_rolls_back_without_faking_a_reentry() {
        let mut tracker = PlanModeTracker::new();
        tracker.enter_pending();
        assert!(tracker.activate_mid_turn());
        assert!(tracker.has_pending_activation());

        tracker.user_exit(true);

        assert_eq!(tracker.state(), PlanModeState::Inactive);
        assert!(!tracker.has_pending_activation());
        // The model was never told, so nothing needs unsaying...
        assert!(!tracker.has_pending_exit_reminder());
        // ...and the next entry is a first entry, not a return.
        tracker.enter_pending();
        assert!(!tracker.is_reentry());
    }

    #[test]
    fn toggling_off_mid_turn_defers_the_exit_to_the_boundary() {
        let mut tracker = PlanModeTracker::new();
        tracker.enter_pending();
        tracker.activate();
        tracker.user_exit(true);
        assert_eq!(tracker.state(), PlanModeState::ExitPending);
        tracker.complete_deferred_exit();
        assert_eq!(tracker.state(), PlanModeState::Inactive);
        assert!(tracker.has_pending_exit_reminder());
    }

    #[test]
    fn toggling_back_on_during_a_deferred_exit_returns_to_active_silently() {
        let mut tracker = PlanModeTracker::new();
        tracker.enter_pending();
        tracker.activate();
        tracker.user_exit(true);
        assert!(tracker.enter_pending());
        assert!(tracker.is_active());
        assert!(!tracker.has_pending_exit_reminder());
    }

    #[test]
    fn an_approved_exit_leaves_no_reminder_because_the_tool_result_said_it() {
        let mut tracker = PlanModeTracker::new();
        tracker.activate_from_tool();
        assert!(tracker.deactivate_approved());
        assert!(!tracker.has_pending_exit_reminder());
    }

    #[test]
    fn reminders_alternate_full_and_sparse_and_compaction_restarts_them() {
        let mut tracker = PlanModeTracker::new();
        tracker.activate_from_tool();
        assert!(tracker.should_use_full_reminder());
        tracker.record_reminder_injected();
        assert!(!tracker.should_use_full_reminder());
        tracker.record_reminder_injected();
        assert!(tracker.should_use_full_reminder());
        tracker.record_reminder_injected();
        tracker.reset_after_compaction();
        assert!(tracker.should_use_full_reminder());
    }

    #[test]
    fn a_second_entry_in_one_session_is_a_reentry() {
        let mut tracker = PlanModeTracker::new();
        tracker.enter_pending();
        tracker.activate();
        tracker.deactivate_approved();
        tracker.enter_pending();
        assert!(tracker.is_reentry());
    }

    #[test]
    fn a_refusal_names_the_only_editable_path() {
        assert!(edit_rejected("/s/plan.md").contains("/s/plan.md"));
    }
}
