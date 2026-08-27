//! The coordinator: one per composition, holding every subagent a session runs.
//!
//! Lifecycle logic lives here rather than in each tool because a subagent's
//! transitions must be recorded in exactly one place. Two tools each owning
//! half of the state is how a start gets logged and a finish does not.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use keke_config_types::SubagentLimits;
use keke_core::SessionBuilder;
use keke_protocol::Message;
use keke_protocol::SessionEvent;
use keke_protocol::SessionId;
use keke_protocol::TurnId;
use keke_protocol::Usage;
use tokio::sync::Semaphore;

/// The handle the model uses to name one subagent.
pub type AgentId = String;

/// How a subagent ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    Completed,
    Failed,
    TimedOut,
    Cancelled,
}

impl AgentStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
        }
    }
}

/// What a finished subagent reports back to the parent's model.
#[derive(Clone, Debug)]
pub struct AgentReport {
    pub id: AgentId,
    pub task: String,
    pub status: AgentStatus,
    /// The child's final assistant message, or the failure that replaced it.
    pub summary: String,
    pub usage: Usage,
    /// Where the child's own log is, so a person can read the full transcript
    /// rather than only the summary the parent's model was given.
    pub log_path: String,
    /// The child's session, when it got far enough to have one.
    pub session: Option<SessionId>,
}

/// A subagent that has not been collected yet.
///
/// The join handle is the whole state: a finished child's report sits in it
/// until someone asks, so a model that spawns without waiting still gets the
/// result whenever it comes back rather than losing it to a reaper.
struct Slot {
    task: String,
    handle: tokio::task::JoinHandle<AgentReport>,
}

/// Why a spawn or collect could not be served.
#[derive(Debug, thiserror::Error)]
pub enum SubagentError {
    /// Nothing attached a recipe, so there is nothing to build a child from.
    /// A configuration fault, not a model one — reported rather than silently
    /// answered with an empty result (`AGENTS.md` invariant 8).
    #[error("subagents are not configured for this session")]
    Unattached,
    #[error("no subagent named `{0}`")]
    Unknown(AgentId),
    #[error("the subagent task panicked: {0}")]
    Lost(String),
}

/// Runs subagents for one composition.
///
/// Built by [`crate::install`] and shared by the tools it registers. The recipe
/// is attached afterwards because a `SessionBuilder` carries the extension
/// registry, and the registry is not finished at the moment this is installed
/// into it — the host is part of what the recipe contains. It is a `OnceLock`
/// and not a lock: attaching happens once, before any turn runs, and the value
/// never changes after (`AGENTS.md` invariant 5).
pub struct SubagentHost {
    recipe: std::sync::OnceLock<SessionBuilder>,
    limits: SubagentLimits,
    permits: Arc<Semaphore>,
    next: AtomicU64,
    slots: Mutex<HashMap<AgentId, Slot>>,
    /// The sessions this host created. A subagent asking for its tool set is
    /// answered with nothing, which is what makes the tree one level deep by
    /// construction rather than by a depth counter someone can raise.
    children: Mutex<HashSet<SessionId>>,
}

impl SubagentHost {
    #[must_use]
    pub fn new(limits: SubagentLimits) -> Self {
        Self {
            recipe: std::sync::OnceLock::new(),
            limits,
            permits: Arc::new(Semaphore::new(usize::from(limits.max_concurrent))),
            next: AtomicU64::new(1),
            slots: Mutex::new(HashMap::new()),
            children: Mutex::new(HashSet::new()),
        }
    }

    /// Supply the recipe children are built from.
    ///
    /// Pass the session builder *before* live updates are attached to it: a
    /// child streams to no surface, it reports once at the end.
    ///
    /// Ignored if a recipe is already attached, so a second composition cannot
    /// redirect the children of the first.
    pub fn attach(&self, builder: SessionBuilder) {
        let _ = self.recipe.set(builder.fresh());
    }

    #[must_use]
    pub fn is_attached(&self) -> bool {
        self.recipe.get().is_some()
    }

    /// Whether `session` is a subagent this host started.
    #[must_use]
    pub fn is_child(&self, session: SessionId) -> bool {
        self.children
            .lock()
            .is_ok_and(|children| children.contains(&session))
    }

    #[must_use]
    pub fn limits(&self) -> SubagentLimits {
        self.limits
    }

