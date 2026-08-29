use keke_acp::PermissionAnswer;
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
    /// Scrolling and selecting lines. The single letters answer the plan here.
    #[default]
    Preview,
    /// Typing — a comment on the selected lines, or freeform revision notes.
    /// No letter answers the plan while this has focus, because a person
    /// writing "say more here" must not have `s` and `a` fire under them.
    Composer,
}

/// A remark attached to a run of plan lines.
///
/// Line numbers are 1-based over the plan's own lines and inclusive at both
/// ends, matching the gutter the preview draws — the number a person read on
/// screen is the number the agent is told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanComment {
    pub first: usize,
    pub last: usize,
    pub text: String,
}

/// The plan awaiting a person's answer, and where they have scrolled to in it.
#[derive(Clone)]
pub struct PlanReview {
    /// The plan as the agent wrote it. Empty when it proposed none, which is a
    /// state of its own rather than an error: a person can still approve, ask
    /// for changes, or drop the plan.
    text: String,
    scroll: usize,
    /// The plan line the selection is on, 0-based.
    cursor: usize,
    /// Where a range selection started, while one is being made.
    anchor: Option<usize>,
    comments: Vec<PlanComment>,
    focus: PlanFocus,
    /// The lines the composer is currently writing a comment about. `None`
    /// means the composer holds freeform revision notes instead.
    commenting: Option<(usize, usize)>,
    /// Where the plan was written, when it could be written at all. Shown to
    /// the person: a plan they cannot find again is one they have to ask for
    /// twice.
    path: Option<std::path::PathBuf>,
    /// How this plan was answered, once it was. `Some` makes the review a
    /// record: it can be read and copied, never answered again.
    answered: Option<PermissionAnswer>,
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

