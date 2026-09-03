//! Session construction and state.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use keke_auth_api::AuthProvider;
use keke_config_types::ApprovalPolicy;
use keke_config_types::CompactionConfig;
use keke_config_types::HomeLayout;
use keke_config_types::MaxOutputTokens;
use keke_config_types::ModelSelection;
use keke_config_types::ReasoningEffort;
use keke_plugin_api::ExtensionRegistry;
use keke_protocol::Message;
use keke_protocol::SessionEvent;
use keke_protocol::SessionId;
use keke_protocol::StopReason;
use keke_protocol::ThreadId;
use keke_protocol::ToolCall;
use keke_protocol::ToolResult;
use keke_protocol::TurnId;
use keke_protocol::Usage;
use keke_provider_api::ArcProvider;
use keke_workspace::Workspace;

use crate::CoreError;
use crate::RolloutRecorder;

/// What the engine tells a surface while a turn runs.
///
/// Distinct from [`SessionEvent`]: session events are the durable record, these
/// are live notifications. A surface renders from these while streaming and from
/// the log when replaying, and the two must agree.
#[derive(Clone, Debug)]
pub enum TurnUpdate {
    TurnStarted {
        turn: TurnId,
    },
    TextDelta {
        turn: TurnId,
        delta: String,
    },
    ThinkingDelta {
        turn: TurnId,
        delta: String,
    },
    ToolCallStarted {
        call: ToolCall,
    },
    ToolCallEnded {
        result: ToolResult,
    },
    /// A tool the vendor ran for itself inside the model call — see
    /// [`keke_protocol::SessionEvent::HostedToolCall`].
    ///
    /// Its own update rather than a `ToolCallStarted`/`ToolCallEnded` pair:
    /// there is no engine-side call to start or end, and it arrives already
    /// resolved. A surface that only drew engine tool calls would show a turn
    /// that silently paused while the vendor searched the web.
    HostedToolCall {
        turn: TurnId,
        name: String,
        query: Option<String>,
    },
    /// One model step's token accounting, as soon as the provider reports it.
    ///
    /// Live rather than only at the end of the turn: a surface showing what a
    /// turn is costing has to show it while the turn is still running, which is
    /// the moment a person can still decide to stop it.
    StepUsage {
        turn: TurnId,
        usage: Usage,
    },
    TurnEnded {
        turn: TurnId,
        stop_reason: StopReason,
    },
}

/// One place the conversation can be wound back to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewindPoint {
    /// Which user turn it is, counting from zero.
    pub turn: usize,
    /// The prompt as it was sent.
    pub prompt: String,
    /// Whether keke holds a snapshot of the tree from before this turn wrote.
    /// False for a turn that never wrote, and for every turn of a session that
    /// ran with checkpoints off.
    pub has_snapshot: bool,
}

/// What a rewind actually did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rewound {
    /// The prompt that started the turn, to hand back for editing.
    pub prompt: String,
    /// How many messages were dropped. Zero for a files-only rewind.
    pub removed_messages: usize,
    /// The files put back, workspace-relative.
    pub restored_files: Vec<String>,
}

/// The session's model configuration.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub model: ModelSelection,
    pub home: HomeLayout,
    /// Filled into every request, so a provider never has to invent one. One
    /// wire format rejects a request that omits it, and letting each vendor
    /// choose would give the same conversation a different budget per vendor.
    pub max_output_tokens: MaxOutputTokens,
    /// How hard the model is asked to think. `None` leaves the vendor default
    /// in place; the engine does not pick a level of its own, because a level
    /// keke chose would be indistinguishable in the log from one a person did.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Which queue the vendor should answer from, for vendors that sell more
    /// than one speed. `None` names none, leaving the endpoint's own routing
    /// alone — see [`ServiceTier`](keke_protocol::ServiceTier).
    pub service_tier: Option<keke_protocol::ServiceTier>,
    /// When and how far to summarize the history. A session that never compacts
    /// works until the provider rejects the request mid-conversation.
    pub compaction: CompactionConfig,
    /// When a tool call must be approved before it runs.
    pub approval: ApprovalPolicy,
    /// Whether the working tree is snapshotted per turn, so a rewind can put
    /// the files back too.
    pub checkpoints: keke_config_types::CheckpointConfig,
}

