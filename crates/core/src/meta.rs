//! A session's derived summary, cached beside its log.
//!
//! Everything here is reconstructable from `rollout.jsonl` — invariant 6 says
//! the log is what a session can be replayed from, and nothing in this file is
//! allowed to become a second source of truth. Deleting `meta.json`, or reading
//! one this build does not understand, costs a full scan and nothing else.
//!
//! It exists because the scan is not cheap. A log records the whole
//! model-visible history on every step, so it grows with the square of the
//! turns: listing sessions by parsing all of them was reading tens of megabytes
//! of JSON to print four columns.
//!
//! Two offsets are what make the cache incremental. `bytes_scanned` is how far
//! the fold has already consumed, so a session that grew by one turn is caught
//! up by reading that turn. `baseline` is where the last [`ModelRequest`] line
//! starts — the history a resume rebuilds from is that line plus what follows,
//! so resuming reads the tail rather than the log.
//!
//! [`ModelRequest`]: keke_protocol::SessionEvent::ModelRequest

use std::io::BufRead;
use std::io::Seek;
use std::path::Path;

use keke_protocol::SessionEvent;
use keke_protocol::SessionEventEnvelope;
use keke_protocol::SessionId;
use keke_protocol::Usage;
use serde::Deserialize;
use serde::Serialize;

use crate::RolloutError;

/// The name of the cache, inside a session's directory.
pub(crate) const META_FILE: &str = "meta.json";

/// The log itself, inside a session's directory.
pub(crate) const LOG_FILE: &str = "rollout.jsonl";

/// Bumped whenever a field's meaning changes. A cache from another version is
/// discarded rather than migrated: it is derived data, and rebuilding it is
/// what the fold already does.
const VERSION: u32 = 1;

/// What a listing and a resume need, without reading the log.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionMeta {
    pub version: u32,
    /// The directory the session was started in, from its `SessionStart`.
    pub cwd: Option<String>,
    /// Set when this session is a subagent's, from its `SessionStart`.
    pub parent: Option<SessionId>,
    /// RFC 3339, from the last line folded. Empty for a log with no lines.
    pub updated_at: String,
    pub turns: usize,
    /// First thing the person said, for telling two sessions apart.
    pub summary: String,
    /// What the session has spent, summed over the turns that finished and the
    /// subagents they delegated to.
    pub usage: Usage,
    /// Input tokens of the last logged model step: how full the window is.
    pub context_input: u64,
    /// The model that answered the last logged step, else the one the session
    /// opened with.
    pub model: Option<String>,
    pub last_step_model: Option<String>,
    pub reasoning_effort: Option<keke_protocol::ReasoningEffort>,
    /// The approval policy of the last logged turn, as the wire spelled it.
    pub approval_policy: Option<String>,
    /// Byte offset of the last `ModelRequest` line, when the log has one.
    pub baseline: Option<u64>,
    /// How much of the log the fields above account for.
    pub bytes_scanned: u64,
    /// Set aside for `context_input` when no step reported one.
    turn_input: u64,
}

