//! Shell commands that outlive the turn that started them.
//!
//! The difference from the blocking `bash` tool is only who waits. The child is
//! spawned the same way, into the same workspace root, under the same approval
//! — it is a normal tool call — and then the turn returns instead of blocking.
//! Its output accumulates in a capped buffer until someone reads it.
//!
//! Everything a background command tells the model arrives as an ordinary tool
//! result (`task_output`), so it is logged like any other, and invariant 6 in
//! `AGENTS.md` needs no new session event to hold it.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use keke_config_types::BackgroundLimits;
use keke_paths::AbsPath;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::process::Command;

use crate::source::TaskId;
use crate::source::TaskOutput;
use crate::source::TaskSnapshot;
use crate::source::TaskSource;
use crate::source::TaskState;

/// The word this source's rows carry, and the prefix on its ids.
pub const KIND: &str = "command";

/// Why a command could not be started.
#[derive(Debug, thiserror::Error)]
pub enum BackgroundError {
    #[error("{0} background commands are already running — kill one first")]
    TooMany(u8),
    #[error("spawning the shell failed: {0}")]
    Spawn(String),
}

/// One running or finished command.
struct Slot {
    command: String,
    state: TaskState,
    /// Output not yet read, oldest first. A deque because the cap is enforced
    /// by dropping from the front, which is the end nobody wants.
    pending: VecDeque<u8>,
    /// How much has been dropped to stay under the cap, over the task's life.
    dropped: u64,
    /// Set while the child is running, taken to stop it.
    kill: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Slot {
    /// Append, dropping from the front to stay inside the cap.
    fn push(&mut self, line: &str, cap: usize) {
        self.pending.extend(line.as_bytes());
        self.pending.push_back(b'\n');
        while self.pending.len() > cap {
            self.pending.pop_front();
            self.dropped += 1;
        }
    }

    fn take_output(&mut self) -> TaskOutput {
        let bytes: Vec<u8> = self.pending.drain(..).collect();
        let dropped = std::mem::take(&mut self.dropped);
        TaskOutput {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            dropped,
        }
    }
}

/// Every background command one session has started.
pub struct BackgroundTasks {
    limits: BackgroundLimits,
    next: AtomicU64,
    slots: Mutex<HashMap<TaskId, Slot>>,
    /// Ids in the order they were started, so rows read chronologically. Kept
    /// beside `slots` because a `HashMap` has no order to offer.
    order: Mutex<Vec<TaskId>>,
    /// Surfaces watching the rows. A whole snapshot goes to each on every
    /// change, for the reason `SubagentHost` does the same: a list this short
    /// is cheaper to resend than to reconcile, and a receiver that missed one
    /// message would otherwise be wrong forever.
    watchers: Mutex<Vec<tokio::sync::mpsc::UnboundedSender<Vec<TaskSnapshot>>>>,
}

impl BackgroundTasks {
    #[must_use]
    pub fn new(limits: BackgroundLimits) -> Self {
        Self {
            limits,
            next: AtomicU64::new(1),
            slots: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
            watchers: Mutex::new(Vec::new()),
        }
    }

    /// Watch the rows. The current snapshot arrives immediately, so a surface
    /// that subscribes mid-session draws what is already running.
    #[must_use]
    pub fn subscribe(&self) -> tokio::sync::mpsc::UnboundedReceiver<Vec<TaskSnapshot>> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = tx.send(self.snapshots());
        if let Ok(mut watchers) = self.watchers.lock() {
            watchers.push(tx);
        }
        rx
    }

    /// Tell every watcher what the rows are now.
    ///
    /// Called from every path that changes a task's state, so that no change
    /// can happen without the surface hearing about it — the failure mode is a
    /// command that exited an hour ago and is still spinning on screen.
    fn publish(&self) {
        let snapshot = self.snapshots();
        if let Ok(mut watchers) = self.watchers.lock() {
            watchers.retain(|watcher| watcher.send(snapshot.clone()).is_ok());
        }
    }

    #[must_use]
    pub fn limits(&self) -> BackgroundLimits {
        self.limits
    }

    fn running_count(&self) -> u8 {
        let Ok(slots) = self.slots.lock() else {
            return 0;
        };
        u8::try_from(
            slots
                .values()
                .filter(|slot| slot.state.is_running())
                .count(),
        )
        .unwrap_or(u8::MAX)
    }

