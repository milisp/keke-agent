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
use keke_config_types::SessionMode;
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
    /// A tool the vendor ran for itself inside the model call — a hosted
    /// `web_search`, say. It arrives already resolved, so it is one update
    /// rather than a started/ended pair, and it carries no call id: there is
    /// no engine-side call for a surface to revise later.
    HostedToolCall {
        name: String,
        query: Option<String>,
    },
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
    /// Every subagent currently worth showing, oldest first. A whole snapshot
    /// rather than per-agent deltas: the list is short, and a surface that
    /// missed one delta would draw a subagent that finished long ago.
    ///
    /// Empty means nothing is outstanding, which is a surface's cue to take the
    /// section away entirely.
    Subagents(Vec<SubagentView>),
    /// The turn failed. The conversation stays usable.
    Failed(String),
    /// The session mode changed without the surface asking — the agent
    /// entered plan mode itself, or a plan was approved and it left again.
    ///
    /// A surface that only tracked its own toggle would keep drawing `plan`
    /// after the agent had already been let out of it.
    ModeChanged(SessionMode),
    /// [`Conversation::new_session`] finished: history and usage are gone,
    /// and whatever the surface shows for either should go back to nothing.
    SessionReset,
}

/// One running subagent, as a client sees it.
///
/// Flat fields rather than the engine's own progress type, for the reason
/// [`PluginCommand`] is flat: this crosses the seam to a surface that may be on
/// the other end of a pipe, and everything here is something to draw rather
/// than something to act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentView {
    /// The handle the agent named it, which is also what a person sees if they
    /// go looking for it in the transcript.
    pub id: String,
    /// What it was asked to do, in full. A surface truncates for its row and
    /// still has the whole thing to show on demand.
    pub task: String,
    /// `completed`, `failed`, `timed_out`, `cancelled` — or `None` while it is
    /// still running.
    pub status: Option<String>,
    /// How full the child's context window is, in input tokens of its last
    /// model step.
    pub input_tokens: u64,
}

/// A conversation that is open and ready to be prompted.
///
/// Carries the id keke filed the session under rather than one the surface
/// invented: the ACP session id and the rollout log's name are the same string,
/// so an id a client saw in `session/list` is one it can resume.
pub struct Opened {
    pub id: String,
    /// The model this session is asking, and every model the surface may offer
    /// instead. Empty when the provider could not be asked and had nothing to
    /// fall back on — a surface then offers no choice rather than a wrong one.
    ///
    /// Carried as [`ModelInfo`] rather than as ids because what a person picks
    /// from is a name and a ladder: a menu of slugs makes them guess which
    /// `gpt-5.6-*` is which, and hides that one of them takes an effort level
    /// the others do not.
    pub model: String,
    pub models: Vec<keke_provider_api::ModelInfo>,
    /// The level this session was configured with, so a client's picker starts
    /// on what is in force rather than on a guess.
    pub effort: Option<ReasoningEffort>,
    /// The approval policy this session was configured with, so a client's
    /// picker starts on what is in force rather than on a guess.
    pub approval_policy: ApprovalPolicy,
    /// The mode this session was configured with, for the same reason. A
    /// resumed session that was planning comes back planning.
    pub mode: SessionMode,
    pub conversation: Arc<dyn Conversation>,
    pub updates: UnboundedReceiver<Update>,
    /// What the session was rebuilt from. Empty for a session that is new.
    pub history: Vec<Message>,
    /// Plugin-contributed commands, already named for the shared namespace.
    ///
    /// The composition root resolves name collisions once — the same rule
    /// `keke_tui::slash::SlashCommands` applies for the TUI's own completion —
    /// so this is the result, not raw plugin contributions. Advertised to the
    /// client as an `AvailableCommandsUpdate` so an editor's own autocomplete
    /// can offer them, the same list keke's own TUI would show.
    pub commands: Vec<PluginCommand>,
}

/// A plugin-contributed command, ready to show a client.
///
/// Flat strings rather than the plugin's own contribution type: this crosses
/// the seam from whoever composed the plugins to the protocol, and the ACP
/// layer only ever echoes the name and description — it does not resolve or
/// run the command itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginCommand {
    pub name: String,
    pub description: String,
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

