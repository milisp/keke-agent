//! The seam every surface talks through.
//!
//! A surface — the TUI, an editor, a script — drives a conversation and renders
//! its updates. It must not care whether the agent is in this process or across
//! a pipe, because the same TUI has to work both ways: attached to a local
//! session, and attached to an agent running somewhere else.
//!
//! So the surface sees [`Conversation`] and [`Update`], never a transport. ACP
//! is one implementation of this trait; [`ScriptedConversation`] is another, and
//! it is what lets a surface be built and tested before any transport exists.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use keke_config_types::ApprovalPolicy;
use keke_protocol::Message;
use keke_protocol::ReasoningEffort;
use keke_protocol::StopReason;
use keke_protocol::ToolCall;
use keke_protocol::ToolResult;
use keke_protocol::Usage;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;

/// A boxed future; [`Conversation`] is always held as `dyn`.
pub type ConversationFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Why a conversation could not carry out a request.
#[derive(Debug, thiserror::Error)]
pub enum ConversationError {
    /// The agent went away — the process exited, or the pipe closed.
    #[error("the agent is no longer reachable: {0}")]
    Disconnected(String),
    /// A turn is already running. Surfaces queue rather than interleave, so
    /// this is a programming error rather than something to show a person.
    #[error("a turn is already in progress")]
    Busy,
    #[error("{0}")]
    Agent(String),
}

/// What a surface renders.
///
/// Deliberately narrower than the engine's own event set: a surface needs what
/// to draw, not the durable record. Anything a surface must be able to
/// reconstruct after a restart belongs in the session log, not here.
#[derive(Clone, Debug, PartialEq)]
pub enum Update {
    /// A turn began. Surfaces use this to clear the "thinking" state.
    TurnStarted,
    /// Visible assistant text.
    TextDelta(String),
    /// Reasoning the model chose to expose, which a surface may hide.
    ThinkingDelta(String),
    ToolCallStarted(ToolCall),
    ToolCallEnded(ToolResult),
    /// Approval is needed before a tool runs. A surface answers with
    /// [`Conversation::respond_to_permission`].
    PermissionRequested {
        id: PermissionId,
        call: ToolCall,
        reason: String,
    },
    /// One model step's token accounting. A surface shows what the turn is
    /// costing while it is still running, which is while a person can still
    /// decide to stop it.
    TokensUsed(Usage),
    TurnEnded(StopReason),
    /// The turn failed. The conversation stays usable.
    Failed(String),
}

/// A conversation that is open and ready to be prompted.
///
/// Carries the id keke filed the session under rather than one the surface
/// invented: the ACP session id and the rollout log's name are the same string,
/// so an id a client saw in `session/list` is one it can resume.
pub struct Opened {
    pub id: String,
    /// The model this session is asking, and every model the surface may offer
    /// instead. Empty when the provider could not be asked — a surface then
    /// offers no choice rather than a wrong one.
    pub model: String,
    pub models: Vec<String>,
    pub conversation: Arc<dyn Conversation>,
    pub updates: UnboundedReceiver<Update>,
    /// What the session was rebuilt from. Empty for a session that is new.
    pub history: Vec<Message>,
}

/// One previous session, as `session/list` reports it.
///
/// Deliberately flat strings: this crosses the seam from whoever keeps the
/// sessions to the protocol, and neither side should have to agree on a
/// storage type to describe one.
pub struct SessionListing {
    pub id: String,
    /// Where the session was started. The lister substitutes its own working
    /// directory for a log that does not say.
    pub cwd: std::path::PathBuf,
    /// What the person opened with, for telling two sessions apart.
    pub title: String,
    /// RFC 3339, from the last thing written to the log.
    pub updated_at: String,
}

/// Identifies one outstanding permission request.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PermissionId(pub String);

/// A person's answer to a [`Update::PermissionRequested`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionAnswer {
    Allow,
    /// Allow this and every later call of the same shape, for this session.
    AllowAlways,
    Deny,
}