    /// Start a command and return at once.
    ///
    /// Refused rather than queued past the limit: the model asked to start
    /// something and carry on, and a start that silently waits for a slot is
    /// the opposite of what it asked for (`AGENTS.md` invariant 8 — say so).
    pub fn spawn(
        self: &Arc<Self>,
        command: String,
        cwd: &AbsPath,
    ) -> Result<TaskId, BackgroundError> {
        if self.running_count() >= self.limits.max_concurrent {
            return Err(BackgroundError::TooMany(self.limits.max_concurrent));
        }

        let (program, flag) = if cfg!(windows) {
            ("cmd", "/C")
        } else {
            ("sh", "-c")
        };
        let mut child = Command::new(program)
            .arg(flag)
            .arg(&command)
            .current_dir(cwd.as_path())
            // A background command has nobody to answer a prompt, so its stdin
            // is closed rather than left inheriting the terminal's — a child
            // reading from the person's keyboard is the worst kind of hang.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| BackgroundError::Spawn(format!("{program}: {error}")))?;

        let id = format!("{KIND}_{}", self.next.fetch_add(1, Ordering::SeqCst));
        let (kill, killed) = tokio::sync::oneshot::channel();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        if let (Ok(mut slots), Ok(mut order)) = (self.slots.lock(), self.order.lock()) {
            slots.insert(
                id.clone(),
                Slot {
                    command,
                    state: TaskState::Running,
                    pending: VecDeque::new(),
                    dropped: 0,
                    kill: Some(kill),
                },
            );
            order.push(id.clone());
        }
        self.publish();

        // Both streams are read into the one buffer, interleaved as they
        // arrive: a command's diagnostics belong beside the output they explain
        // rather than in a second list a reader has to merge by hand.
        for stream in [stdout.map(Reader::Out), stderr.map(Reader::Err)] {
            let Some(stream) = stream else { continue };
            let host = Arc::clone(self);
            let id = id.clone();
            tokio::spawn(async move { host.pump(&id, stream).await });
        }

        let host = Arc::clone(self);
        let waiting_id = id.clone();
        let grace = self.limits.kill_grace();
        tokio::spawn(async move {
            let state = tokio::select! {
                status = child.wait() => match status {
                    Ok(status) => TaskState::Exited(status.code()),
                    Err(error) => {
                        host.note(&waiting_id, &format!("[wait failed: {error}]"));
                        TaskState::Exited(None)
                    }
                },
                _ = killed => {
                    terminate(&mut child, grace).await;
                    TaskState::Killed
                }
            };
            host.finish(&waiting_id, state);
        });

        Ok(id)
    }

    async fn pump(&self, id: &str, stream: Reader) {
        let cap = usize::try_from(self.limits.output_bytes).unwrap_or(usize::MAX);
        let mut lines = match stream {
            Reader::Out(out) => BufReader::new(Box::pin(out) as PinnedRead).lines(),
            Reader::Err(err) => BufReader::new(Box::pin(err) as PinnedRead).lines(),
        };
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(mut slots) = self.slots.lock() else {
                return;
            };
            let Some(slot) = slots.get_mut(id) else {
                return;
            };
            slot.push(&line, cap);
        }
    }

    /// Add a line of keke's own, distinguishable from the child's by its
    /// brackets, for the one case where the failure is ours to report.
    fn note(&self, id: &str, line: &str) {
        let cap = usize::try_from(self.limits.output_bytes).unwrap_or(usize::MAX);
        if let Ok(mut slots) = self.slots.lock()
            && let Some(slot) = slots.get_mut(id)
        {
            slot.push(line, cap);
        }
    }

    fn finish(&self, id: &str, state: TaskState) {
        if let Ok(mut slots) = self.slots.lock()
            && let Some(slot) = slots.get_mut(id)
        {
            slot.state = state;
            slot.kill = None;
        }
        self.publish();
    }

    /// Forget a finished task. Running ones are killed first, so a session
    /// shutting down leaves no children behind.
    pub fn clear(&self) {
        let ids: Vec<TaskId> = self
            .order
            .lock()
            .map(|order| order.clone())
            .unwrap_or_default();
        for id in &ids {
            self.kill(id);
        }
        if let (Ok(mut slots), Ok(mut order)) = (self.slots.lock(), self.order.lock()) {
            slots.clear();
            order.clear();
        }
        self.publish();
    }
}

/// SIGTERM, then SIGKILL once the grace period is up.
///
/// The grace is what separates a stop from a kill: a dev server given a moment
/// removes its socket, and one that is not leaves it for the next run to trip
/// over.
///
/// The signal goes through `kill(1)` rather than `libc::kill`, because the
/// workspace denies `unsafe`. One extra process on a path taken once per task
/// is a better trade than an unsafe block and a new dependency.
async fn terminate(child: &mut tokio::process::Child, grace: std::time::Duration) {
    if cfg!(unix)
        && !grace.is_zero()
        && let Some(pid) = child.id()
        && let Ok(mut signal) = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    {
        let _ = signal.wait().await;
        if tokio::time::timeout(grace, child.wait()).await.is_ok() {
            return;
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

/// Which stream a pump is draining. The two halves have different types, and
/// this is the smallest thing that lets one function drain either.
enum Reader {
    Out(tokio::process::ChildStdout),
    Err(tokio::process::ChildStderr),
}

type PinnedRead = std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>;

impl TaskSource for BackgroundTasks {
    fn owns(&self, id: &str) -> bool {
        id.starts_with(KIND)
    }

    fn snapshots(&self) -> Vec<TaskSnapshot> {
        let Ok(order) = self.order.lock() else {
            return Vec::new();
        };
        order.iter().filter_map(|id| self.snapshot(id)).collect()
    }

    fn snapshot(&self, id: &str) -> Option<TaskSnapshot> {
        let slots = self.slots.lock().ok()?;
        let slot = slots.get(id)?;
        Some(TaskSnapshot {
            id: id.to_string(),
            kind: KIND,
            description: slot.command.clone(),
            state: slot.state.clone(),
        })
    }

    fn take_output(&self, id: &str) -> Option<TaskOutput> {
        let mut slots = self.slots.lock().ok()?;
        Some(slots.get_mut(id)?.take_output())
    }

    fn kill(&self, id: &str) -> bool {
        let Ok(mut slots) = self.slots.lock() else {
            return false;
        };
        let Some(slot) = slots.get_mut(id) else {
            return false;
        };
        // Dropping the sender is what the waiter sees; a task that already
        // finished has no sender left and needs no signal.
        if let Some(kill) = slot.kill.take() {
            let _ = kill.send(());
        }
        drop(slots);
        self.publish();
        true
    }
}
