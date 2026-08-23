//! The state a terminal draws, and the only place it changes.
//!
//! Nothing here touches a terminal, so every rule the interface has — what a
//! denial looks like, when Ctrl-C quits, whether new output moves the view —
//! is assertable without a backend.

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use keke_acp::Conversation;
use keke_acp::PermissionAnswer;
use keke_acp::PermissionId;
use keke_acp::Update;
use keke_config_types::ApprovalPolicy;
use keke_config_types::ReasoningEffort;
use keke_protocol::StopReason;
use keke_protocol::Usage;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;

use crate::history::PromptHistory;
use crate::input::InputBox;
use crate::login::Notice;
use crate::scroll::Scrollback;
use crate::slash::Builtin;
use crate::slash::SlashAction;
use crate::slash::SlashCommands;
use crate::transcript::Cell;
use crate::transcript::Transcript;

/// Where the conversation is, from the surface's point of view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Turn {
    #[default]
    Idle,
    Running,
    /// Stopped on an approval prompt. Distinct from `Running` because the
    /// person, not the agent, is the one holding the turn up.
    AwaitingPermission,
}

impl Turn {
    pub fn is_busy(self) -> bool {
        !matches!(self, Turn::Idle)
    }
}

pub struct App {
    conversation: Arc<dyn Conversation>,
    /// Updates the surface generates for itself — a prompt that never left, a
    /// login notice. Merged with the agent's stream so the draw loop has one
    /// source of truth.
    local: UnboundedSender<Update>,
    pub transcript: Transcript,
    pub input: InputBox,
    pub scroll: Scrollback,
    pub commands: SlashCommands,
    /// What was typed in this project before, and where the arrow keys are
    /// within it.
    pub history: PromptHistory,
    /// Which completion the arrow keys are on. Clamped rather than reset on
    /// every keystroke, so typing one more letter does not jump the highlight
    /// back to the top of a list the person was already moving through.
    completion: usize,
    approval: ApprovalPolicy,
    /// How hard the model is asked to think. `None` is the vendor's own
    /// default, which is a state of its own and not the lowest rung.
    effort: Option<ReasoningEffort>,
    turn: Turn,
    /// When the running turn started, and how long the last one took. Both are
    /// held because the status bar keeps showing the duration after the turn
    /// ends: "worked for 12s" is what a person looks for once the answer is on
    /// screen and they have stopped watching the clock.
    started: Option<Instant>,
    last_turn: Option<Duration>,
    /// Tokens this session has spent, including whatever a resumed log already
    /// accounted for.
    usage: Usage,
    show_thinking: bool,
    should_quit: bool,
}

impl App {
    /// Returns the app and the receiver for its self-generated updates; the
    /// event loop selects over that alongside the agent's stream.
    pub fn new(conversation: Arc<dyn Conversation>) -> (Self, UnboundedReceiver<Update>) {
        let (local, local_updates) = tokio::sync::mpsc::unbounded_channel();
        (
            Self {
                conversation,
                local,
                transcript: Transcript::default(),
                input: InputBox::default(),
                scroll: Scrollback::default(),
                commands: SlashCommands::default(),
                history: PromptHistory::default(),
                completion: 0,
                approval: ApprovalPolicy::default(),
                effort: None,
                turn: Turn::Idle,
                started: None,
                last_turn: None,
                usage: Usage::default(),
                show_thinking: true,
                should_quit: false,
            },
            local_updates,
        )
    }

    /// The command list a person completes against. Composed by the host,
    /// because nothing here knows what a plugin is.
    #[must_use]
    pub fn with_commands(mut self, commands: SlashCommands) -> Self {
        self.commands = commands;
        self
    }

    /// The prompt history this project already had, and the sink new prompts
    /// are appended to.
    #[must_use]
    pub fn with_prompt_history(mut self, history: PromptHistory) -> Self {
        self.history = history;
        self
    }

    /// The mode the session was started in. The surface shows it and cycles it
    /// from here on; the session is told through the seam.
    #[must_use]
    pub fn with_approval_policy(mut self, policy: ApprovalPolicy) -> Self {
        self.approval = policy;
        self
    }