    /// The file this plan was written to, when it was.
    #[must_use]
    pub fn path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
    }

    #[must_use]
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    #[must_use]
    pub fn focus(&self) -> PlanFocus {
        self.focus
    }

    #[must_use]
    pub fn comments(&self) -> &[PlanComment] {
        &self.comments
    }

    /// Whether the composer is writing a comment rather than revision notes.
    #[must_use]
    pub fn is_commenting(&self) -> bool {
        self.commenting.is_some()
    }

    /// Whether this plan has already been answered, and so is a record of what
    /// was decided rather than a live question.
    #[must_use]
    pub fn is_answered(&self) -> bool {
        self.answered.is_some()
    }

    /// The selected plan lines, 0-based and inclusive at both ends.
    #[must_use]
    pub fn selection(&self) -> (usize, usize) {
        let anchor = self.anchor.unwrap_or(self.cursor);
        (anchor.min(self.cursor), anchor.max(self.cursor))
    }

    fn line_count(&self) -> usize {
        self.text.lines().count()
    }

    /// The comments, and any freeform notes, as the text the agent will read —
    /// `None` when the person said nothing, so a bare yes stays a bare yes.
    ///
    /// The agent sees text and not this struct, so every comment names the
    /// lines it is about *and* quotes them: a line number alone is ambiguous
    /// the moment the agent rewrites the plan, and a quote alone is ambiguous
    /// whenever the same sentence appears twice. The wording matches
    /// grok-build's plan approval view; nothing is ported, but an agent that
    /// has read one of these before should not have to learn a second dialect.
    #[must_use]
    pub fn feedback(&self, freeform: Option<&str>) -> Option<String> {
        let lines: Vec<&str> = self.text.lines().collect();
        let mut parts: Vec<String> = self
            .comments
            .iter()
            .map(|comment| {
                let label = if comment.first == comment.last {
                    format!("Proposed plan line {}:", comment.first)
                } else {
                    format!("Proposed plan lines {}-{}:", comment.first, comment.last)
                };
                let quoted = lines
                    .get(comment.first.saturating_sub(1)..comment.last.min(lines.len()))
                    .unwrap_or_default()
                    .iter()
                    .map(|line| format!("> {line}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("{label}\n{quoted}\n\nComment:\n{}", comment.text)
            })
            .collect();

        if let Some(text) = freeform
            && !text.trim().is_empty()
        {
            // Labelled only when it follows comments, so the agent can tell
            // "and also, generally" from a remark about a particular line.
            parts.push(if parts.is_empty() {
                text.trim().to_string()
            } else {
                format!("Additional feedback:\n{}", text.trim())
            });
        }
        (!parts.is_empty()).then(|| parts.join("\n\n"))
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

    pub(super) fn open_plan_review(&mut self, call: &ToolCall) {
        let text = plan_text(call);
        let path = self.write_plan(&text);
        self.plan = Some(PlanReview {
            path,
            text,
            scroll: 0,
            cursor: 0,
            anchor: None,
            comments: Vec::new(),
            focus: PlanFocus::Preview,
            commenting: None,
            answered: None,
        });
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

    /// Put the answered plan away where `/view-plan` can find it.
    ///
    /// It lives on the app rather than in the transcript because the
    /// transcript records what was *said* — the permission prompt and its
    /// answer — while this is reviewable state: a scroll position, a set of
    /// comments, and a plan body that would otherwise be gone the moment it
    /// was answered. Only the last one is kept: "the plan" in a person's head
    /// is the one they just looked at.
    fn archive_plan(&mut self, answer: PermissionAnswer) {
        if let Some(mut review) = self.plan.take() {
            review.answered = Some(answer);
            review.focus = PlanFocus::Preview;
            review.commenting = None;
            self.last_plan = Some(review);
        }
    }

    /// Approve the plan: the agent may start building.
    ///
    /// The mode is not cleared here. Plan mode ends when the agent says it has
    /// ended, over [`keke_acp::Update::ModeChanged`] — a surface that turned
    /// its own flag off on approval would be drawing an outcome it had only
    /// asked for.
    pub fn approve_plan(&mut self) {
        if self.refuse_answered_plan() {
            return;
        }
        // Not answered here. Plan mode is the tightest rung of the strictness
        // ladder, so leaving it is the one moment a person is deciding how
        // much of the plan may be carried out without them — the overlay asks
        // that once, rather than letting the policy underneath plan mode
        // decide it silently.
        self.open_policy_picker();
    }

    /// Approve, and carry the plan out under `policy`: what the overlay
    /// [`App::approve_plan`] opens answers with.
    pub fn approve_plan_under(&mut self, policy: keke_config_types::ApprovalPolicy) {
        if self.refuse_answered_plan() {
            return;
        }
        self.set_approval_policy_aloud(policy);
        let feedback = self.plan.as_ref().and_then(|r| r.feedback(None));
        self.show_plan_feedback(feedback.as_deref());
        self.answer_permission_with_note(PermissionAnswer::Allow, feedback);
        self.archive_plan(PermissionAnswer::Allow);
    }

    /// Send the agent back to planning, with whatever was said about the plan.
    pub fn request_plan_changes(&mut self) {
        if self.refuse_answered_plan() {
            return;
        }
        let notes = self.input.take();
        let feedback = self.plan.as_ref().and_then(|r| r.feedback(Some(&notes)));
        self.show_plan_feedback(feedback.as_deref());
        if feedback.is_none() {
            self.set_flash("plan sent back — type what should change");
        }
        // The refusal reason is what the model reads as the call's result, so
        // what the person wrote about the plan is the refusal itself.
        self.answer_permission_with_note(PermissionAnswer::Deny, feedback);
        self.archive_plan(PermissionAnswer::Deny);
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
    pub fn quit_plan(&mut self) {
        // A record is closed, not quit: its question was answered long ago,
        // and asking to leave plan mode again would be answering for the
        // person a second time.
        if self.plan.as_ref().is_some_and(PlanReview::is_answered) {
            self.plan = None;
            return;
        }
        self.answer_permission(PermissionAnswer::Deny);
        self.archive_plan(PermissionAnswer::Deny);
        self.request_session_mode(keke_config_types::SessionMode::Default);
    }

    /// Whether the open review is a record, saying so if it is.
    fn refuse_answered_plan(&mut self) -> bool {
        if self.plan.as_ref().is_some_and(PlanReview::is_answered) {
            self.set_flash("this plan was already answered — q closes the record");
            return true;
        }
        false
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

    /// Move the selected line. `extend` grows the range instead of moving it,
    /// which is how a comment comes to cover more than one line.
    pub fn move_plan_cursor(&mut self, delta: isize, extend: bool) {
        let Some(review) = &mut self.plan else {
            return;
        };
        let last = review.line_count().saturating_sub(1);
        if extend {
            review.anchor.get_or_insert(review.cursor);
        } else {
            review.anchor = None;
        }
        review.cursor = review.cursor.saturating_add_signed(delta).min(last);
    }

    /// Start writing a comment about the selected lines.
    pub fn begin_plan_comment(&mut self) {
        if self.refuse_answered_plan() {
            return;
        }
        let Some(review) = &mut self.plan else {
            return;
        };
        if review.is_empty() {
            self.set_flash("no plan lines to comment on");
            return;
        }
        let (first, last) = review.selection();
        review.commenting = Some((first + 1, last + 1));
        review.focus = PlanFocus::Composer;
    }

    /// Move the keyboard between the preview and the composer.
    pub fn toggle_plan_focus(&mut self) {
        match self.plan_focus() {
            PlanFocus::Preview => {
                if let Some(review) = &mut self.plan {
                    review.focus = PlanFocus::Composer;
                    review.commenting = None;
                }
            }
            PlanFocus::Composer => self.focus_plan_preview(),
        }
    }

    /// Hand the keyboard back to the preview, abandoning a half-written
    /// comment's line range but keeping what was typed — a person who pressed
    /// Esc wanted out of commenting, not their words deleted.
    pub fn focus_plan_preview(&mut self) {
        if let Some(review) = &mut self.plan {
            review.focus = PlanFocus::Preview;
            review.commenting = None;
        }
    }

    /// Enter in the composer: attach the comment being written, or send the
    /// plan back with whatever notes are in the box.
    pub fn submit_plan_composer(&mut self) {
        let Some(review) = &self.plan else {
            return;
        };
        let Some((first, last)) = review.commenting else {
            self.request_plan_changes();
            return;
        };
        let text = self.input.take();
        let text = text.trim().to_string();
        let Some(review) = &mut self.plan else {
            return;
        };
        review.commenting = None;
        review.focus = PlanFocus::Preview;
        if text.is_empty() {
            return;
        }
        review.comments.push(PlanComment { first, last, text });
        review.anchor = None;
    }

    /// Reopen the last plan this session saw, as a record.
    pub(super) fn view_plan_command(&mut self) {
        if self.plan.is_some() {
            return;
        }
        let Some(review) = self.last_plan.clone() else {
            self.set_flash("no plan yet — /plan asks the agent for one");
            return;
        };
        self.plan = Some(review);
    }

    /// Told by `draw` how far down the plan can go, so a held-down arrow key
    /// cannot scroll past the end and leave a blank panel behind.
    pub(crate) fn clamp_plan_scroll(&mut self, max: usize) {
        if let Some(review) = &mut self.plan {
            review.scroll = review.scroll.min(max);
        }
    }

    /// Told by `draw` where the selected line landed, so moving the selection
    /// scrolls the panel rather than moving a highlight nobody can see.
    pub(crate) fn reveal_plan_row(&mut self, row: usize, visible: usize) {
        let Some(review) = &mut self.plan else {
            return;
        };
        if row < review.scroll {
            review.scroll = row;
        } else if visible > 0 && row >= review.scroll + visible {
            review.scroll = row + 1 - visible;
        }
    }
}