impl SessionMeta {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            version: VERSION,
            ..Self::default()
        }
    }

    /// Read the cache for the session logged at `log`, if there is a usable one.
    ///
    /// A cache from another version, or one claiming to have read more of the
    /// log than the log currently holds, is absent: the second case is a log
    /// that was truncated or rewritten underneath it, and folding onto a stale
    /// prefix would produce numbers no scan agrees with.
    #[must_use]
    pub fn read(log: &Path, len: u64) -> Option<Self> {
        let text = std::fs::read_to_string(meta_path(log)?).ok()?;
        let meta: Self = serde_json::from_str(&text).ok()?;
        (meta.version == VERSION && meta.bytes_scanned <= len).then_some(meta)
    }

    /// Write the cache beside `log`.
    ///
    /// Through a temporary name, because a listing that reads a half-written
    /// cache would report a session that never existed. A failure is dropped:
    /// the cache is an optimisation, and a session that cannot write one still
    /// has to be able to run.
    pub fn write(&self, log: &Path) {
        let Some(path) = meta_path(log) else {
            return;
        };
        let Ok(text) = serde_json::to_string(self) else {
            return;
        };
        let temporary = path.with_extension("json.tmp");
        if std::fs::write(&temporary, text).is_ok() && std::fs::rename(&temporary, &path).is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
    }

    /// The cache for `log`, brought up to date with whatever it now holds.
    ///
    /// Reads from `bytes_scanned` rather than from the start, so the cost is
    /// the growth since the last fold and not the size of the log.
    pub fn refreshed(log: &Path) -> Result<Self, RolloutError> {
        let len = std::fs::metadata(log)
            .map_err(|source| RolloutError::Io {
                path: log.display().to_string(),
                source,
            })?
            .len();
        let mut meta = Self::read(log, len).unwrap_or_else(Self::new);
        if meta.bytes_scanned < len {
            meta.fold(log)?;
        }
        Ok(meta)
    }

    /// Consume the log from `bytes_scanned` to its end.
    ///
    /// A line that does not parse advances the offset like any other: skipping
    /// it without accounting for its bytes would fold it again on every
    /// refresh, and the fold has to be idempotent to be a cache at all.
    fn fold(&mut self, log: &Path) -> Result<(), RolloutError> {
        let io = |source: std::io::Error| RolloutError::Io {
            path: log.display().to_string(),
            source,
        };
        let mut file = std::fs::File::open(log).map_err(io)?;
        file.seek(std::io::SeekFrom::Start(self.bytes_scanned))
            .map_err(io)?;

        let mut reader = std::io::BufReader::new(file);
        let mut line = String::new();
        loop {
            line.clear();
            let read = reader.read_line(&mut line).map_err(io)?;
            if read == 0 {
                break;
            }
            // A final line with no newline is a write that was interrupted:
            // leaving the offset before it means the completed line is folded
            // on the next refresh instead of being lost.
            if !line.ends_with('\n') {
                break;
            }
            let at = self.bytes_scanned;
            self.bytes_scanned += read as u64;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionEventEnvelope>(&line) {
                Ok(envelope) => self.apply(at, &envelope),
                Err(error) => tracing::warn!(%error, "skipping unreadable session log line"),
            }
        }
        Ok(())
    }

    /// Fold an event the recorder just appended, `len` bytes long.
    ///
    /// The writer folds what it writes instead of reading it back, so a running
    /// session never rescans its own log.
    pub(crate) fn append(&mut self, len: u64, envelope: &SessionEventEnvelope) {
        let at = self.bytes_scanned;
        self.bytes_scanned += len;
        self.apply(at, envelope);
    }

    /// Fold one event, whose line begins at byte `at`.
    fn apply(&mut self, at: u64, envelope: &SessionEventEnvelope) {
        self.updated_at.clone_from(&envelope.at);
        match &envelope.event {
            SessionEvent::SessionStart {
                cwd, model, parent, ..
            } => {
                // The first start wins for cwd: a resumed run writes another
                // one, and a session belongs to where it began.
                if self.cwd.is_none() {
                    self.cwd = Some(cwd.clone());
                }
                if self.parent.is_none() {
                    self.parent = *parent;
                }
                if self.model.is_none() {
                    self.model = Some(model.clone());
                }
            }
            SessionEvent::TurnStart {
                input,
                approval_policy,
                ..
            } => {
                self.turns += 1;
                if self.summary.is_empty() {
                    self.summary = one_line(&input.text());
                }
                self.approval_policy.clone_from(approval_policy);
            }
            SessionEvent::ModelRequest {
                messages,
                reasoning_effort,
                model,
                ..
            } => {
                // Only a turn's first step carries a `messages` snapshot;
                // later steps log an empty one (see `turn.rs`) and must not
                // move the baseline, or `load_session` would seek to a line
                // `history_from_log` cannot rebuild a history from.
                if !messages.is_empty() {
                    self.baseline = Some(at);
                }
                self.reasoning_effort = *reasoning_effort;
                if let Some(model) = model {
                    self.last_step_model = Some(model.clone());
                }
            }
            SessionEvent::ModelResponse { usage, .. } => {
                if usage.input_tokens > 0 {
                    self.context_input = usage.input_tokens;
                }
            }
            SessionEvent::TurnEnd { usage, .. } => {
                self.usage.add(*usage);
                if usage.input_tokens > 0 {
                    self.turn_input = usage.input_tokens;
                }
            }
            // A child's tokens are spent under the parent's turn, so they
            // belong on the parent's bill. Counted here and nowhere else: the
            // child's own `TurnEnd`s are in the child's log, which this fold
            // never reads, so the same tokens cannot be billed twice.
            SessionEvent::SubagentEnd { usage, .. } => self.usage.add(*usage),
            _ => {}
        }
    }

    /// The model the session was last talking to.
    ///
    /// The last step that named one wins over what the session opened with: a
    /// session can switch models mid-conversation, and resuming should pick up
    /// where it left off rather than where it started.
    #[must_use]
    pub fn current_model(&self) -> Option<String> {
        self.last_step_model.clone().or_else(|| self.model.clone())
    }

    /// How full the context window was on the last logged step.
    ///
    /// Some providers report nothing per step and only account at turn end, in
    /// which case that turn's figure is the best on record — a coarse number
    /// beats reporting zero.
    #[must_use]
    pub fn context(&self) -> u64 {
        if self.context_input > 0 {
            self.context_input
        } else {
            self.turn_input
        }
    }
}

/// Where the cache for `log` belongs.
///
/// A session that owns a directory keeps it inside; a log from the older flat
/// layout shares its directory with its siblings, so its cache is named after
/// it. One file per session either way — a single `meta.json` among flat logs
/// would describe whichever session wrote it last.
fn meta_path(log: &Path) -> Option<std::path::PathBuf> {
    if log.file_name()? == LOG_FILE {
        return Some(log.with_file_name(META_FILE));
    }
    Some(log.with_extension("meta.json"))
}

/// Flatten to one line, and cut it to something a listing column can hold.
pub(crate) fn one_line(text: &str) -> String {
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() <= 60 {
        return flattened;
    }
    let kept: String = flattened.chars().take(59).collect();
    format!("{kept}…")
}