/// A live conversation with an agent.
pub trait Conversation: Send + Sync {
    /// Send a prompt and start a turn.
    fn prompt<'a>(&'a self, text: String) -> ConversationFuture<'a, Result<(), ConversationError>>;

    /// Ask the agent to stop the running turn.
    ///
    /// Cooperative and idempotent: cancelling an idle conversation is not an
    /// error, because a person pressing Ctrl-C twice should not see one.
    fn cancel(&self);

    /// Answer an outstanding permission request.
    fn respond_to_permission(&self, id: &PermissionId, answer: PermissionAnswer);

    /// Change how much the agent may do without asking.
    ///
    /// A person switching modes mid-conversation is answering about the work in
    /// front of them, so this is on the seam rather than in the session's
    /// startup configuration: a surface that offers the switch must be able to
    /// make it whatever the agent is attached to.
    fn set_approval_policy(&self, policy: ApprovalPolicy);

    /// Change how hard the model is asked to think.
    ///
    /// On the seam for the same reason the approval policy is: the level is a
    /// setting a person changes while the conversation runs, and `None` means
    /// "unset, let the model decide" rather than the lowest rung.
    fn set_reasoning_effort(&self, effort: Option<ReasoningEffort>);

    /// Change which model answers, within the provider the session was built
    /// with. On the seam for the same reason the two settings above are: a
    /// person switching models is talking about the next answer.
    ///
    /// Naming a model the provider does not serve is the provider's to refuse,
    /// not this seam's to guess at.
    fn set_model(&self, model: String);
}

/// A conversation that replays a prepared script.
///
/// Exists so a surface can be built and tested against the seam before any
/// transport implements it, and so its tests need no agent, no model, and no
/// network.
pub struct ScriptedConversation {
    updates: UnboundedSender<Update>,
    script: Mutex<Vec<Vec<Update>>>,
    /// Prompts received, so a test can assert what the surface sent.
    prompts: Arc<Mutex<Vec<String>>>,
    answers: Arc<Mutex<Vec<(PermissionId, PermissionAnswer)>>>,
    cancels: Arc<Mutex<usize>>,
    policies: Arc<Mutex<Vec<ApprovalPolicy>>>,
    efforts: Arc<Mutex<Vec<Option<ReasoningEffort>>>>,
    models: Arc<Mutex<Vec<String>>>,
}

impl ScriptedConversation {
    /// One entry per prompt, replayed in order.
    #[must_use]
    pub fn new(script: Vec<Vec<Update>>) -> (Self, UnboundedReceiver<Update>) {
        let (updates, receiver) = tokio::sync::mpsc::unbounded_channel();
        (
            Self {
                updates,
                script: Mutex::new(script),
                prompts: Arc::new(Mutex::new(Vec::new())),
                answers: Arc::new(Mutex::new(Vec::new())),
                cancels: Arc::new(Mutex::new(0)),
                policies: Arc::new(Mutex::new(Vec::new())),
                efforts: Arc::new(Mutex::new(Vec::new())),
                models: Arc::new(Mutex::new(Vec::new())),
            },
            receiver,
        )
    }

    #[must_use]
    pub fn prompts(&self) -> Vec<String> {
        self.prompts
            .lock()
            .map(|seen| seen.clone())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn answers(&self) -> Vec<(PermissionId, PermissionAnswer)> {
        self.answers
            .lock()
            .map(|seen| seen.clone())
            .unwrap_or_default()
    }

    /// Every policy the surface has asked for, in order.
    #[must_use]
    pub fn policies(&self) -> Vec<ApprovalPolicy> {
        self.policies
            .lock()
            .map(|seen| seen.clone())
            .unwrap_or_default()
    }

    /// Every model the surface has asked for, in order.
    #[must_use]
    pub fn models(&self) -> Vec<String> {
        self.models
            .lock()
            .map(|seen| seen.clone())
            .unwrap_or_default()
    }

    /// Every effort level the surface has asked for, in order.
    #[must_use]
    pub fn efforts(&self) -> Vec<Option<ReasoningEffort>> {
        self.efforts
            .lock()
            .map(|seen| seen.clone())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn cancel_count(&self) -> usize {
        self.cancels.lock().map(|count| *count).unwrap_or_default()
    }

    /// Push an update outside the script, for a test driving timing by hand.
    pub fn emit(&self, update: Update) {
        let _ = self.updates.send(update);
    }
}

impl Conversation for ScriptedConversation {
    fn prompt<'a>(&'a self, text: String) -> ConversationFuture<'a, Result<(), ConversationError>> {
        Box::pin(async move {
            if let Ok(mut seen) = self.prompts.lock() {
                seen.push(text);
            }
            let turn = match self.script.lock() {
                Ok(mut script) if !script.is_empty() => script.remove(0),
                // An exhausted script answers rather than hanging, so a test
                // that forgot an entry fails in its assertion.
                _ => vec![
                    Update::TurnStarted,
                    Update::Failed("nothing scripted for this prompt".to_string()),
                ],
            };
            for update in turn {
                let _ = self.updates.send(update);
            }
            Ok(())
        })
    }

