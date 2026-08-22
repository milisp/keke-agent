//! Session construction and state.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use keke_auth_api::AuthProvider;
use keke_config_types::HomeLayout;
use keke_config_types::ModelSelection;
use keke_plugin_api::ExtensionRegistry;
use keke_protocol::Message;
use keke_protocol::SessionEvent;
use keke_protocol::SessionId;
use keke_protocol::StopReason;
use keke_protocol::ThreadId;
use keke_protocol::ToolCall;
use keke_protocol::ToolResult;
use keke_protocol::TurnId;
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
    TurnEnded {
        turn: TurnId,
        stop_reason: StopReason,
    },
}

/// The session's model configuration.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub model: ModelSelection,
    pub home: HomeLayout,
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
    pub(crate) recorder: RolloutRecorder,
    pub(crate) updates: Option<tokio::sync::mpsc::UnboundedSender<TurnUpdate>>,
    pub(crate) cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
    flag: Arc<AtomicBool>,
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
#[derive(Default)]
pub struct SessionBuilder {
    config: Option<SessionConfig>,
    provider: Option<ArcProvider>,
    auth: Option<Arc<dyn AuthProvider>>,
    registry: Option<ExtensionRegistry>,
    cwd: Option<PathBuf>,
    updates: Option<tokio::sync::mpsc::UnboundedSender<TurnUpdate>>,
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

        let id = SessionId::new();
        let mut recorder = RolloutRecorder::create(&config.home.home, id).await?;
        let workspace = Workspace::new(config.home.workspace_root.clone());
        let cwd = self
            .cwd
            .unwrap_or_else(|| config.home.workspace_root.as_path().to_path_buf());

        recorder
            .append(SessionEvent::SessionStart {
                cwd: cwd.display().to_string(),
                provider: config.model.provider.clone(),
                model: config.model.model.clone(),
            })
            .await?;

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
            history: Vec::new(),
            recorder,
            updates: self.updates,
            cancelled,
            flag,
        })
    }
}
