use keke_acp::PermissionAnswer;
use keke_config_types::ApprovalPolicy;
use keke_protocol::ToolCall;

use super::App;
use crate::transcript::Cell;

/// The tool the agent calls to ask out of plan mode. Recognized by name
/// because that is all the seam carries: a surface across a pipe sees a tool
/// call, not the engine's idea of what plan mode is.
pub(crate) const EXIT_PLAN_MODE: &str = "exit_plan_mode";

/// Which half of the review the keyboard is in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlanFocus {
    /// Reading the plan and picking a row with the arrow keys.
    #[default]
    Preview,
    /// Typing revision notes. No letter answers the plan while this has
    /// focus, because a person writing "say more here" must not have `y` and
    /// the arrow keys fire under them.
    Composer,
}

/// The rows on the panel under the plan, in the order they are shown. Picking
/// one and pressing Enter is the whole interaction — there is no separate
/// policy submenu and no per-line comment to attach first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanRow {
    /// Carry the plan out without asking again.
    AutoMode,
    /// Carry the plan out, asking before each command.
    ManualApprove,
    /// Send the agent back to planning with what gets typed next.
    TellKekeWhatToChange,
}

/// Every row, in display order.
pub(crate) const ROWS: [PlanRow; 3] = [
    PlanRow::AutoMode,
    PlanRow::ManualApprove,
    PlanRow::TellKekeWhatToChange,
];

impl PlanRow {
    /// What the row says on the panel.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            PlanRow::AutoMode => "Yes, and use auto mode",
            PlanRow::ManualApprove => "Yes, manually approve edits",
            PlanRow::TellKekeWhatToChange => "Tell Keke what to change",
        }
    }

    /// The policy an approving row carries the plan out under. `None` for the
    /// row that sends the plan back instead of approving it.
    #[must_use]
    fn policy(self) -> Option<ApprovalPolicy> {
        match self {
            PlanRow::AutoMode => Some(ApprovalPolicy::Never),
            PlanRow::ManualApprove => Some(ApprovalPolicy::OnRequest),
            PlanRow::TellKekeWhatToChange => None,
        }
    }
}

/// What is being done to the plan in the scrollback while it waits.
///
/// The plan's text is here too, since the feedback quotes it and reading it
/// back out of the transcript to build a quote would be the same string with
/// one more way to be wrong.
#[derive(Clone)]
pub struct PlanReview {
    /// The plan as the agent wrote it. Empty when it proposed none, which is a
    /// state of its own rather than an error: a person can still approve, ask
    /// for changes, or drop the plan.
    text: String,
    /// The row the arrow keys have landed on.
    row: PlanRow,
    focus: PlanFocus,
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

    /// The row the arrow keys have landed on.
    #[must_use]
    pub fn row(&self) -> PlanRow {
        self.row
    }

    #[must_use]
    pub fn focus(&self) -> PlanFocus {
        self.focus
    }