/// A live conversation.
pub struct Session {
    pub(crate) id: SessionId,
    pub(crate) thread: ThreadId,
    pub(crate) config: SessionConfig,
    pub(crate) provider: ArcProvider,
    pub(crate) auth: Option<Arc<dyn AuthProvider>>,
    pub(crate) registry: ExtensionRegistry,
    pub(crate) workspace: Workspace,
    pub(crate) cwd: PathBuf,
    pub(crate) history: Vec<Message>,
    /// Working-tree snapshots, when the deployment keeps them.
    ///
    /// Opening the store means creating a bare git repo the first time a
    /// project sees one, which costs a real `git init` subprocess — paid once
    /// per turn that actually writes, not once per session opened. A session
    /// that only talks, or that a person quits without prompting, must not pay
    /// it at all.
    pub(crate) checkpoints: CheckpointsState,
    /// The snapshot each user turn started from, by turn ordinal. Seeded from
    /// the log on a resume, so a rewind reaches past this process.
    pub(crate) snapshots: std::collections::BTreeMap<usize, keke_checkpoint::Snapshot>,
    /// Whether this turn has already taken its snapshot. A turn snapshots once,
    /// before it first changes anything, so a turn that only talks costs
    /// nothing and one that writes twice does not record the tree half-changed.
    pub(crate) snapshot_taken: bool,
    pub(crate) recorder: RolloutRecorder,
    pub(crate) updates: Option<tokio::sync::mpsc::UnboundedSender<TurnUpdate>>,
    pub(crate) cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
    pub(crate) approvals: Arc<crate::ApprovalMemory>,
    /// The live policy. Kept beside the config rather than in it because it is
    /// the one setting a person changes without restarting the session.
    pub(crate) approval: Arc<crate::ApprovalSwitch>,
    /// The live effort level, kept beside the config for the same reason.
    pub(crate) effort: Arc<crate::EffortSwitch>,
    /// The live service tier, kept beside the config for the same reason.
    pub(crate) tier: Arc<crate::ServiceTierSwitch>,
    /// The live model, kept beside the config for the same reason.
    pub(crate) model: Arc<crate::ModelSwitch>,
    /// The live session mode, kept beside the config for the same reason. Held
    /// by the engine and by whatever extension enforces the mode, which is why
    /// it can be supplied to the builder rather than only created by it.
    pub(crate) mode: Arc<crate::SessionModeSwitch>,
    flag: Arc<AtomicBool>,
}

/// Where the working-tree snapshot store stands.
///
/// `Pending` carries everything [`keke_checkpoint::Checkpoints::open`] needs
/// so opening it can be deferred past session construction to the first turn
/// that actually writes — see [`Session::checkpoints`].
pub(crate) enum CheckpointsState {
    /// The deployment has checkpoints off, or this is a subagent session.
    Disabled,
    /// Not opened yet: the store's directory, waiting for a first write.
    Pending(PathBuf),
    /// Opened and ready.
    Open(keke_checkpoint::Checkpoints),
    /// Opening failed. Remembered rather than retried every turn — a store
    /// that could not be created once is not going to succeed on the next
    /// attempt with nothing about the environment having changed.
    Failed,
}