/// One authentication method the agent offers before any session exists.
///
/// Vendor-agnostic: the id is a route name (`"codex"`, `"grok"`, ...), never a
/// wire concept, so the ACP layer can describe it without knowing what vendor
/// is behind it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthMethodDescriptor {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// Whether this route already resolves a credential, so a client can say
    /// "signed in" instead of offering a login that would no-op. Advertised
    /// rather than inferred: only the factory can see the credential stores.
    pub signed_in: bool,
    /// Where that credential came from (`"oauth"`, `"env"`, another CLI's
    /// file, ...), for a client that wants to name it. `None` when the route
    /// is not signed in.
    pub source: Option<String>,
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
    ///
    /// `note` is what the person said while answering, when they said
    /// anything. It travels with the answer rather than after it: someone who
    /// approves while asking for one thing to be different is instructing the
    /// work the call is about to do, and a note sent as the next prompt would
    /// arrive once that work was already finished.
    fn respond_to_permission(
        &self,
        id: &PermissionId,
        answer: PermissionAnswer,
        note: Option<String>,
    );

    /// Change how much the agent may do without asking.
    ///
    /// A person switching modes mid-conversation is answering about the work in
    /// front of them, so this is on the seam rather than in the session's
    /// startup configuration: a surface that offers the switch must be able to
    /// make it whatever the agent is attached to.
    fn set_approval_policy(&self, policy: ApprovalPolicy);

    /// Turn plan mode on or off.
    ///
    /// On the seam for the same reason [`Self::set_approval_policy`] is: a
    /// person asking to plan first is answering about the work in front of
    /// them, and every surface must be able to ask for it whatever the agent
    /// is attached to.
    ///
    /// Asking for a mode is not the same as being in it. The agent may only
    /// be able to act on it at the next turn boundary, and it may leave the
    /// mode on its own — so what a surface *draws* comes from
    /// [`Update::ModeChanged`], never from the fact that it made this call.
    fn set_session_mode(&self, mode: SessionMode);

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

    /// Retire this conversation's history and start over, as if the process
    /// had just been launched again: empty history, usage back to zero, the
    /// same model/effort/approval a fresh launch would start with.
    ///
    /// Distinct from clearing a surface's own transcript: that is a view
    /// changing what it shows of a conversation the agent still remembers.
    /// This is the agent itself forgetting, which is why it is on the seam
    /// rather than left to a surface to fake by discarding what it drew.
    fn new_session(&self) -> ConversationFuture<'_, Result<(), ConversationError>>;

    /// Take back the `nth` thing the person said (counting from zero) and
    /// everything that followed it, answering with the words themselves so a
    /// surface can put them back in front of them to edit.
    ///
    /// On the seam rather than left to a surface, for the reason
    /// [`Self::new_session`] is: a surface that only dropped what it had drawn
    /// would show a conversation the agent still remembers, and the next turn
    /// would be answered against the very messages a person asked to take
    /// back.
    ///
    /// `None` means there is no such turn to go back to — the conversation is
    /// shorter than `nth` — and nothing was changed. Winding back to a turn
    /// that exists is not refusable: the words are the person's to withdraw.
    fn rewind(
        &self,
        nth: usize,
    ) -> ConversationFuture<'_, Result<Option<String>, ConversationError>>;
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
    answers: Arc<Mutex<Vec<(PermissionId, PermissionAnswer, Option<String>)>>>,
    cancels: Arc<Mutex<usize>>,
    policies: Arc<Mutex<Vec<ApprovalPolicy>>>,
    efforts: Arc<Mutex<Vec<Option<ReasoningEffort>>>>,
    models: Arc<Mutex<Vec<String>>>,
    modes: Arc<Mutex<Vec<SessionMode>>>,
    new_sessions: Arc<Mutex<usize>>,
    rewinds: Arc<Mutex<Vec<usize>>>,
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
                modes: Arc::new(Mutex::new(Vec::new())),
                new_sessions: Arc::new(Mutex::new(0)),
                rewinds: Arc::new(Mutex::new(Vec::new())),
            },
            receiver,
        )
    }

    /// How many times `new_session` was called, so a test can assert a
    /// person's `/new` actually reached the conversation.
    #[must_use]
    pub fn new_session_count(&self) -> usize {
        self.new_sessions.lock().map(|count| *count).unwrap_or(0)
    }

    /// Every turn the surface asked to wind back to, in order.
    #[must_use]
    pub fn rewinds(&self) -> Vec<usize> {
        self.rewinds
            .lock()
            .map(|seen| seen.clone())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn prompts(&self) -> Vec<String> {
        self.prompts
            .lock()
            .map(|seen| seen.clone())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn answers(&self) -> Vec<(PermissionId, PermissionAnswer, Option<String>)> {
        self.answers
            .lock()
            .map(|seen| seen.clone())
            .unwrap_or_default()
    }

    /// Every mode the surface has asked for, in order.
    #[must_use]
    pub fn modes(&self) -> Vec<SessionMode> {
        self.modes
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

    fn respond_to_permission(
        &self,
        id: &PermissionId,
        answer: PermissionAnswer,
        note: Option<String>,
    ) {
        if let Ok(mut seen) = self.answers.lock() {
            seen.push((id.clone(), answer, note));
        }
    }

    fn set_session_mode(&self, mode: SessionMode) {
        if let Ok(mut seen) = self.modes.lock() {
            seen.push(mode);
        }
        let _ = self.updates.send(Update::ModeChanged(mode));
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

    fn new_session(&self) -> ConversationFuture<'_, Result<(), ConversationError>> {
        Box::pin(async move {
            if let Ok(mut count) = self.new_sessions.lock() {
                *count += 1;
            }
            let _ = self.updates.send(Update::SessionReset);
            Ok(())
        })
    }

    /// Forgets the prompts from `nth` on, so `prompts()` reads the way a real
    /// agent's history would after the same call.
    fn rewind(
        &self,
        nth: usize,
    ) -> ConversationFuture<'_, Result<Option<String>, ConversationError>> {
        Box::pin(async move {
            if let Ok(mut seen) = self.rewinds.lock() {
                seen.push(nth);
            }
            let Ok(mut prompts) = self.prompts.lock() else {
                return Ok(None);
            };
            if nth >= prompts.len() {
                return Ok(None);
            }
            let text = prompts[nth].clone();
            prompts.truncate(nth);
            Ok(Some(text))
        })
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
        conversation.respond_to_permission(
            &PermissionId("p1".to_string()),
            PermissionAnswer::Deny,
            None,
        );
        conversation.cancel();
        conversation.cancel();

        assert_eq!(
            conversation.answers(),
            vec![(PermissionId("p1".to_string()), PermissionAnswer::Deny, None)]
        );
        // Cancelling twice is what a person pressing Ctrl-C twice does, and it
        // must not be an error.
        assert_eq!(conversation.cancel_count(), 2);
    }
}