    /// Freeform revision notes, as the text the agent will read — `None` when
    /// the person said nothing, so a bare denial stays a bare denial.
    #[must_use]
    fn feedback(freeform: Option<&str>) -> Option<String> {
        let text = freeform?.trim();
        (!text.is_empty()).then(|| text.to_string())
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

/// The plan's own title, as a file name: the first markdown heading, or the
/// first line that says anything, cut to something a directory listing can be
/// read at a glance. A plan with no words in it at all is named for the moment
/// it arrived, since that is the only thing left to tell it by.
fn plan_slug(text: &str, now: chrono::DateTime<chrono::Local>) -> String {
    let title = text
        .lines()
        .map(|line| line.trim().trim_start_matches('#').trim())
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let mut slug = String::new();
    for ch in title.chars() {
        if ch.is_alphanumeric() {
            slug.extend(ch.to_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= 60 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        now.format("plan-%Y%m%d-%H%M%S").to_string()
    } else {
        slug.to_string()
    }
}

impl App {
    /// The plan review, while one is open. `None` is the ordinary state.
    #[must_use]
    pub fn plan_review(&self) -> Option<&PlanReview> {
        self.plan.as_ref()
    }

    /// Which half of the open review has the keyboard.
    #[must_use]
    pub fn plan_focus(&self) -> PlanFocus {
        self.plan.as_ref().map_or(PlanFocus::Preview, |r| r.focus)
    }

    pub(super) fn open_plan_review(&mut self, id: keke_acp::PermissionId, call: &ToolCall) {
        let text = plan_text(call);
        let path = self.write_plan(&text);
        self.transcript.request_plan(id, text.clone(), path);
        self.plan = Some(PlanReview {
            text,
            row: PlanRow::ManualApprove,
            focus: PlanFocus::Preview,
        });
        self.scroll.follow();
    }

    /// Write the plan under `$KEKE_HOME/plans`, and say where it went.
    ///
    /// A plan is a document a person reads, edits, and shows to somebody else,
    /// and until now it lived only in this process — closing keke threw away
    /// the one artifact of a planning session. Named for its own title rather
    /// than for the session, so the directory reads as a list of what was
    /// planned; a revised plan under the same title replaces its earlier
    /// draft, which is what "the plan" means to the person who asked for it.
    ///
    /// Best-effort, like the config writes beside it: a plan that could not be
    /// saved is still a plan to review, and an error cell over a convenience
    /// write would push the plan itself off the screen.
    fn write_plan(&self, text: &str) -> Option<std::path::PathBuf> {
        if text.trim().is_empty() {
            return None;
        }
        let home = self.config_home.as_ref()?;
        let directory = home.as_path().join("plans");
        let path = directory.join(format!("{}.md", plan_slug(text, chrono::Local::now())));
        if let Err(error) = std::fs::create_dir_all(&directory)
            .and_then(|()| std::fs::write(&path, format!("{}\n", text.trim_end())))
        {
            tracing::warn!(%error, path = %path.display(), "could not save the plan");
            return None;
        }
        Some(path)
    }

    /// Enter on the selected row: approve under its policy, or — on the "tell
    /// Keke what to change" row — hand the keyboard to the composer instead.
    pub fn commit_plan_row(&mut self) {
        let Some(review) = &self.plan else {
            return;
        };
        match review.row.policy() {
            Some(policy) => self.approve_plan_with(policy),
            None => self.toggle_plan_focus(),
        }
    }

    /// Approve the plan: the agent may start building, under the policy the
    /// chosen row carries.
    ///
    /// `exit_plan_mode` is the agent asking whether it may leave plan mode;
    /// approving it is the answer. Requesting the mode change here — the same
    /// way [`Self::quit_plan`] does when the plan is dropped instead — rather
    /// than waiting on the agent to echo it back over
    /// [`keke_acp::Update::ModeChanged`] keeps the status bar from sitting on
    /// `plan` after a person has already answered; a late `ModeChanged` from
    /// the agent after this only repeats what was already asked for.
    fn approve_plan_with(&mut self, policy: ApprovalPolicy) {
        // Plan mode is the tightest rung of the strictness ladder, so leaving
        // it is the moment a person decides how much of the plan may happen
        // without them. The panel asked that while they read; this is the
        // answer, not a policy inherited from underneath plan mode.
        self.set_approval_policy_aloud(policy);
        self.answer_permission_with_note(PermissionAnswer::Allow, None);
        self.plan = None;
        self.request_session_mode(keke_config_types::SessionMode::Default);
    }

    /// Send the agent back to planning, with whatever was said about the plan.
    pub fn request_plan_changes(&mut self) {
        if self.plan.is_none() {
            return;
        }
        let notes = self.input.take();
        let feedback = PlanReview::feedback(Some(&notes));
        self.show_plan_feedback(feedback.as_deref());
        if feedback.is_none() {
            self.set_flash("plan sent back — type what should change");
        }
        // The refusal reason is what the model reads as the call's result, so
        // what the person wrote about the plan is the refusal itself.
        self.answer_permission_with_note(PermissionAnswer::Deny, feedback);
        self.plan = None;
    }

    /// Put what the person said into the transcript.
    ///
    /// It travels to the model with the answer, which the transcript does not
    /// show — so without this a person's own comments would be the one thing
    /// said this turn that they cannot read back.
    fn show_plan_feedback(&mut self, feedback: Option<&str>) {
        if let Some(text) = feedback {
            self.transcript.push(Cell::User(text.to_string()));
        }
    }

    /// Abandon the plan and leave plan mode.
    ///
    /// Unlike requesting changes, this asks for the mode itself to end: a
    /// person who is done planning wants the next prompt answered, not planned.
    ///
    /// Denying the approval alone only settles that one call; the turn that
    /// asked for it is still running underneath. Cancel it too, the same way
    /// Ctrl-C would, so esc actually stops the agent rather than leaving it to
    /// keep working toward a plan nobody is reading anymore.
    pub fn quit_plan(&mut self) {
        if self.plan.is_none() {
            return;
        }
        self.answer_permission(PermissionAnswer::Deny);
        self.plan = None;
        self.request_session_mode(keke_config_types::SessionMode::Default);
        if self.turn.is_busy() {
            self.conversation.cancel();
            self.transcript.cancel_running_tools();
            self.end_turn();
        }
    }

    /// Move the row the arrow keys have landed on.
    pub fn move_plan_policy(&mut self, delta: isize) {
        let Some(review) = &mut self.plan else {
            return;
        };
        let at = ROWS.iter().position(|row| *row == review.row).unwrap_or(0) as isize;
        let at = (at + delta).rem_euclid(ROWS.len() as isize) as usize;
        review.row = ROWS[at];
    }

    /// Open the saved plan file in vim.
    ///
    /// Held as a pending action rather than spawned here: the terminal's raw
    /// mode belongs to the event loop, and it is the only thing that can
    /// suspend it before vim takes the screen.
    pub fn edit_plan(&mut self) {
        if self.plan.is_none() {
            return;
        }
        match self
            .transcript
            .last_plan()
            .and_then(|cell| cell.path.clone())
        {
            Some(path) => self.pending_edit = Some(path),
            None => self.set_flash("no plan file to edit"),
        }
    }

    /// The plan file waiting to be opened, if `edit_plan` was just pressed.
    pub fn take_pending_edit(&mut self) -> Option<std::path::PathBuf> {
        self.pending_edit.take()
    }

    /// Move the keyboard between the preview and the composer.
    pub fn toggle_plan_focus(&mut self) {
        let Some(review) = &mut self.plan else {
            return;
        };
        review.focus = match review.focus {
            PlanFocus::Preview => PlanFocus::Composer,
            PlanFocus::Composer => PlanFocus::Preview,
        };
    }

    /// Hand the keyboard back to the preview, keeping what was typed — a
    /// person who pressed Esc wanted out of the composer, not their words
    /// deleted.
    pub fn focus_plan_preview(&mut self) {
        if let Some(review) = &mut self.plan {
            review.focus = PlanFocus::Preview;
        }
    }

    /// Enter in the composer: send the plan back with whatever notes are in
    /// the box.
    pub fn submit_plan_composer(&mut self) {
        if self.plan.is_none() {
            return;
        }
        self.request_plan_changes();
    }

    /// `/view-plan`: put the last plan back on screen.
    ///
    /// It is still in the scrollback — a plan is never thrown away now — so
    /// this is a scroll, not a resurrection.
    pub(super) fn view_plan_command(&mut self) {
        if self.transcript.last_plan().is_none() {
            self.set_flash("no plan yet — /plan asks the agent for one");
            return;
        }
        self.show_last_plan = true;
    }

    /// What the composer is writing about, while it is writing about a plan.
    #[must_use]
    pub fn plan_comment_label(&self) -> Option<String> {
        let review = self.plan.as_ref()?;
        (review.focus == PlanFocus::Composer).then(|| " what should change? ".to_string())
    }

    /// Which rendered line the frame should bring into view: the top of the
    /// last plan, when `/view-plan` asked for it.
    pub(crate) fn wanted_plan_line(&mut self, plan_lines: &[(usize, usize)]) -> Option<usize> {
        std::mem::take(&mut self.show_last_plan)
            .then(|| plan_lines.first().map(|(_, line)| *line))
            .flatten()
    }

    /// Told by `draw` which rendered line the frame put the plan's first line
    /// on, so `/view-plan` scrolls it into view.
    pub(crate) fn reveal_plan_line(&mut self, line: usize) {
        self.scroll.reveal(line);
    }
}