impl Session {
    /// The snapshot store, opening it on first use.
    ///
    /// A session whose store failed to open, or that has checkpoints off,
    /// answers `None` and goes on running — it simply cannot put files back.
    pub(crate) async fn checkpoints(&mut self) -> Option<&keke_checkpoint::Checkpoints> {
        if let CheckpointsState::Pending(dir) = &self.checkpoints {
            let dir = dir.clone();
            let opened = keke_checkpoint::Checkpoints::open(
                &dir,
                &self.config.home.workspace_root,
                // keke's home, and the session logs inside it: a deployment
                // may put either in the project, and a restore that rolled
                // back the log being written into would take the record of
                // itself with it.
                &[
                    self.config.home.home.as_path(),
                    &crate::sessions_dir(&self.config.home.home),
                ],
                &self.id.to_string(),
                self.config.checkpoints.keep_days,
            )
            .await;
            self.checkpoints = match opened {
                Ok(store) => CheckpointsState::Open(store),
                Err(error) => {
                    // Not fatal: a session whose snapshots failed still works,
                    // it just cannot offer to put files back.
                    tracing::warn!(%error, "checkpoints are off for this session");
                    CheckpointsState::Failed
                }
            };
        }
        match &self.checkpoints {
            CheckpointsState::Open(store) => Some(store),
            CheckpointsState::Disabled
            | CheckpointsState::Pending(_)
            | CheckpointsState::Failed => None,
        }
    }
}

impl Session {
    #[must_use]
    pub fn id(&self) -> SessionId {
        self.id
    }

    #[must_use]
    pub fn thread(&self) -> ThreadId {
        self.thread
    }

    /// Where this session's log lives.
    #[must_use]
    pub fn log_path(&self) -> &std::path::Path {
        self.recorder.path()
    }

    /// The conversation so far.
    #[must_use]
    pub fn history(&self) -> &[Message] {
        &self.history
    }