    /// The level the session was configured with, so the bar and `/effort`
    /// start from what is actually in force rather than from a guess.
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: Option<ReasoningEffort>) -> Self {
        self.effort = effort;
        self
    }

    /// Seed the surface from a resumed session: what was said, and what it has
    /// already spent.
    ///
    /// The transcript is rebuilt from the same history the engine resumes with,
    /// so what a person reads on screen is what the model is about to be sent —
    /// a summary written separately would be free to drift from it.
    #[must_use]
    pub fn with_history(mut self, history: &[keke_protocol::Message], usage: Usage) -> Self {
        self.transcript.replay(history);
        self.usage = usage;
        self
    }

    pub fn approval_policy(&self) -> ApprovalPolicy {
        self.approval
    }

    pub fn turn(&self) -> Turn {
        self.turn
    }

    /// What this session has spent so far.
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// How long the current turn has been running, or how long the last one
    /// took. `None` before the first turn.
    pub fn elapsed(&self) -> Option<Duration> {
        match self.started {
            Some(started) => Some(started.elapsed()),
            None => self.last_turn,
        }
    }

    /// Whether a turn is on the clock, so the caller redraws on a timer rather
    /// than only when something arrives.
    pub fn is_timing(&self) -> bool {
        self.started.is_some()
    }

    pub fn show_thinking(&self) -> bool {
        self.show_thinking
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn toggle_thinking(&mut self) {
        self.show_thinking = !self.show_thinking;
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    /// Fold one update into the transcript.
    pub fn apply(&mut self, update: Update) {
        match update {
            Update::TurnStarted => {
                self.begin_turn();
                self.transcript.seal();
            }
            Update::TextDelta(text) => {
                self.begin_turn();
                self.transcript.push_text_delta(&text);
            }
            Update::ThinkingDelta(text) => {
                self.begin_turn();
                self.transcript.push_thinking_delta(&text);
            }
            Update::ToolCallStarted(call) => {
                self.begin_turn();
                self.transcript.start_tool(&call);
            }
            Update::ToolCallEnded(result) => {
                if !self.transcript.finish_tool(&result) {
                    // A result with no call on screen still has to be visible;
                    // silently dropping it would hide work that really ran.
                    self.transcript.push(Cell::Error(format!(
                        "result for unknown call {}",
                        result.id
                    )));
                }
            }
            Update::TokensUsed(usage) => self.usage.add(usage),
            Update::PermissionRequested { id, call, reason } => {
                self.turn = Turn::AwaitingPermission;
                self.transcript.request_permission(id, &call, reason);
            }
            Update::TurnEnded(reason) => {
                self.end_turn();
                self.transcript.seal();
                if let StopReason::Refusal { message } = reason {
                    self.transcript
                        .push(Cell::Error(format!("refused: {message}")));
                }
            }
            Update::Failed(message) => {
                // Deliberately does not quit: the seam promises the
                // conversation survives a failed turn.
                self.end_turn();
                self.transcript.push(Cell::Error(message));
            }
        }
    }

    /// Show something the host wants said without printing over the interface.
    pub fn apply_notice(&mut self, notice: Notice) {
        self.transcript.push(Cell::Notice(notice.to_string()));
    }

    /// Send whatever is in the input box, if anything.
    ///
    /// The prompt is spawned rather than awaited so a slow agent cannot stop
    /// the surface from redrawing or from accepting Ctrl-C.
    pub fn submit(&mut self) {
        if self.input.is_empty() {
            return;
        }
        let text = self.input.take();
        self.history.submit(&text);
        if let Some((name, arguments)) = crate::slash::parse(text.trim()) {
            self.run_command(&text, name, arguments);
            return;
        }
        self.transcript.push(Cell::User(text.clone()));
        // Submitting is an intent to watch the answer, so it returns to live.
        self.scroll.follow();
        self.begin_turn();

        let conversation = Arc::clone(&self.conversation);
        let local = self.local.clone();
        tokio::spawn(async move {
            if let Err(error) = conversation.prompt(text).await {
                let _ = local.send(Update::Failed(error.to_string()));
            }
        });
    }

    /// Ctrl-C: stop the turn, or leave if there is nothing to stop.
    ///
    /// Quitting mid-turn would orphan whatever the agent is doing, so the first
    /// Ctrl-C never exits while work is in flight.
    pub fn interrupt(&mut self) {
        if self.turn.is_busy() {
            self.conversation.cancel();
            self.transcript.cancel_running_tools();
            self.transcript.push(Cell::Notice("cancelled".to_string()));
            self.end_turn();
        } else {
            self.should_quit = true;
        }
    }

    /// Mark the turn running, starting the clock if it was not already.
    ///
    /// Not restarted per update: a turn's elapsed time is measured from the
    /// prompt, so the number a person reads is how long they have waited rather
    /// than how long since the last token.
    fn begin_turn(&mut self) {
        self.turn = Turn::Running;
        if self.started.is_none() {
            self.started = Some(Instant::now());
        }
    }

    fn end_turn(&mut self) {
        self.turn = Turn::Idle;
        if let Some(started) = self.started.take() {
            self.last_turn = Some(started.elapsed());
        }
    }

    /// Answer the prompt currently blocking the turn.
    pub fn answer_permission(&mut self, answer: PermissionAnswer) {
        let Some(id) = self.open_permission_id() else {
            return;
        };
        self.conversation.respond_to_permission(&id, answer);
        self.transcript.answer_permission(&id, answer);
        // Denial ends nothing by itself: the agent decides what to do next.
        self.turn = Turn::Running;
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

    /// Cycle the approval mode: the shift-tab gesture.
    ///
    /// Silent, unlike `/mode`. The gesture is meant to be tapped through the
    /// modes while looking at the status bar, and a line per tap would push the
    /// conversation off screen to say what the bar is already saying.
    pub fn cycle_approval_policy(&mut self) {
        let next = match self.approval {
            ApprovalPolicy::OnRequest => ApprovalPolicy::OnFailure,
            ApprovalPolicy::OnFailure => ApprovalPolicy::Never,
            ApprovalPolicy::Never => ApprovalPolicy::OnRequest,
        };
        self.set_approval_policy(next);
    }

    pub fn set_approval_policy(&mut self, policy: ApprovalPolicy) {
        self.approval = policy;
        self.conversation.set_approval_policy(policy);
    }

    /// Set the mode and say so, which is what a typed `/mode` does: the person
    /// asked in the transcript, so the answer belongs there too.
    fn set_approval_policy_aloud(&mut self, policy: ApprovalPolicy) {
        self.set_approval_policy(policy);
        self.transcript.push(Cell::Notice(format!(
            "approval mode: {}",
            crate::slash::policy_name(policy)
        )));
    }

    #[must_use]
    pub fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.effort
    }

    pub fn set_reasoning_effort(&mut self, effort: Option<ReasoningEffort>) {
        self.effort = effort;
        self.conversation.set_reasoning_effort(effort);
    }

    /// Set the level and say so, which is what a typed `/effort` does.
    fn set_reasoning_effort_aloud(&mut self, effort: Option<ReasoningEffort>) {
        self.set_reasoning_effort(effort);
        self.transcript.push(Cell::Notice(format!(
            "reasoning effort: {}",
            crate::slash::effort_name(effort)
        )));
    }

    fn run_command(&mut self, typed: &str, name: &str, arguments: &str) {
        let Some(command) = self.commands.find(name) else {
            self.transcript.push(Cell::Error(format!(
                "unknown command /{name} — /help lists them"
            )));
            return;
        };
        match command.action.clone() {
            SlashAction::Builtin(Builtin::Help) => {
                let text = self.help_text();
                self.transcript.push(Cell::Notice(text));
            }
            SlashAction::Builtin(Builtin::Clear) => {
                // On screen only: the rollout log is the record, and a person
                // clearing the view is not asking the agent to forget.
                self.transcript = Transcript::default();
                self.scroll.follow();
            }
            SlashAction::Builtin(Builtin::Quit) => self.should_quit = true,
            SlashAction::Builtin(Builtin::Thinking) => {
                self.toggle_thinking();
                let state = if self.show_thinking {
                    "shown"
                } else {
                    "hidden"
                };
                self.transcript
                    .push(Cell::Notice(format!("reasoning {state}")));
            }
            SlashAction::Builtin(Builtin::Effort) => match crate::slash::effort(arguments) {
                Ok(Some(effort)) => self.set_reasoning_effort_aloud(effort),
                Ok(None) => {
                    let next = crate::slash::next_effort(self.effort);
                    self.set_reasoning_effort_aloud(next);
                }
                Err(unknown) => self.transcript.push(Cell::Error(unknown)),
            },
            SlashAction::Builtin(Builtin::Mode) => match crate::slash::policy(arguments) {
                Ok(Some(policy)) => self.set_approval_policy_aloud(policy),
                Ok(None) => {
                    self.cycle_approval_policy();
                    let policy = self.approval;
                    self.set_approval_policy_aloud(policy);
                }
                Err(unknown) => self.transcript.push(Cell::Error(unknown)),
            },
            SlashAction::Prompt(path) => match std::fs::read_to_string(&path) {
                Ok(body) => {
                    let text = if arguments.is_empty() {
                        body
                    } else {
                        format!("{body}\n\n{arguments}")
                    };
                    // What the person typed is what they should see; the body
                    // goes to the model, not onto their screen.
                    self.transcript.push(Cell::User(typed.trim().to_string()));
                    self.send(text);
                }
                Err(error) => self
                    .transcript
                    .push(Cell::Error(format!("reading {}: {error}", path.display()))),
            },
        }
    }

    fn help_text(&self) -> String {
        let mut text = String::from("commands:");
        for entry in self.commands.entries() {
            text.push_str(&format!("\n  /{} — {}", entry.name, entry.description));
        }
        text
    }

    /// Start a turn with text that did not come from the input box.
    fn send(&mut self, text: String) {
        self.scroll.follow();
        self.begin_turn();
        let conversation = Arc::clone(&self.conversation);
        let local = self.local.clone();
        tokio::spawn(async move {
            if let Err(error) = conversation.prompt(text).await {
                let _ = local.send(Update::Failed(error.to_string()));
            }
        });
    }

    pub fn open_permission_id(&self) -> Option<PermissionId> {
        self.transcript
            .open_permission()
            .map(|prompt| prompt.id.clone())
    }
}
