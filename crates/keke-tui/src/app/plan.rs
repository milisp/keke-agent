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
    /// The policy the plan would be carried out under, chosen with the arrow
    /// keys on the panel under the transcript.
    policy: keke_config_types::ApprovalPolicy,
    /// The plan line the selection is on, 0-based.
    cursor: usize,
    /// Where a range selection started, while one is being made.
    anchor: Option<usize>,
    comments: Vec<PlanComment>,
    focus: PlanFocus,
    /// The lines the composer is currently writing a comment about. `None`
    /// means the composer holds freeform revision notes instead.
    commenting: Option<(usize, usize)>,
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

    /// The policy that would carry this plan out.
    #[must_use]
    pub fn policy(&self) -> keke_config_types::ApprovalPolicy {
        self.policy
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

    pub(super) fn open_plan_review(&mut self, id: keke_acp::PermissionId, call: &ToolCall) {
        let text = plan_text(call);
        let path = self.write_plan(&text);
        self.transcript.request_plan(id, text.clone(), path);
        self.plan = Some(PlanReview {
            text,
            // Starts at the policy in force: a person who just wants the plan
            // carried out the way the session already runs presses one key.
            policy: self.approval,
            cursor: 0,
            anchor: None,
            comments: Vec::new(),
            focus: PlanFocus::Preview,
            commenting: None,
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

    /// Approve the plan: the agent may start building, under the policy
    /// chosen on the panel below it.
    ///
    /// The mode is not cleared here. Plan mode ends when the agent says it has
    /// ended, over [`keke_acp::Update::ModeChanged`] — a surface that turned
    /// its own flag off on approval would be drawing an outcome it had only
    /// asked for.
    pub fn approve_plan(&mut self) {
        let Some(policy) = self.plan.as_ref().map(|review| review.policy) else {
            return;
        };
        // Plan mode is the tightest rung of the strictness ladder, so leaving
        // it is the moment a person decides how much of the plan may happen
        // without them. The panel asked that while they read; this is the
        // answer, not a policy inherited from underneath plan mode.
        self.set_approval_policy_aloud(policy);
        let feedback = self.plan.as_ref().and_then(|r| r.feedback(None));
        self.show_plan_feedback(feedback.as_deref());
        self.answer_permission_with_note(PermissionAnswer::Allow, feedback);
        self.plan = None;
    }

    /// Send the agent back to planning, with whatever was said about the plan.
    pub fn request_plan_changes(&mut self) {
        if self.plan.is_none() {
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
    pub fn quit_plan(&mut self) {
        if self.plan.is_none() {
            return;
        }
        self.answer_permission(PermissionAnswer::Deny);
        self.plan = None;
        self.request_session_mode(keke_config_types::SessionMode::Default);
    }

    /// Choose the policy the plan will be carried out under: the arrow keys on
    /// the panel under the transcript.
    pub fn move_plan_policy(&mut self, delta: isize) {
        let Some(review) = &mut self.plan else {
            return;
        };
        let policies = crate::slash::POLICIES;
        let at = policies
            .iter()
            .position(|policy| *policy == review.policy)
            .unwrap_or(0) as isize;
        let at = (at + delta).rem_euclid(policies.len() as isize) as usize;
        review.policy = policies[at];
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
        if review.focus != PlanFocus::Composer {
            return None;
        }
        Some(match review.commenting {
            Some((first, last)) if first == last => format!(" comment on line {first} "),
            Some((first, last)) => format!(" comment on lines {first}-{last} "),
            None => " what should change? ".to_string(),
        })
    }

    /// How the waiting plan is being read, for the frame that draws it.
    #[must_use]
    pub(crate) fn plan_view(&self) -> Option<crate::draw::transcript::PlanView> {
        let review = self.plan.as_ref()?;
        let (first, last) = review.selection();
        Some(crate::draw::transcript::PlanView {
            first,
            last,
            selecting: review.focus == PlanFocus::Preview,
        })
    }

    /// Which rendered line the frame should bring into view: the selected plan
    /// line, or the top of the last plan when `/view-plan` asked for it.
    pub(crate) fn wanted_plan_line(&mut self, plan_lines: &[(usize, usize)]) -> Option<usize> {
        if std::mem::take(&mut self.show_last_plan) {
            return plan_lines.first().map(|(_, line)| *line);
        }
        let (first, _) = self.plan.as_ref()?.selection();
        plan_lines
            .iter()
            .find(|(plan_line, _)| *plan_line == first)
            .map(|(_, line)| *line)
    }

    /// Told by `draw` which rendered line the selected plan line landed on, so
    /// moving the selection scrolls the transcript rather than moving a
    /// highlight off screen.
    pub(crate) fn reveal_plan_line(&mut self, line: usize) {
        self.scroll.reveal(line);
    }
}