    /// Abort the running turn.
    ///
    /// Cooperative: tools poll `ToolCallContext::is_cancelled` and the loop
    /// checks between steps. Nothing is killed outright, so a partially written
    /// file is not left behind by the harness itself.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        (self.cancelled)()
    }

    /// A handle that cancels this session, detached from its lifetime.
    ///
    /// A signal handler outlives the borrow a `&self` method would need, so it
    /// takes this instead of a reference to the session.
    pub fn canceller(&self) -> impl Fn() + Send + Sync + 'static + use<> {
        let flag = Arc::clone(&self.flag);
        move || flag.store(true, Ordering::SeqCst)
    }

    /// The standing permissions this session has been given.
    ///
    /// Exposed so a surface can show them, and so a test can assert that
    /// "always allow" was actually remembered rather than merely answered.
    #[must_use]
    pub fn approvals(&self) -> &Arc<crate::ApprovalMemory> {
        &self.approvals
    }

    /// How much this session may do without asking, right now.
    #[must_use]
    pub fn approval_policy(&self) -> ApprovalPolicy {
        self.approval.get()
    }

    /// Change the policy, taking effect on the next tool call rather than the
    /// next turn — a person raising the bar mid-turn means the call in front of
    /// them, not the one after the answer they are still waiting for.
    pub fn set_approval_policy(&self, policy: ApprovalPolicy) {
        self.approval.set(policy);
    }

    /// How hard the model is being asked to think, right now.
    #[must_use]
    pub fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.effort.get()
    }

    /// Change the effort level, taking effect on the next model request rather
    /// than the next turn: a person raising it mid-turn wants the step in front
    /// of them thought about harder, not the one after the answer.
    pub fn set_reasoning_effort(&self, effort: Option<ReasoningEffort>) {
        self.effort.set(effort);
    }

    /// The model this session is asking, right now.
    #[must_use]
    pub fn model(&self) -> Arc<str> {
        self.model.get()
    }

    /// Change the model, taking effect on the next model request.
    ///
    /// Within the session's provider only: the route was chosen when the
    /// session was built, along with the credentials it authenticates with.
    pub fn set_model(&self, model: impl Into<Arc<str>>) {
        self.model.set(model);
    }

    /// A handle that changes this session's model, detached from its lifetime.
    #[must_use]
    pub fn model_switch(&self) -> Arc<crate::ModelSwitch> {
        Arc::clone(&self.model)
    }

    /// How the session is being routed, right now.
    #[must_use]
    pub fn service_tier(&self) -> Option<keke_protocol::ServiceTier> {
        self.tier.get()
    }

    /// Change the queue, taking effect on the next model request for the same
    /// reason [`Session::set_reasoning_effort`] does: a person asking for a
    /// faster answer means the step in front of them.
    pub fn set_service_tier(&self, tier: Option<keke_protocol::ServiceTier>) {
        self.tier.set(tier);
    }

    /// A handle that changes this session's queue, detached from its lifetime.
    #[must_use]
    pub fn service_tier_switch(&self) -> Arc<crate::ServiceTierSwitch> {
        Arc::clone(&self.tier)
    }

    /// A handle that changes this session's effort level, detached from its
    /// lifetime — the counterpart of [`Session::approval_switch`].
    #[must_use]
    pub fn effort_switch(&self) -> Arc<crate::EffortSwitch> {
        Arc::clone(&self.effort)
    }

    /// A handle that changes this session's policy, detached from its lifetime.
    ///
    /// A surface holds this rather than the session, the same way a signal
    /// handler holds [`Session::canceller`].
    #[must_use]
    pub fn approval_switch(&self) -> Arc<crate::ApprovalSwitch> {
        Arc::clone(&self.approval)
    }

    /// A handle to the live session mode — the counterpart of
    /// [`Session::approval_switch`], and the one a surface writes through to
    /// turn plan mode on without restarting the session.
    #[must_use]
    pub fn mode_switch(&self) -> Arc<crate::SessionModeSwitch> {
        Arc::clone(&self.mode)
    }

    #[must_use]
    pub fn session_mode(&self) -> keke_config_types::SessionMode {
        self.mode.get()
    }

    /// Where the conversation can be wound back to, newest last.
    ///
    /// The prompt as it was sent, and whether keke holds a snapshot of the
    /// working tree from before that turn changed anything. A turn with no
    /// snapshot is one that never wrote — or one from a session that ran with
    /// checkpoints off — and the surface says so rather than offering a
    /// restore that would do nothing.
    #[must_use]
    pub fn rewind_points(&self) -> Vec<RewindPoint> {
        self.history
            .iter()
            .filter(|message| message.role == keke_protocol::Role::User)
            .enumerate()
            .map(|(turn, message)| RewindPoint {
                turn,
                prompt: message.text(),
                has_snapshot: self.snapshots.contains_key(&turn),
            })
            .collect()
    }

    /// Which files a restore to `nth` would put back.
    ///
    /// Asked when a person is deciding, not when the list is drawn: it is a
    /// diff against the working tree, and running one per row would spend the
    /// cost on every point they are not choosing.
    pub async fn changed_since_turn(&mut self, nth: usize) -> Result<Vec<String>, CoreError> {
        // No point opening the store for a turn that never took a snapshot —
        // there is nothing on either side of the diff to open it for.
        let Some(snapshot) = self.snapshots.get(&nth).cloned() else {
            return Ok(Vec::new());
        };
        let Some(store) = self.checkpoints().await else {
            return Ok(Vec::new());
        };
        Ok(store.changed_since(&snapshot).await?)
    }

    /// Wind the session back to just before its `nth` user turn (counting from
    /// zero), putting back what `scope` asks for.
    ///
    /// Two things can be taken back and they are independent. The conversation
    /// half drops the turn and everything after it: the prompt, the answer,
    /// every tool call it made, so the next request is assembled as though it
    /// had never happened. The files half puts the working tree back to the
    /// snapshot taken before that turn first wrote. Someone fixing a typo
    /// wants the first; someone whose agent made a mess of the tree may want
    /// the second and to keep the discussion of how it happened.
    ///
    /// Counts `Role::User` messages, which are exactly the turn inputs — a
    /// tool result is a `Role::Tool` message and never a turn of its own. A
    /// history with fewer user turns than `nth` is left untouched and answers
    /// `None`, rather than winding back to whatever happens to be last.
    ///
    /// Logged before it returns, because it changes both what the next model
    /// request will contain and what is on disk, and nothing else in the log
    /// would say so.
    pub async fn rewind_to_user_turn(
        &mut self,
        nth: usize,
        scope: keke_protocol::RewindScope,
    ) -> Result<Option<Rewound>, CoreError> {
        let Some(at) = self
            .history
            .iter()
            .enumerate()
            .filter(|(_, message)| message.role == keke_protocol::Role::User)
            .map(|(index, _)| index)
            .nth(nth)
        else {
            return Ok(None);
        };
        let prompt = self.history[at].text();

        // The files first: a failure here must not leave a conversation wound
        // back past the tree it was talking about, and a restore is the half
        // that can fail.
        let restored = match (scope.touches_files(), self.snapshots.get(&nth).cloned()) {
            (true, Some(snapshot)) => match self.checkpoints().await {
                Some(store) => store.restore(&snapshot).await?,
                None => keke_checkpoint::Restored::default(),
            },
            _ => keke_checkpoint::Restored::default(),
        };

        let removed_messages = if scope.touches_conversation() {
            let removed = self.history.len() - at;
            self.history.truncate(at);
            // The turns that went take their snapshots with them.
            self.snapshots.retain(|turn, _| *turn < nth);
            removed
        } else {
            0
        };

        self.log(SessionEvent::Rewound {
            scope,
            history: scope.touches_conversation().then(|| self.history.clone()),
            prompt: prompt.clone(),
            removed_messages,
            restored_files: restored.files.clone(),
            undo: restored.undo.as_ref().map(ToString::to_string),
        })
        .await?;

        Ok(Some(Rewound {
            prompt,
            removed_messages,
            restored_files: restored.files,
        }))
    }

    /// Snapshot the working tree before `call` is the first thing this turn to
    /// change it.
    ///
    /// Before rather than after, and only for a tool that can write: the point
    /// to go back to is the tree as it was when the person asked, and a turn
    /// that only reads has nothing to record. A store that fails is reported
    /// and forgotten — a snapshot keke could not take must not stop the work
    /// the person actually asked for.
    pub(crate) async fn checkpoint_before(&mut self, turn: TurnId, read_only: bool) {
        if read_only || self.snapshot_taken {
            return;
        }
        self.snapshot_taken = true;
        // Computed before the store borrow starts: the store's lifetime is
        // tied to `&mut self`, so nothing else on `self` can be read while
        // it's held.
        let user_turn = self
            .history
            .iter()
            .filter(|message| message.role == keke_protocol::Role::User)
            .count()
            .saturating_sub(1);
        let Some(store) = self.checkpoints().await else {
            return;
        };
        let snapshot = match store.take(&format!("before turn {}", user_turn + 1)).await {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(%error, "could not snapshot the working tree");
                return;
            }
        };
        if let Err(error) = self
            .log(SessionEvent::Checkpoint {
                turn,
                user_turn,
                snapshot: snapshot.to_string(),
            })
            .await
        {
            tracing::warn!(%error, "could not log a checkpoint");
        }
        self.snapshots.insert(user_turn, snapshot);
    }

    /// Clear the abort flag so the session can take another turn.
    pub fn reset_cancellation(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }

    pub(crate) async fn log(&mut self, event: SessionEvent) -> Result<(), CoreError> {
        self.recorder.append(event).await.map_err(CoreError::from)
    }

    pub(crate) fn emit(&self, update: TurnUpdate) {
        if let Some(sender) = &self.updates {
            // A closed receiver means the surface went away; the turn continues
            // and the log still records everything.
            let _ = sender.send(update);
        }
    }
}

