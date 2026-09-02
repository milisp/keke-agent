//! The verbs every kind of outstanding work answers to.
//!
//! A session can leave two very different things running — a shell command and
//! a subagent — and a model that has to remember which verb goes with which
//! kind will eventually pick wrong. So the id namespace is shared and the
//! questions are the same three: what is it doing, what has it said, stop it.
//!
//! Each kind implements this trait over its own registry. The tools dispatch by
//! asking each source in turn whether it owns the id, which is why an id must
//! be unique across sources — [`TaskSource::owns`] is the whole contract.

use std::sync::Arc;

/// What the model calls one piece of outstanding work.
pub type TaskId = String;

/// How a task ended, or that it has not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskState {
    Running,
    /// Exited on its own. The code is what the process returned; `None` where
    /// a signal ended it and there was no code to return.
    Exited(Option<i32>),
    /// Stopped because someone asked, rather than because it was finished.
    Killed,
}

impl TaskState {
    #[must_use]
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }

    /// How the state is written wherever a person or a model reads one.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Running => "running".to_string(),
            Self::Exited(Some(0)) => "exited".to_string(),
            Self::Exited(Some(code)) => format!("exited({code})"),
            Self::Exited(None) => "signalled".to_string(),
            Self::Killed => "killed".to_string(),
        }
    }
}

/// One row of outstanding work, small enough to resend whole on every change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskSnapshot {
    pub id: TaskId,
    /// `command` or `subagent`. A fixed word rather than an enum because the
    /// set is open — anything a future source registers is another word, not
    /// another variant every match arm has to learn.
    pub kind: &'static str,
    /// What it was started to do, for a person reading a status line.
    pub description: String,
    pub state: TaskState,
}

/// What a read of a task's output returned.
#[derive(Clone, Debug, Default)]
pub struct TaskOutput {
    pub text: String,
    /// Bytes dropped from the front because the buffer was full. Reported
    /// rather than hidden: a model reasoning about a log needs to know it is
    /// looking at a tail.
    pub dropped: u64,
}

/// A registry of outstanding work that the shared task verbs can address.
///
/// Implementers own their own lifecycle state. This trait is deliberately
/// read-and-stop only — there is no `start` — because what it takes to start a
/// task is exactly what differs between kinds, and a common constructor would
/// be a union of every kind's arguments.
pub trait TaskSource: Send + Sync {
    /// Whether this source is the one that can answer for `id`.
    ///
    /// Ids must not collide across sources. Each source prefixes its own, and
    /// the dispatcher takes the first that claims one.
    fn owns(&self, id: &str) -> bool;

    /// Every task this source is holding, oldest first, including finished
    /// ones that have not been read yet.
    fn snapshots(&self) -> Vec<TaskSnapshot>;

    /// One task's row, or `None` if this source does not have it.
    fn snapshot(&self, id: &str) -> Option<TaskSnapshot>;

    /// What the task has said since the last read.
    ///
    /// Consuming rather than replaying: a model polling a dev server wants the
    /// new lines, and re-sending the whole log every time is how a context
    /// window is spent on text that was already read.
    fn take_output(&self, id: &str) -> Option<TaskOutput>;

    /// Stop it. Returns whether the id was known — stopping something already
    /// finished is not an error, because a caller cannot know it finished
    /// between its last read and this call.
    fn kill(&self, id: &str) -> bool;
}

/// The sources a session's task verbs address, in the order they are asked.
#[derive(Clone, Default)]
pub struct TaskSources(Vec<Arc<dyn TaskSource>>);

impl TaskSources {
    #[must_use]
    pub fn new(sources: Vec<Arc<dyn TaskSource>>) -> Self {
        Self(sources)
    }

    /// The source that owns `id`, if any.
    #[must_use]
    pub fn find(&self, id: &str) -> Option<&Arc<dyn TaskSource>> {
        self.0.iter().find(|source| source.owns(id))
    }

    /// Every outstanding row across every source, in source order.
    #[must_use]
    pub fn snapshots(&self) -> Vec<TaskSnapshot> {
        self.0
            .iter()
            .flat_map(|source| source.snapshots())
            .collect()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for TaskSources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskSources")
            .field("sources", &self.0.len())
            .finish()
    }
}
