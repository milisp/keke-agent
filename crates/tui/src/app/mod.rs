//! The state a terminal draws, and the only place it changes.
//!
//! Nothing here touches a terminal, so every rule the interface has — what a
//! denial looks like, when Ctrl-C quits, whether new output moves the view —
//! is assertable without a backend.

mod commands;
mod completion;
mod picker_overlay;
pub(crate) mod plan;
mod rewind;
mod session;
mod subagents;

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
    /// The MCP servers, as the host described them at startup.
    mcp: Vec<crate::mcp::McpServerStatus>,
    /// How to sign in to one, when the host can. `None` in a surface with no
    /// credential store — `/mcp` then still lists, it just cannot authorize.
    sign_in: Option<Arc<dyn crate::mcp::McpSignIn>>,
    /// What a login is doing, per server, while it is doing it. Keyed by name
    /// so a report arriving late lands on the row it is about rather than on
    /// whichever row happens to be highlighted.
    mcp_activity: std::collections::HashMap<String, String>,
    /// Toggling, removing, and refreshing a server from `/mcp`. `None` in a
    /// surface with no way to write `.mcp.json` back.
    manage: Option<Arc<dyn crate::mcp::McpManage>>,
    /// Where a login flow's progress goes. Held so a sign-in started from
    /// `/mcp` reaches the transcript the same way a startup login's does.
    notices: Option<UnboundedSender<Notice>>,
    /// `@`-completion: fuzzy file/folder search over the current line.
    pub file_search: FileSearchState,
    /// Standing prompts from `/loop`. Held by the surface, not the session:
    /// a loop is a person's instruction to keep asking, and it ends with the
    /// window it was typed into.
    pub(crate) schedule: crate::schedule::Scheduler,
    /// What was typed in this project before, and where the arrow keys are
    /// within it.
    pub history: PromptHistory,
    /// Which completion the arrow keys are on. Clamped rather than reset on
    /// every keystroke, so typing one more letter does not jump the highlight
    /// back to the top of a list the person was already moving through.
    completion: usize,
    /// When Esc was last pressed with nothing to interrupt, if it is still
    /// waiting for its second tap. `None` is the ordinary state: Esc is the
    /// key for "never mind", so the first press does nothing but arm.
    esc_armed: Option<Instant>,
    /// When a rewind last put a prompt back in the composer. Enter is
    /// swallowed for a moment after that: the same key carries out the rewind
    /// and sends the composer, and a terminal that repeats a held Enter — or a
    /// person who taps it twice — would fire the words straight back off
    /// unedited, which is the one thing a rewind is for avoiding.
    pub(crate) rewound_at: Option<Instant>,
    /// The rewind overlay, while one is open. It holds the keyboard, the way
    /// the model picker does.
    rewind: Option<crate::rewind::Rewind>,
    /// The model or provider overlay, while one is open. `None` is the
    /// ordinary state: the composer has the keyboard.
    picker: Option<crate::picker::Picker>,
    /// What is being done to the plan in the scrollback, while one is waiting
    /// for an answer: which lines are selected, what has been said about them,
    /// and which policy would carry it out. The plan itself is a cell like any
    /// other; this is only the reading of it.
    plan: Option<plan::PlanReview>,
    /// Set by `/view-plan`, cleared by the frame that acts on it: bring the
    /// last plan back on screen. A flag rather than a scroll because only the
    /// frame knows which line the plan came out on.
    show_last_plan: bool,
    approval: ApprovalPolicy,
    /// Whether the session is planning. Held rather than derived because the
    /// agent can enter and leave plan mode on its own, so the only truthful
    /// source is what the seam last said.
    mode: keke_config_types::SessionMode,
    /// How hard the model is asked to think. `None` is the vendor's own
    /// default, which is a state of its own and not the lowest rung.
    effort: Option<ReasoningEffort>,
    /// Which queue the vendor is being asked to answer from. `None` names
    /// none, which is a state of its own and not the standard queue.
    tier: Option<keke_config_types::ServiceTier>,
    /// Which model is answering, and every model this session's provider
    /// serves. The list is empty when the provider could not be asked and had
    /// nothing to fall back on; `/model` then says so rather than showing an
    /// empty menu.
    model: String,
    /// The route serving `model`. Persisted with it so config.toml always
    /// holds a pair that existed.
    provider: Option<String>,
    /// The route `conversation` was actually built with. Unlike `provider`,
    /// `/provider` never touches this: the running conversation keeps
    /// answering through the route it was handed, so this is what tells
    /// `/model` whether a name it is about to switch to would land on the
    /// provider actually serving this turn, or on one only config.toml knows
    /// about yet.
    launched_provider: Option<String>,
    /// Every route this build has registered, so `/provider` can list them and
    /// refuse a name none of them answers to. Empty when the host did not say,
    /// and then nothing is refused — keke has no grounds to.
    routes: Vec<crate::picker::ProviderChoice>,
    models: Vec<keke_provider_api::ModelInfo>,
    turn: Turn,
    /// When the running turn started, and how long the last one took. Both are
    /// held because the status bar keeps showing the duration after the turn
    /// ends: "worked for 12s" is what a person looks for once the answer is on
    /// screen and they have stopped watching the clock.
    started: Option<Instant>,
    last_turn: Option<Duration>,
    /// Wall-clock time the last turn ended, for "done 10:41 PM" beside the
    /// elapsed duration — `Instant` has no wall-clock reading of its own.
    last_turn_finished_at: Option<chrono::DateTime<chrono::Local>>,
    /// Tokens this session has spent, including whatever a resumed log already
    /// accounted for.
    usage: Usage,
    /// The input tokens of the most recent model step. Unlike the additive
    /// `usage`, each request resends the whole conversation, so a step's
    /// `input_tokens` is not an increment — it *is* the current context size.
    context_input: u64,
    /// Whether the model is currently emitting a reasoning delta. Not kept as
    /// transcript text — only `turn_status` reads it, to show "thought" in
    /// place of "working" while it is true.
    thinking: bool,
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
    /// The subagents currently worth drawing, as the agent last reported them.
    /// Replaced wholesale rather than merged: the agent sends whole snapshots
    /// precisely so this cannot drift.
    subagents: Vec<keke_acp::SubagentView>,
    /// Background commands, as the session last reported them. A whole
    /// snapshot each time, so nothing here has to be reconciled.
    tasks: Vec<keke_acp::TaskView>,
    /// When each subagent id was first seen here. The duration on a row is
    /// measured against this rather than against a timestamp the agent sends,
    /// because a surface across a pipe has no shared clock to compare with —
    /// and what a person wants to know is how long they have been waiting.
    subagent_since: std::collections::HashMap<String, Instant>,
    /// Where this frame drew each subagent row, as `(row, agent id)`, for the
    /// same reason `toggles` exists: a click is a screen position and nothing
    /// else.
    subagent_rows: Vec<(u16, String)>,
    /// Which subagent's task is open in full. Cleared when that subagent goes.
    subagent_detail: Option<String>,
    /// A word about something keke just did for the person at the keyboard —
    /// copied, resumed. It goes in the status bar and expires, never into
    /// the transcript: the transcript is the conversation, and a line in it
    /// reads as something the agent said.
    flash: Option<(String, Instant)>,
    /// Text waiting to go to the clipboard. Held rather than written here so
    /// the state tests never touch a terminal.
    pending_copy: Option<String>,
    /// A plan file waiting to be opened in the person's editor. Held rather
    /// than spawned here for the same reason as `pending_copy`: only the
    /// event loop owns the terminal's raw mode, so only it can suspend it.
    pending_edit: Option<std::path::PathBuf>,
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
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let transcript = keke_paths::AbsPath::new(&cwd)
            .map(|cwd| Transcript::with_cwd(&cwd))
            .unwrap_or_default();
        (
            Self {
                conversation,
                local,
                transcript,
                input: InputBox::default(),
                scroll: Scrollback::default(),
                commands: SlashCommands::default(),
                mcp: Vec::new(),
                mcp_activity: std::collections::HashMap::new(),
                manage: None,
                sign_in: None,
                notices: None,
                file_search: FileSearchState::new(cwd),
                schedule: crate::schedule::Scheduler::default(),
                history: PromptHistory::default(),
                completion: 0,
                esc_armed: None,
                rewound_at: None,
                rewind: None,
                picker: None,
                plan: None,
                show_last_plan: false,
                approval: ApprovalPolicy::default(),
                mode: keke_config_types::SessionMode::default(),
                effort: None,
                tier: None,
                model: String::new(),
                provider: None,
                launched_provider: None,
                routes: Vec::new(),
                models: Vec::new(),
                turn: Turn::Idle,
                started: None,
                last_turn: None,
                last_turn_finished_at: None,
                usage: Usage::default(),
                context_input: 0,
                thinking: false,
                mouse_capture: true,
                follow_button: None,
                expanded: std::collections::HashSet::new(),
                toggles: Vec::new(),
                subagents: Vec::new(),
                tasks: Vec::new(),
                subagent_since: std::collections::HashMap::new(),
                subagent_rows: Vec::new(),
                subagent_detail: None,
                flash: None,
                pending_copy: None,
                pending_edit: None,
                selection: crate::selection::Selection::default(),
                should_quit: false,
                config_home: None,
            },
            local_updates,
        )
    }

    /// The MCP servers `/mcp` reports, and how to authorize one.
    #[must_use]
    pub fn with_mcp(
        mut self,
        servers: Vec<crate::mcp::McpServerStatus>,
        sign_in: Option<Arc<dyn crate::mcp::McpSignIn>>,
        manage: Option<Arc<dyn crate::mcp::McpManage>>,
    ) -> Self {
        self.mcp = servers;
        self.sign_in = sign_in;
        self.manage = manage;
        self
    }

    /// Where login progress is sent, for flows the interface starts itself.
    #[must_use]
    pub fn with_notices(mut self, notices: UnboundedSender<Notice>) -> Self {
        self.notices = Some(notices);
        self
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

    /// The three-line startup banner, once, at the top of the scrollback.
    ///
    /// A surface call, not part of `new`: state tests build an `App` and
    /// expect an empty transcript to assert against, and a banner naming a
    /// real version and shelling out to `git` has nothing to do with that.
    #[must_use]
    pub fn with_banner(mut self) -> Self {
        let lines = crate::banner::startup(self.cwd());
        self.transcript.push(Cell::Banner(lines));
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
    pub fn with_session_mode(mut self, mode: keke_config_types::SessionMode) -> Self {
        self.mode = mode;
        self
    }

    /// The approval policy the session was started under.
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

    /// The queue the session was configured to be answered from, so the bar
    /// and `/fast` start from what is in force rather than from a guess.
    #[must_use]
    pub fn with_service_tier(mut self, tier: Option<keke_config_types::ServiceTier>) -> Self {
        self.tier = tier;
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
        provider: impl Into<String>,
        model: impl Into<String>,
        models: Vec<keke_provider_api::ModelInfo>,
    ) -> Self {
        let provider = provider.into();
        self.launched_provider = Some(provider.clone());
        self.provider = Some(provider);
        self.model = model.into();
        self.models = models;
        self
    }

    /// Every provider instance this build registered, so `/provider` lists
    /// what a person could point the next session at.
    ///
    /// From the composition root for the same reason the model list is: only it
    /// has a registry, and a surface handed nothing offers no choice rather
    /// than a wrong one.
    #[must_use]
    pub fn with_provider_routes(mut self, routes: Vec<crate::picker::ProviderChoice>) -> Self {
        self.routes = routes;
        self
    }

    /// Seed the surface from a resumed session: what was said, and what it has
    /// already spent.
    ///
    /// The transcript is rebuilt from the same history the engine resumes with,
    /// so what a person reads on screen is what the model is about to be sent —
    /// a summary written separately would be free to drift from it.
    #[must_use]
    pub fn with_history(
        mut self,
        history: &[keke_protocol::Message],
        usage: Usage,
        context_input: u64,
    ) -> Self {
        self.transcript.replay(history);
        self.usage = usage;
        self.context_input = context_input;
        self
    }

    pub fn approval_policy(&self) -> ApprovalPolicy {
        self.approval
    }

    #[must_use]
    pub fn session_mode(&self) -> keke_config_types::SessionMode {
        self.mode
    }

    pub fn turn(&self) -> Turn {
        self.turn
    }

    /// What this session has spent so far.
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// Input tokens of the most recent model call: how full the context
    /// window is right now, not what the session has cumulatively spent.
    pub fn context_input(&self) -> u64 {
        self.context_input
    }

    /// How long the current turn has been running, or how long the last one
    /// took. `None` before the first turn.
    pub fn elapsed(&self) -> Option<Duration> {
        match self.started {
            Some(started) => Some(started.elapsed()),
            None => self.last_turn,
        }
    }

    /// Wall-clock time the last turn finished, for the idle status row.
    /// `None` while a turn is running or before the first one has ended.
    pub fn last_turn_finished_at(&self) -> Option<chrono::DateTime<chrono::Local>> {
        self.last_turn_finished_at
    }

    /// Whether a turn is on the clock, so the caller redraws on a timer rather
    /// than only when something arrives.
    /// Whether anything on screen changes on its own. A flash counts only
    /// while it is live: an expired one must not keep an idle interface
    /// redrawing forever.
    pub fn is_timing(&self) -> bool {
        self.started.is_some() || self.flash().is_some() || self.file_search.is_open()
    }

    /// How long the event loop may block before it must look at the app again.
    ///
    /// `None` means nothing is moving and it may block indefinitely — an idle
    /// session with no loops must not wake the terminal on a timer, which is
    /// what a person notices on a laptop.
    #[must_use]
    pub fn next_wakeup(&self, tick: Duration) -> Option<Duration> {
        let timing = self.is_timing().then_some(tick);
        // A due loop that cannot fire yet must not be woken for: while a turn
        // holds it up, waking on its deadline is a spin at zero delay. The
        // turn's own tick is what brings us back to look again.
        let due = self.schedule.until_due(Instant::now()).map(|due| {
            if self.turn.is_busy() {
                due.max(tick)
            } else {
                due
            }
        });
        match (timing, due) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (only, None) | (None, only) => only,
        }
    }

    /// Fire the loop that is due, if the agent is free to answer it.
    ///
    /// Called from the event loop on every wakeup. A due loop waits while a
    /// turn is running rather than interrupting it: two prompts in flight
    /// means one of them is answered with the other's context.
    pub fn fire_due_schedules(&mut self) {
        for id in self.schedule.expire(Instant::now()) {
            self.transcript.push(Cell::Notice(format!(
                "loop {id} expired after a week and has stopped"
            )));
        }
        if self.turn.is_busy() {
            return;
        }
        let Some((id, prompt)) = self.schedule.take_due(Instant::now()) else {
            return;
        };
        self.transcript
            .push(Cell::Notice(format!("loop {id} firing")));
        self.transcript.push(Cell::User(prompt.clone()));
        self.send_text(prompt);
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

    /// Whether the model is mid-reasoning right now, for `turn_status` to
    /// show in place of "working".
    pub fn is_thinking(&self) -> bool {
        self.thinking
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
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
            Update::ModeChanged(mode) => {
                self.mode = mode;
            }
            Update::TextDelta(text) => {
                self.begin_turn();
                self.thinking = false;
                self.transcript.push_text_delta(&text);
            }
            Update::ThinkingDelta(_) => {
                self.begin_turn();
                self.thinking = true;
            }
            Update::ToolCallStarted(call) => {
                self.begin_turn();
                self.thinking = false;
                self.transcript.start_tool(&call);
            }
            Update::HostedToolCall { name, query } => {
                self.begin_turn();
                self.thinking = false;
                self.transcript.hosted_tool(&name, query.as_deref());
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
            Update::TokensUsed(usage) => {
                self.usage.add(usage);
                self.context_input = usage.input_tokens;
            }
            Update::PermissionRequested { id, call, reason } => {
                self.turn = Turn::AwaitingPermission;
                // A plan is not a tool prompt with a plan attached: it is the
                // plan, so it goes into the scrollback as one.
                if call.name == plan::EXIT_PLAN_MODE {
                    self.open_plan_review(id, &call);
                } else {
                    self.transcript.request_permission(id, &call, reason);
                }
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
            Update::Subagents(rows) => {
                self.set_subagents(rows);
            }
            Update::Tasks(rows) => self.tasks = rows,
            Update::RewindPoints(points) => self.offer_rewind_points(points),
            Update::RewindPreview { turn, files } => self.preview_rewind(turn, files),
            Update::Rewound(rewound) => self.report_rewind(&rewound),
            Update::SessionReset => {
                self.transcript.clear();
                self.set_subagents(Vec::new());
                self.scroll.follow();
                self.usage = Usage::default();
                self.context_input = 0;
            }
        }
    }

    /// Show something the host wants said without printing over the interface.
    ///
    /// These go in the transcript because a login URL or a device code has to
    /// stay put long enough to be read off the screen and typed elsewhere.
    pub fn apply_notice(&mut self, notice: Notice) {
        // An MCP login reports onto its own row. While the overlay is open the
        // person is already looking at that row, so saying it again in the
        // transcript is noise in the conversation about something that is not
        // part of it.
        match &notice {
            Notice::SignedIn(name) => {
                if let Some(server) = self.mcp.iter_mut().find(|server| &server.name == name) {
                    server.signed_in = true;
                }
                self.mcp_activity.remove(name);
            }
            Notice::McpProgress { name, message } => {
                self.mcp_activity.insert(name.clone(), message.clone());
            }
            _ => {}
        }
        if matches!(notice, Notice::SignedIn(_) | Notice::McpProgress { .. })
            && self.mcp_picker().is_some()
        {
            return;
        }
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
        // A loop was written against the conversation it was typed into;
        // carrying it over would keep asking a question about work the fresh
        // session has no record of.
        if !self.schedule.is_empty() {
            self.transcript
                .push(Cell::Notice("loops stopped with the session".to_string()));
            self.schedule.clear();
        }
        let conversation = Arc::clone(&self.conversation);
        let local = self.local.clone();
        tokio::spawn(async move {
            if let Err(error) = conversation.new_session().await {
                let _ = local.send(Update::Failed(error.to_string()));
            }
        });
    }

    /// The background commands this session has started, oldest first.
    #[must_use]
    pub fn tasks(&self) -> &[keke_acp::TaskView] {
        &self.tasks
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
        self.thinking = false;
        if let Some(started) = self.started.take() {
            self.last_turn = Some(started.elapsed());
            self.last_turn_finished_at = Some(chrono::Local::now());
        }
    }

    /// Answer the prompt currently blocking the turn.
    pub fn answer_permission(&mut self, answer: PermissionAnswer) {
        self.answer_permission_with_note(answer, None);
    }

    /// Answer, saying something about it.
    ///
    /// The note rides with the answer rather than following as a prompt: the
    /// turn is parked on this question, so a prompt sent now would be queued
    /// behind the rest of the turn and arrive after the work it was meant to
    /// shape.
    pub fn answer_permission_with_note(&mut self, answer: PermissionAnswer, note: Option<String>) {
        let Some(id) = self.open_permission_id() else {
            return;
        };
        self.conversation.respond_to_permission(&id, answer, note);
        self.transcript.answer_permission(&id, answer);
        // Denial ends nothing by itself: the agent decides what to do next.
        self.turn = Turn::Running;
    }

    pub fn open_permission_id(&self) -> Option<PermissionId> {
        self.transcript.open_permission_id()
    }

    pub fn open_permission(&self) -> Option<&crate::transcript::PermissionCell> {
        self.transcript.open_permission()
    }
}