/// Assembles a [`Session`].
///
/// A builder rather than a wide constructor because the required pieces come
/// from different places — config from disk, the provider from the registry,
/// extensions from the composition root — and a missing one should name itself.
#[derive(Clone, Default)]
pub struct SessionBuilder {
    config: Option<SessionConfig>,
    provider: Option<ArcProvider>,
    auth: Option<Arc<dyn AuthProvider>>,
    registry: Option<ExtensionRegistry>,
    cwd: Option<PathBuf>,
    updates: Option<tokio::sync::mpsc::UnboundedSender<TurnUpdate>>,
    resume: Option<Resumed>,
    parent: Option<SessionId>,
    mode: Option<Arc<crate::SessionModeSwitch>>,
}

/// The session a build continues instead of starting.
#[derive(Clone)]
struct Resumed {
    id: SessionId,
    history: Vec<Message>,
    /// The snapshots the earlier run took, by user-turn ordinal, so a rewind
    /// in a resumed session can still put the files back.
    snapshots: std::collections::BTreeMap<usize, String>,
}

impl SessionBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn config(mut self, config: SessionConfig) -> Self {
        self.config = Some(config);
        self
    }

    #[must_use]
    pub fn provider(mut self, provider: ArcProvider) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Share the session-mode switch with whoever enforces the mode.
    ///
    /// Supplied rather than only created here because the extension that
    /// enforces plan mode and the session that reports it must be looking at
    /// the same cell — two copies would let a surface say "planning" while the
    /// guards had already stopped. A build given none makes its own, so a
    /// session with no plan-mode extension installed still answers the
    /// question.
    ///
    /// Kept across a rebuild from the same recipe, because the extension
    /// registry is too: whoever rebuilds decides what a fresh session's mode
    /// should be, since only they know whether the rebuild is a person asking
    /// to start over.
    #[must_use]
    pub fn mode_switch(mut self, mode: Arc<crate::SessionModeSwitch>) -> Self {
        self.mode = Some(mode);
        self
    }

    /// Supply the auth provider backing `provider`, enabling refresh-and-retry
    /// on a 401. Optional: a provider authenticating some other way works
    /// without it, it just cannot recover from an expired credential.
    #[must_use]
    pub fn auth(mut self, auth: Arc<dyn AuthProvider>) -> Self {
        self.auth = Some(auth);
        self
    }

    #[must_use]
    pub fn extensions(mut self, registry: ExtensionRegistry) -> Self {
        self.registry = Some(registry);
        self
    }

    #[must_use]
    pub fn cwd(mut self, cwd: PathBuf) -> Self {
        self.cwd = Some(cwd);
        self
    }

    /// Continue an existing session: append to its log and start from its
    /// history rather than from nothing.
    ///
    /// The history is rebuilt from the log by [`crate::load_session`], never
    /// carried in a side file — a session keke can replay is a session keke can
    /// continue, and there is no second place for the two to disagree.
    #[must_use]
    pub fn resume(mut self, id: SessionId, history: Vec<Message>) -> Self {
        self.resume = Some(Resumed {
            id,
            history,
            snapshots: std::collections::BTreeMap::new(),
        });
        self
    }

    /// The working-tree snapshots the earlier run took, by user-turn ordinal,
    /// as [`crate::load_session`] read them back.
    ///
    /// Separate from [`Self::resume`] because a caller may be continuing a log
    /// written before checkpoints existed, or by a deployment that had them
    /// off: no snapshots is an ordinary resume, not a broken one.
    #[must_use]
    pub fn snapshots(mut self, snapshots: std::collections::BTreeMap<usize, String>) -> Self {
        if let Some(resumed) = self.resume.as_mut() {
            resumed.snapshots = snapshots;
        }
        self
    }

    /// Name the session this one is a child of.
    ///
    /// Set for a subagent, so its log says what it is. A child's log looks
    /// exactly like a person's — same turns, same cwd — and a listing that
    /// cannot tell them apart offers to continue a conversation nobody had.
    #[must_use]
    pub fn parent(mut self, parent: SessionId) -> Self {
        self.parent = Some(parent);
        self
    }

    /// Discard any resume this builder carries.
    ///
    /// Lets a caller keep a builder around as a recipe — same config,
    /// provider, extensions, cwd — and reuse it to start a genuinely new
    /// session later, even though the builder it was cloned from was itself
    /// continuing a previous log.
    #[must_use]
    pub fn fresh(mut self) -> Self {
        self.resume = None;
        self
    }

    /// Receive live turn updates. Without this the turn still runs and is still
    /// logged; nothing is rendered.
    #[must_use]
    pub fn updates(mut self, sender: tokio::sync::mpsc::UnboundedSender<TurnUpdate>) -> Self {
        self.updates = Some(sender);
        self
    }

    /// Build the session, creating its rollout log.
    pub async fn build(self) -> Result<Session, CoreError> {
        let config = self.config.ok_or(CoreError::Incomplete("a config"))?;
        let provider = self.provider.ok_or(CoreError::Incomplete("a provider"))?;

        let resumed = self.resume;
        let id = resumed.as_ref().map_or_else(SessionId::new, |it| it.id);
        // The recorder opens for append, so a resumed session writes on past
        // the end of the log it was rebuilt from.
        let workspace = Workspace::new(config.home.workspace_root.clone());
        let cwd = self
            .cwd
            .unwrap_or_else(|| config.home.workspace_root.as_path().to_path_buf());
        let mut recorder = RolloutRecorder::create(&config.home.home, &cwd, id).await?;

        // Written again on a resume: the log then says what the continued run
        // was configured with, which may not be what the first one was.
        recorder
            .append(SessionEvent::SessionStart {
                cwd: cwd.display().to_string(),
                provider: config.model.provider.clone(),
                model: config.model.model.clone(),
                parent: self.parent,
            })
            .await?;

        // Beside the project's logs rather than the session's: a store keyed
        // by session id would never see its own `HEAD` on the next run — every
        // session mints a fresh id — so opening it would `git init` a bare
        // repo on every single startup instead of hitting the early return
        // that makes a repeat run's `open` free. One repo per project, shared
        // across that project's sessions, is what makes the early return
        // actually fire after the first session ever opened here.
        //
        // Beside the log rather than inside the project: the snapshots are
        // keke's, and a store in the workspace would be one more directory a
        // person has to know to ignore.
        //
        // Not opened here: `git init`-ing a bare repo the first time a project
        // sees one is a real subprocess, and a session that only talks — or
        // that a person quits before typing anything — must not pay for it.
        // [`Session::checkpoints`] opens it lazily, the first time a turn
        // actually writes.
        //
        // A subagent takes no snapshots of its own. Its work happens inside a
        // tool call of the parent's turn, which the parent has already
        // snapshotted before dispatching — a second store under the child's
        // session would record the same tree again and offer a point no
        // conversation has a prompt for.
        let checkpoints = if config.checkpoints.enabled && self.parent.is_none() {
            CheckpointsState::Pending(
                crate::project_dir(&config.home.home, &cwd).join("checkpoints.git"),
            )
        } else {
            CheckpointsState::Disabled
        };

        let approval = config.approval;
        let effort = config.reasoning_effort;
        let tier = config.service_tier;
        let model = Arc::new(crate::ModelSwitch::new(config.model.model.as_str()));
        let flag = Arc::new(AtomicBool::new(false));
        let cancelled = {
            let flag = Arc::clone(&flag);
            Arc::new(move || flag.load(Ordering::SeqCst)) as Arc<dyn Fn() -> bool + Send + Sync>
        };

        Ok(Session {
            id,
            thread: ThreadId::new(),
            config,
            provider,
            auth: self.auth,
            registry: self.registry.unwrap_or_default(),
            workspace,
            cwd,
            snapshots: resumed
                .as_ref()
                .map(|it| {
                    it.snapshots
                        .iter()
                        .map(|(turn, id)| (*turn, keke_checkpoint::Snapshot::from(id.clone())))
                        .collect()
                })
                .unwrap_or_default(),
            history: resumed.map(|it| it.history).unwrap_or_default(),
            checkpoints,
            snapshot_taken: false,
            recorder,
            updates: self.updates,
            cancelled,
            approvals: Arc::new(crate::ApprovalMemory::default()),
            approval: Arc::new(crate::ApprovalSwitch::new(approval)),
            effort: Arc::new(crate::EffortSwitch::new(effort)),
            tier: Arc::new(crate::ServiceTierSwitch::new(tier)),
            model,
            mode: self.mode.unwrap_or_default(),
            flag,
        })
    }
}