    /// Start a subagent and return its handle without waiting for it.
    ///
    /// The child is built inside the spawned task, so a spawn never blocks the
    /// parent's turn on session construction or on the concurrency permit.
    pub fn spawn(
        self: &Arc<Self>,
        task: String,
        parent_cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Result<AgentId, SubagentError> {
        let recipe = self.recipe.get().ok_or(SubagentError::Unattached)?.clone();
        let id = format!("agent_{}", self.next.fetch_add(1, Ordering::SeqCst));

        let permits = Arc::clone(&self.permits);
        let timeout = self.limits.timeout();
        let handle = tokio::spawn(run_child(
            Arc::clone(self),
            id.clone(),
            task.clone(),
            recipe,
            permits,
            timeout,
            parent_cancelled,
        ));

        if let Ok(mut slots) = self.slots.lock() {
            slots.insert(id.clone(), Slot { task, handle });
        }
        Ok(id)
    }

    /// Record a child's session id so its own turns are answered without the
    /// subagent tools. Called by the child task as soon as the session exists.
    fn adopt(&self, session: SessionId) {
        if let Ok(mut children) = self.children.lock() {
            children.insert(session);
        }
    }

    /// Wait for one subagent and take its report.
    ///
    /// Taking rather than reading: a report is delivered to the model once, and
    /// a handle that has been collected is gone. Asking again names the id that
    /// no longer exists instead of replaying an answer the model already has.
    pub async fn collect(&self, id: &str) -> Result<AgentReport, SubagentError> {
        let slot = self
            .slots
            .lock()
            .ok()
            .and_then(|mut slots| slots.remove(id))
            .ok_or_else(|| SubagentError::Unknown(id.to_string()))?;

        slot.handle
            .await
            .map_err(|error| SubagentError::Lost(error.to_string()))
    }

    /// Every subagent still outstanding, oldest handle first.
    #[must_use]
    pub fn outstanding(&self) -> Vec<AgentId> {
        let Ok(slots) = self.slots.lock() else {
            return Vec::new();
        };
        let mut ids: Vec<_> = slots.keys().cloned().collect();
        // Numeric rather than lexical: `agent_10` is younger than `agent_9`.
        ids.sort_by_key(|id| {
            id.rsplit_once('_')
                .and_then(|(_, n)| n.parse::<u64>().ok())
                .unwrap_or(u64::MAX)
        });
        ids
    }

    /// What a running subagent was asked to do, for a report that has to name
    /// it before it has one of its own.
    #[must_use]
    pub fn task_of(&self, id: &str) -> Option<String> {
        let slots = self.slots.lock().ok()?;
        Some(slots.get(id)?.task.clone())
    }
}

/// The parent's record of a child, written into the parent's log.
///
/// Not the child's log — the child writes its own, in full. This is the entry
/// that lets a reader of the parent transcript find it (`AGENTS.md` invariant 6).
#[must_use]
pub(crate) fn end_event(turn: TurnId, report: &AgentReport) -> SessionEvent {
    SessionEvent::SubagentEnd {
        turn,
        agent: report.id.clone(),
        session: report.session,
        status: report.status.as_str().to_string(),
        summary: report.summary.clone(),
        usage: report.usage,
    }
}

/// How often the child checks whether the parent's turn was aborted.
///
/// A poll rather than a shared token because `ToolCallContext` hands out a
/// closure, deliberately, so the tool ABI stays free of a runtime dependency.
const CANCEL_POLL_MILLIS: u64 = 200;

/// Build the child, run its single turn, and report.
async fn run_child(
    host: Arc<SubagentHost>,
    id: AgentId,
    task: String,
    recipe: SessionBuilder,
    permits: Arc<Semaphore>,
    timeout: std::time::Duration,
    parent_cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
) -> AgentReport {
    let failed = |status: AgentStatus, summary: String| AgentReport {
        id: id.clone(),
        task: task.clone(),
        status,
        summary,
        usage: Usage::default(),
        log_path: String::new(),
        session: None,
    };

    // Acquired before the session is built: a queued subagent should not have
    // opened a rollout log it is not yet running in.
    let Ok(_permit) = permits.acquire().await else {
        return failed(
            AgentStatus::Failed,
            "the subagent pool was shut down".to_string(),
        );
    };

    if parent_cancelled() {
        return failed(
            AgentStatus::Cancelled,
            "the parent turn was cancelled before this subagent started".to_string(),
        );
    }

    let mut session = match recipe.build().await {
        Ok(session) => session,
        Err(error) => {
            return failed(
                AgentStatus::Failed,
                format!("the subagent session could not be opened: {error}"),
            );
        }
    };

    // Before the first turn: from here on the child asking for its tool set is
    // answered without the subagent tools, so it cannot fork further.
    host.adopt(session.id());

    let log_path = session.log_path().display().to_string();
    let canceller = session.canceller();
    let watchdog = tokio::spawn(async move {
        while !parent_cancelled() {
            tokio::time::sleep(std::time::Duration::from_millis(CANCEL_POLL_MILLIS)).await;
        }
        canceller();
    });

    let outcome =
        tokio::time::timeout(timeout, session.run_turn(Message::user(task.clone()))).await;
    watchdog.abort();

    let (status, summary, usage) = match outcome {
        Ok(Ok(turn)) => (
            AgentStatus::Completed,
            turn.message
                .as_ref()
                .map(Message::text)
                .unwrap_or_else(|| "the subagent produced no reply".to_string()),
            turn.usage,
        ),
        Ok(Err(error)) => (
            AgentStatus::Failed,
            format!("the subagent failed: {error}"),
            Usage::default(),
        ),
        Err(_) => {
            // Cooperative, like every other cancel in keke: the child stops at
            // its next checkpoint rather than being killed mid-write.
            session.cancel();
            (
                AgentStatus::TimedOut,
                format!(
                    "the subagent ran past its {}ms budget and was stopped",
                    timeout.as_millis()
                ),
                Usage::default(),
            )
        }
    };

    AgentReport {
        id,
        task,
        status,
        summary,
        usage,
        log_path,
        session: Some(session.id()),
    }
}
