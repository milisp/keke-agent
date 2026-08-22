//! The state a terminal draws, and the only place it changes.
//!
//! Nothing here touches a terminal, so every rule the interface has — what a
//! denial looks like, when Ctrl-C quits, whether new output moves the view —
//! is assertable without a backend.

use std::sync::Arc;

use keke_acp::Conversation;
use keke_acp::PermissionAnswer;
use keke_acp::PermissionId;
use keke_acp::Update;
use keke_protocol::StopReason;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;

use crate::input::InputBox;
use crate::login::Notice;
use crate::scroll::Scrollback;
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
    turn: Turn,
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
                turn: Turn::Idle,
                show_thinking: true,
                should_quit: false,
            },
            local_updates,
        )
    }

    pub fn turn(&self) -> Turn {
        self.turn
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
                self.turn = Turn::Running;
                self.transcript.seal();
            }
            Update::TextDelta(text) => {
                self.turn = Turn::Running;
                self.transcript.push_text_delta(&text);
            }
            Update::ThinkingDelta(text) => {
                self.turn = Turn::Running;
                self.transcript.push_thinking_delta(&text);
            }
            Update::ToolCallStarted(call) => {
                self.turn = Turn::Running;
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
            Update::PermissionRequested { id, call, reason } => {
                self.turn = Turn::AwaitingPermission;
                self.transcript.request_permission(id, &call, reason);
            }
            Update::TurnEnded(reason) => {
                self.turn = Turn::Idle;
                self.transcript.seal();
                if let StopReason::Refusal { message } = reason {
                    self.transcript
                        .push(Cell::Error(format!("refused: {message}")));
                }
            }
            Update::Failed(message) => {
                // Deliberately does not quit: the seam promises the
                // conversation survives a failed turn.
                self.turn = Turn::Idle;
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
        self.transcript.push(Cell::User(text.clone()));
        // Submitting is an intent to watch the answer, so it returns to live.
        self.scroll.follow();
        self.turn = Turn::Running;

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
            self.turn = Turn::Idle;
        } else {
            self.should_quit = true;
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

    pub fn open_permission_id(&self) -> Option<PermissionId> {
        self.transcript
            .open_permission()
            .map(|prompt| prompt.id.clone())
    }
}
