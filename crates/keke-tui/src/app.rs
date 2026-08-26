//! The state a terminal draws, and the only place it changes.
//!
//! Nothing here touches a terminal, so every rule the interface has — what a
//! denial looks like, when Ctrl-C quits, whether new output moves the view —
//! is assertable without a backend.

use std::path::PathBuf;
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

use crate::file_search::FileSearchState;
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
    /// `@`-completion: fuzzy file/folder search over the current line.
    pub file_search: FileSearchState,
    /// What was typed in this project before, and where the arrow keys are
    /// within it.
    pub history: PromptHistory,
    /// Which completion the arrow keys are on. Clamped rather than reset on
    /// every keystroke, so typing one more letter does not jump the highlight
    /// back to the top of a list the person was already moving through.
    completion: usize,
    /// The model overlay, while it is open. `None` is the ordinary state: the
    /// composer has the keyboard.
    picker: Option<crate::picker::ModelPicker>,
    approval: ApprovalPolicy,
    /// How hard the model is asked to think. `None` is the vendor's own
    /// default, which is a state of its own and not the lowest rung.
    effort: Option<ReasoningEffort>,
    /// Which model is answering, and every model this session's provider
    /// serves. The list is empty when the provider could not be asked and had
    /// nothing to fall back on; `/model` then says so rather than showing an
    /// empty menu.
    model: String,
    models: Vec<keke_provider_api::ModelInfo>,
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
    /// Whether keke is asking the terminal for mouse events. On, because the
    /// wheel and the jump-to-bottom button need them. The terminal's own
    /// drag-select is then behind that terminal's bypass modifier — shift in
    /// most, option or fn on macOS — so `/mouse` exists for the terminals
    /// where there is none.
    mouse_capture: bool,
    /// Where the jump-to-bottom button was drawn, as `(x, y, width)`. A click
    /// arrives as a screen position and nothing else, so the only thing that
    /// can say what was clicked is the frame that drew it.
    follow_button: Option<(u16, u16, u16)>,
    /// Which collapsed cells the reader has opened, by index into the
    /// transcript. Cells are only ever appended, so an index stays the cell it
    /// was; nothing a later turn adds can reopen something already closed.
    expanded: std::collections::HashSet<usize>,
    /// Where this frame drew each expandable header, as `(row, cell index)`.
    toggles: Vec<(u16, usize)>,
    /// A word about something keke just did for the person at the keyboard —
    /// copied, resumed. It goes in the status bar and expires, never into
    /// the transcript: the transcript is the conversation, and a line in it
    /// reads as something the agent said.
    flash: Option<(String, Instant)>,
    /// Text waiting to go to the clipboard. Held rather than written here so
    /// the state tests never touch a terminal.
    pending_copy: Option<String>,
    /// Drag-select over the transcript, since a captured mouse is one the
    /// terminal can no longer select with.
    pub(crate) selection: crate::selection::Selection,
    should_quit: bool,
    /// Where `$KEKE_HOME/config.toml` lives, so `/model`, `/effort`, and the
    /// shift-tab approval-mode gesture can write the switch back to disk.
    /// `None` in tests, where there is no home to write into and persistence
    /// is not under test.
    config_home: Option<keke_paths::AbsPath>,
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
                file_search: FileSearchState::new(
                    std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                ),
                history: PromptHistory::default(),
                completion: 0,
                picker: None,
                approval: ApprovalPolicy::default(),
                effort: None,
                model: String::new(),
                models: Vec::new(),
                turn: Turn::Idle,
                started: None,
                last_turn: None,
                usage: Usage::default(),
                show_thinking: true,
                mouse_capture: true,
                follow_button: None,
                expanded: std::collections::HashSet::new(),
                toggles: Vec::new(),
                flash: None,
                pending_copy: None,
                selection: crate::selection::Selection::default(),
                should_quit: false,
                config_home: None,
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

    /// Where `$KEKE_HOME` is, so a typed `/model` or `/effort`, or the
    /// shift-tab approval-mode gesture, writes the new value back to
    /// `config.toml` and outlives this process.
    #[must_use]
    pub fn with_config_home(mut self, home: keke_paths::AbsPath) -> Self {
        self.config_home = Some(home);
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

    /// Which model the session was configured with, and what its provider
    /// serves.
    ///
    /// Both come from the composition root: only it has a provider to ask, and
    /// only it knows that asking one may mean a network call. A surface handed
    /// an empty list offers no choice rather than a wrong one.
    #[must_use]
    pub fn with_models(
        mut self,
        model: impl Into<String>,
        models: Vec<keke_provider_api::ModelInfo>,
    ) -> Self {
        self.model = model.into();
        self.models = models;
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
    /// Whether anything on screen changes on its own. A flash counts only
    /// while it is live: an expired one must not keep an idle interface
    /// redrawing forever.
    pub fn is_timing(&self) -> bool {
        self.started.is_some() || self.flash().is_some() || self.file_search.is_open()
    }

    /// How long a flash stays up. Long enough to read, short enough that it is
    /// gone before it can be mistaken for state.
    const FLASH: Duration = Duration::from_secs(5);

    /// The current flash, if it has not expired.
    pub fn flash(&self) -> Option<&str> {
        self.flash
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Self::FLASH)
            .map(|(text, _)| text.as_str())
    }

    fn set_flash(&mut self, text: impl Into<String>) {
        self.flash = Some((text.into(), Instant::now()));
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

    pub fn mouse_capture(&self) -> bool {
        self.mouse_capture
    }

    /// Give the mouse back to the terminal, or take it again.
    ///
    /// keke holds it by default and answers the drag itself, so clicking a tool
    /// call open and selecting text both work. The escape hatch is for the
    /// terminal whose own selection does something keke's cannot — a
    /// rectangular block, a click-through URL.
    pub fn toggle_mouse_capture(&mut self) {
        self.mouse_capture = !self.mouse_capture;
        self.set_flash(if self.mouse_capture {
            "mouse captured — drag selects, click expands a tool call"
        } else {
            "mouse released to the terminal — keke stops answering it"
        });
    }

    /// Told by `draw` where the jump-to-bottom button ended up, or that it was
    /// not drawn at all.
    pub(crate) fn set_follow_button(&mut self, area: Option<(u16, u16, u16)>) {
        self.follow_button = area;
    }

    /// The cells the reader has opened.
    pub(crate) fn expanded(&self) -> &std::collections::HashSet<usize> {
        &self.expanded
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
    fn copy(&mut self, text: String) {
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
            Update::SessionReset => {
                self.transcript = Transcript::default();
                self.scroll.follow();
                self.usage = Usage::default();
            }
        }
    }

    /// Show something the host wants said without printing over the interface.
    ///
    /// These go in the transcript because a login URL or a device code has to
    /// stay put long enough to be read off the screen and typed elsewhere.
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

    /// Retire the conversation the agent is holding and start a fresh one.
    ///
    /// Unlike `/clear`, this reaches the agent: history and usage go to zero
    /// there too, not just on this surface's own transcript. Spawned for the
    /// same reason `submit` is — rebuilding a session can mean a network
    /// round trip, and that must not stop the interface from redrawing.
    fn start_new_session(&mut self) {
        let conversation = Arc::clone(&self.conversation);
        let local = self.local.clone();
        tokio::spawn(async move {
            if let Err(error) = conversation.new_session().await {
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

    /// Cycle the approval mode: the shift-tab gesture.
    ///
    /// Silent by design. The gesture is meant to be tapped through the modes
    /// while looking at the status bar, and a line per tap would push the
    /// conversation off screen to say what the bar is already saying.
    pub fn cycle_approval_policy(&mut self) {
        let next = match self.approval {
            ApprovalPolicy::OnRequest => ApprovalPolicy::OnFailure,
            ApprovalPolicy::OnFailure => ApprovalPolicy::Never,
            ApprovalPolicy::Never => ApprovalPolicy::OnRequest,
        };
        self.set_approval_policy(next);
        self.persist_override(|file| {
            file.approval_policy = Some(next);
        });
    }

    /// Write one field of `$KEKE_HOME/config.toml`, so the switch a person
    /// just made outlives this process instead of reverting on the next
    /// launch. Best-effort: a write that fails (read-only home, no disk) is
    /// logged rather than surfaced, since the switch already took effect for
    /// this session and a transcript error over a convenience write would be
    /// out of proportion.
    fn persist_override(&self, patch: impl FnOnce(&mut keke_config::ConfigFile)) {
        let Some(home) = &self.config_home else {
            return;
        };
        if let Err(error) = keke_config::persist_user_override(home, patch) {
            tracing::warn!(%error, "could not persist the switch to config.toml");
        }
    }

    pub fn set_approval_policy(&mut self, policy: ApprovalPolicy) {
        self.approval = policy;
        self.conversation.set_approval_policy(policy);
    }

    #[must_use]
    pub fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.effort
    }

    pub fn set_reasoning_effort(&mut self, effort: Option<ReasoningEffort>) {
        self.effort = effort;
        self.conversation.set_reasoning_effort(effort);
    }

    /// Set the level, which is what a typed `/effort` does. Silent in the
    /// transcript: the input box already shows what was typed.
    fn set_reasoning_effort_aloud(&mut self, effort: Option<ReasoningEffort>) {
        self.set_reasoning_effort(effort);
        self.persist_override(|file| {
            file.reasoning_effort = effort.map(|level| level.as_str().to_string());
        });
    }

    /// Which model is answering, for the status bar.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// What this session's provider serves.
    #[must_use]
    pub fn models(&self) -> &[keke_provider_api::ModelInfo] {
        &self.models
    }

    /// The levels the current model takes, or nothing when it did not say.
    ///
    /// Empty is not "no reasoning": it is "the vendor published no ladder", and
    /// the difference matters because the first would hide `/effort` and the
    /// second must leave every rung available.
    fn offered_efforts(&self) -> Vec<ReasoningEffort> {
        self.models
            .iter()
            .find(|model| model.id == self.model)
            .map(|model| model.reasoning_efforts.clone())
            .unwrap_or_default()
    }

    /// Switch models, or say why not.
    ///
    /// A model the provider does not serve is refused rather than sent: the
    /// rejection would otherwise land on the next prompt, long after the
    /// command that caused it. When the provider could not be asked at all the
    /// list is empty and nothing is refused — keke has no grounds to.
    fn set_model_aloud(&mut self, wanted: &str) {
        if !self.models.is_empty() && !self.models.iter().any(|model| model.id == wanted) {
            self.transcript.push(Cell::Error(format!(
                "no model {wanted:?} on this provider — /model lists them"
            )));
            return;
        }
        self.model = wanted.to_string();
        self.conversation.set_model(wanted.to_string());
        self.persist_override(|file| {
            file.model = Some(wanted.to_string());
        });

        // A level the new model does not take would be sent anyway and
        // rejected, so it is dropped here where the cause is still on screen.
        let offered = self.offered_efforts();
        if let Some(level) = self.effort
            && !offered.is_empty()
            && !offered.contains(&level)
        {
            self.set_reasoning_effort(None);
            self.transcript.push(Cell::Notice(format!(
                "{wanted} does not take {level} — reasoning effort is back to the model's default"
            )));
        }
    }

    /// Open the model overlay, or say why there is nothing to open.
    ///
    /// A provider that published no list is not an empty menu: it is a session
    /// where keke has no grounds to refuse any name, so the person is told to
    /// type one rather than shown a box with nothing in it.
    pub fn open_model_picker(&mut self) {
        if self.models.is_empty() {
            self.transcript.push(Cell::Notice(self.model_list()));
            return;
        }
        let mut picker = crate::picker::ModelPicker::default();
        if let Some(at) = self.models.iter().position(|model| model.id == self.model) {
            picker.move_selection(at as isize, self.models.len());
        }
        self.picker = Some(picker);
    }

    #[must_use]
    pub fn model_picker(&self) -> Option<&crate::picker::ModelPicker> {
        self.picker.as_ref()
    }

    /// The rows the overlay is showing this frame, after its filter.
    #[must_use]
    pub fn picker_models(&self) -> Vec<&keke_provider_api::ModelInfo> {
        let Some(picker) = &self.picker else {
            return Vec::new();
        };
        self.models
            .iter()
            .filter(|model| picker.matches(model))
            .collect()
    }

    /// Which row of [`App::picker_models`] is highlighted.
    #[must_use]
    pub fn picker_selected(&self) -> usize {
        let count = self.picker_models().len();
        self.picker
            .as_ref()
            .map_or(0, |picker| picker.selected(count))
    }

    pub(crate) fn move_picker_selection(&mut self, delta: isize) {
        let count = self.picker_models().len();
        if let Some(picker) = &mut self.picker {
            picker.move_selection(delta, count);
        }
    }

    pub(crate) fn type_into_picker(&mut self, ch: char) {
        if let Some(picker) = &mut self.picker {
            picker.push(ch);
        }
    }

    pub(crate) fn backspace_in_picker(&mut self) {
        if let Some(picker) = &mut self.picker {
            picker.backspace();
        }
    }

    /// Switch to the highlighted model and close. A filter that matches
    /// nothing accepts nothing — there is no row under the cursor to mean.
    pub(crate) fn accept_picker(&mut self) {
        let wanted = self
            .picker_models()
            .get(self.picker_selected())
            .map(|model| model.id.clone());
        if let Some(wanted) = wanted {
            self.close_picker();
            self.set_model_aloud(&wanted);
        }
    }

    pub(crate) fn close_picker(&mut self) {
        self.picker = None;
    }

    /// What `/model` says when there is no list to open.
    fn model_list(&self) -> String {
        if self.models.is_empty() {
            return format!(
                "model: {}\n\nThis provider published no model list, so there is nothing to \n\
                 choose between here. `/model <id>` still switches to whatever you name.",
                if self.model.is_empty() {
                    "(unset)"
                } else {
                    &self.model
                }
            );
        }
        let mut text = String::from("models:");
        for model in &self.models {
            let current = if model.id == self.model { "*" } else { " " };
            text.push_str(&format!(
                "\n {current} {} ({})",
                model.display_name, model.id
            ));
            if let Some(window) = model.context_window {
                text.push_str(&format!("  ·  {}k context", window / 1_000));
            }
            if model.supports_reasoning() {
                let levels: Vec<&str> = model
                    .reasoning_efforts
                    .iter()
                    .map(|effort| effort.as_str())
                    .collect();
                text.push_str(&format!("  ·  effort: {}", levels.join(", ")));
            }
            if let Some(description) = &model.description {
                text.push_str(&format!("\n      {description}"));
            }
        }
        text.push_str("\n\n/model <id> switches; /effort sets how hard it thinks.");
        text
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
            SlashAction::Builtin(Builtin::New) => self.start_new_session(),
            SlashAction::Builtin(Builtin::Quit) => self.should_quit = true,
            SlashAction::Builtin(Builtin::Copy) => self.copy_last_reply(),
            SlashAction::Builtin(Builtin::Effort) => match crate::slash::effort(arguments) {
                Ok(Some(effort)) => self.set_reasoning_effort_aloud(effort),
                Ok(None) => {
                    let next = crate::slash::next_effort(self.effort, &self.offered_efforts());
                    self.set_reasoning_effort_aloud(next);
                }
                Err(unknown) => self.transcript.push(Cell::Error(unknown)),
            },
            SlashAction::Builtin(Builtin::Model) => {
                let wanted = arguments.trim().to_string();
                if wanted.is_empty() {
                    self.open_model_picker();
                } else {
                    self.set_model_aloud(&wanted);
                }
            }
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
        let mut text = String::from(
            "keys:\n  ctrl-o — expand or collapse the newest thought or run of calls\n  \
             ctrl-t — show or hide reasoning\n  \
             drag to select and copy; click a tool call to expand it\n\ncommands:",
        );
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