    fn cancel(&self) {
        if let Ok(mut count) = self.cancels.lock() {
            *count += 1;
        }
    }

    fn respond_to_permission(&self, id: &PermissionId, answer: PermissionAnswer) {
        if let Ok(mut seen) = self.answers.lock() {
            seen.push((id.clone(), answer));
        }
    }

    fn set_approval_policy(&self, policy: ApprovalPolicy) {
        if let Ok(mut seen) = self.policies.lock() {
            seen.push(policy);
        }
    }

    fn set_reasoning_effort(&self, effort: Option<ReasoningEffort>) {
        if let Ok(mut seen) = self.efforts.lock() {
            seen.push(effort);
        }
    }

    fn set_model(&self, model: String) {
        if let Ok(mut seen) = self.models.lock() {
            seen.push(model);
        }
    }
}

#[cfg(test)]
mod tests {
    use keke_protocol::ToolCallId;

    use super::*;

    fn call() -> ToolCall {
        ToolCall {
            id: ToolCallId::new("c1"),
            name: "read_file".to_string(),
            arguments: serde_json::Value::Null,
        }
    }

    #[tokio::test]
    async fn a_scripted_turn_replays_in_order() {
        let (conversation, mut updates) = ScriptedConversation::new(vec![vec![
            Update::TurnStarted,
            Update::TextDelta("hi".to_string()),
            Update::TurnEnded(StopReason::EndTurn),
        ]]);

        conversation
            .prompt("hello".to_string())
            .await
            .expect("prompts");

        let mut seen = Vec::new();
        while let Ok(update) = updates.try_recv() {
            seen.push(update);
        }
        assert_eq!(
            seen,
            vec![
                Update::TurnStarted,
                Update::TextDelta("hi".to_string()),
                Update::TurnEnded(StopReason::EndTurn),
            ]
        );
        assert_eq!(conversation.prompts(), vec!["hello".to_string()]);
    }

    /// A surface under test must fail in its assertion rather than wait forever
    /// for a turn nobody scripted.
    #[tokio::test]
    async fn an_exhausted_script_answers_rather_than_hanging() {
        let (conversation, mut updates) = ScriptedConversation::new(Vec::new());
        conversation
            .prompt("hello".to_string())
            .await
            .expect("prompts");

        let mut seen = Vec::new();
        while let Ok(update) = updates.try_recv() {
            seen.push(update);
        }
        assert!(matches!(seen.last(), Some(Update::Failed(_))), "{seen:?}");
    }

    #[tokio::test]
    async fn permission_answers_and_cancels_are_observable() {
        let (conversation, _updates) =
            ScriptedConversation::new(vec![vec![Update::PermissionRequested {
                id: PermissionId("p1".to_string()),
                call: call(),
                reason: "reads outside the workspace".to_string(),
            }]]);

        conversation
            .prompt("go".to_string())
            .await
            .expect("prompts");
        conversation.respond_to_permission(&PermissionId("p1".to_string()), PermissionAnswer::Deny);
        conversation.cancel();
        conversation.cancel();

        assert_eq!(
            conversation.answers(),
            vec![(PermissionId("p1".to_string()), PermissionAnswer::Deny)]
        );
        // Cancelling twice is what a person pressing Ctrl-C twice does, and it
        // must not be an error.
        assert_eq!(conversation.cancel_count(), 2);
    }
}
