//! Reading a session back off disk.
//!
//! Invariant 6 says a model-visible input is reconstructable from the rollout
//! log, and this is the code that cashes that promise in: the history a resumed
//! session starts from is rebuilt from the log rather than from any side file,
//! so a session keke can replay is a session keke can continue.
//!
//! The rebuild leans on [`SessionEvent::ModelRequest`], which carries the whole
//! model-visible history for one step. Taking the last one and replaying only
//! the tail after it means a compaction, a system change, or a variant this
//! build does not know still lands correctly — whatever the model last saw is
//! what the resumed session sees.

use std::path::Path;
use std::path::PathBuf;

use keke_paths::AbsPath;
use keke_protocol::ContentBlock;
use keke_protocol::Message;
use keke_protocol::Role;
use keke_protocol::SessionEvent;
use keke_protocol::SessionId;
use keke_protocol::Usage;

use crate::RolloutError;
use crate::read_log;
use crate::read_log_from;

/// Where a home layout keeps its rollout logs.
#[must_use]
pub fn sessions_dir(home: &AbsPath) -> PathBuf {
    home.as_path().join("sessions")
}

/// Where one project's logs live: its rollouts and its prompt history together.
///
/// A session belongs to the directory it was started in, and so does the typing
/// history, so the two live side by side under one directory named after the
/// project rather than in two unrelated places.
#[must_use]
pub fn project_dir(home: &AbsPath, cwd: &Path) -> PathBuf {
    sessions_dir(home).join(encode_path(&cwd.display().to_string()))
}

/// Percent-encode everything outside the URL unreserved set.
///
/// Enough to make a path one directory name on every platform keke runs on:
/// separators, spaces, colons and non-ASCII all become escapes, and the result
/// is still readable enough to recognise the project by eye.
fn encode_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// One resumable session, as `keke resume --list` prints it.
#[derive(Clone, Debug)]
pub struct SessionSummary {
    pub id: SessionId,
    pub path: PathBuf,
    /// The directory the session was started in, from its `SessionStart`.
    pub cwd: Option<String>,
    /// RFC 3339, from the last line written. Empty for a log with no lines.
    pub updated_at: String,
    /// First thing the person said, for telling two sessions apart.
    pub summary: String,
    pub turns: usize,
    /// Set when this session is a subagent's, naming the session that spawned
    /// it. A child is not something a person continues.
    pub parent: Option<SessionId>,
}

/// The shortest handle anyone is asked to type. Below this, two homes that
/// differ only by a few sessions would print handles of different lengths for
/// the same session, which is worse than printing a character too many.
const MIN_ABBREVIATION: usize = 8;

impl SessionSummary {
    /// The trailing part of the id, cut to `width` — what a listing prints and
    /// what a person types back.
    ///
    /// The tail, not the head, because that is where a UUIDv7 keeps what makes
    /// it different. Its leading characters are a timestamp, and the `uuid`
    /// crate resolves ties inside one millisecond with a counter in the low
    /// bits: sessions started back to back agree to the last few characters and
    /// disagree only at the end. Abbreviating from the front asked a person to
    /// type 23 characters on a real home, and eight of them named five
    /// different sessions.
    ///
    /// The width comes from [`abbreviation`] over the whole listing rather than
    /// from this session alone: how much of an id is enough is a fact about
    /// what it is being told apart from.
    #[must_use]
    pub fn abbreviated(&self, width: usize) -> String {
        tail(&self.id.to_string(), width)
    }
}

/// The last `width` characters of `id`.
fn tail(id: &str, width: usize) -> String {
    let skip = id.chars().count().saturating_sub(width);
    id.chars().skip(skip).collect()
}

/// How much of an id a listing has to print for every row to name one session.
///
/// Git's rule, for git's reason: an id nobody can type back is not an id. What
/// is printed is what `keke resume` takes, so the listing has to print enough
/// to tell apart what it is printing — and no more.
///
/// Two rows can carry the same id: a session resumed under a different
/// directory logs under that project too. No width separates those, and the
/// full id would not either, so they are counted once.
#[must_use]
pub fn abbreviation(ids: impl IntoIterator<Item = SessionId>) -> usize {
    let ids: std::collections::HashSet<String> = ids.into_iter().map(|id| id.to_string()).collect();
    let full = ids.iter().map(String::len).max().unwrap_or(0);

    for width in MIN_ABBREVIATION..full {
        // A handle opening on a dash carries no more than the one before it,
        // and looks like something mistyped rather than something to copy.
        if ids.iter().any(|id| tail(id, width).starts_with('-')) {
            continue;
        }
        let mut seen = std::collections::HashSet::with_capacity(ids.len());
        if ids.iter().all(|id| seen.insert(tail(id, width))) {
            return width;
        }
    }
    full.max(MIN_ABBREVIATION)
}

/// Everything needed to continue a session.
#[derive(Clone, Debug)]
pub struct ResumedSession {
    pub id: SessionId,
    pub path: PathBuf,
    /// The model-visible history, as of the last logged model request.
    pub history: Vec<Message>,
    /// What the session has spent so far, summed over its turns.
    pub usage: Usage,
    /// Input tokens of the last logged model step. Each request resends the
    /// whole conversation, so a step's `input_tokens` is the context size, not
    /// an increment — this is how full the window is on resume. Distinct from
    /// `usage`, whose additive inputs answer "what did it cost", not "how full".
    pub context_input: u64,
    /// The working-tree snapshot each user turn started from, by turn ordinal.
    /// Empty for a log written with checkpoints off, which is an ordinary
    /// resume: the conversation still winds back, the files just cannot.
    pub snapshots: std::collections::BTreeMap<usize, String>,
    pub cwd: Option<String>,
    /// The model that answered the last logged step, if the log named one.
    /// Falls back to `SessionStart`'s model when no step did — a log written
    /// before `ModelRequest` carried its own model still says what the
    /// session opened with.
    pub model: Option<String>,
    /// How hard the model was last asked to think, from the last logged step
    /// that said. A log with no step naming one has none: the session ran on
    /// whatever the vendor defaults to, and resuming should too.
    pub reasoning_effort: Option<keke_protocol::ReasoningEffort>,
    /// The approval policy in force for the last logged turn, if the log named
    /// one. A log written before this field existed, or with no turn yet, has
    /// none — the caller falls back to configuration, same as it always did.
    pub approval_policy: Option<keke_config_types::ApprovalPolicy>,
}

/// What a name a person typed matched.
///
/// A prefix that could mean two sessions is reported as ambiguous rather than
/// resolved to the newest: invariant 8 — ambiguity fails loud — and the one
/// place it could bite is the one where keke would silently continue the wrong
/// conversation.
#[derive(Clone, Debug)]
pub enum SessionMatch {
    One(Box<SessionSummary>),
    /// Several sessions start with what was typed.
    Ambiguous(Vec<SessionSummary>),
    None,
}

/// Resolve what a person typed to one session.
///
/// A full id works, and so does either end of one — a UUID is not something
/// anyone retypes correctly. Both ends, because the two are what a person has
/// to hand: `--list` prints the tail, since that is where a UUIDv7 differs,
/// while an id pasted from a log path or an error message leads with its head.
/// Matching is case-insensitive and ignores dashes, so a handle copied with or
/// without them behaves the same.
pub fn find_session(home: &AbsPath, typed: &str) -> Result<SessionMatch, RolloutError> {
    let needle = normalize(typed);
    if needle.is_empty() {
        return Ok(SessionMatch::None);
    }
    // Matched on the name before anything is read: a handle names at most a
    // handful of sessions, and summarizing the rest to discard them is what
    // made resuming by id cost a scan of every log on disk.
    let matched: Vec<SessionSummary> = log_paths(home)?
        .into_iter()
        .filter(|log| {
            let id = normalize(&log.id.to_string());
            id.starts_with(&needle) || id.ends_with(&needle)
        })
        .filter_map(|log| summarize(&log).ok())
        // A log with no turns holds no conversation, so it is not a candidate
        // for continuing one — and counting it would make a prefix ambiguous
        // against something nobody could have meant. Neither is a subagent's:
        // it is a turn of some other session, not a session of its own.
        .filter(|session| session.turns > 0 && session.parent.is_none())
        .collect();

    let mut found = matched;
    Ok(match found.len() {
        0 => SessionMatch::None,
        1 => match found.pop() {
            Some(session) => SessionMatch::One(Box::new(session)),
            None => SessionMatch::None,
        },
        _ => SessionMatch::Ambiguous(found),
    })
}

fn normalize(id: &str) -> String {
    id.chars()
        .filter(|ch| *ch != '-')
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

/// One session's log on disk, found without reading it.
struct LogPath {
    id: SessionId,
    path: PathBuf,
}

/// Every session log under `home`, newest first, by name alone.
///
/// Ids are UUIDv7, so their string form already sorts chronologically; the
/// log's own name is a steadier key than an mtime a backup tool may rewrite.
/// Nothing here opens a file, which is what lets the callers that only need a
/// few sessions pay for only those.
///
/// Two layouts are recognised. A session owns a directory holding
/// `rollout.jsonl` and its cache; before that it was one `<id>.jsonl` beside
/// its siblings, and those keep working, without a cache, for as long as they
/// are on disk. There is no migration to run.
fn log_paths(home: &AbsPath) -> Result<Vec<LogPath>, RolloutError> {
    let dir = sessions_dir(home);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // No directory means no sessions, which is an empty list rather than a
        // failure: a first run has nothing to resume and that is not an error.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(RolloutError::Io {
                path: dir.display().to_string(),
                source,
            });
        }
    };

    let mut logs: Vec<LogPath> = Vec::new();
    for project in entries.filter_map(Result::ok).map(|entry| entry.path()) {
        let Ok(inner) = std::fs::read_dir(&project) else {
            continue;
        };
        for entry in inner.filter_map(Result::ok) {
            let path = entry.path();
            let Some(id) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| uuid::Uuid::parse_str(stem).ok())
                .map(SessionId::from)
            else {
                continue;
            };
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                logs.push(LogPath {
                    id,
                    path: path.join(crate::meta::LOG_FILE),
                });
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                logs.push(LogPath { id, path });
            }
        }
    }
    logs.sort_by_key(|log| std::cmp::Reverse(log.id));
    Ok(logs)
}

/// Every session under `home`, newest first.
///
/// A file that cannot be read at all is skipped rather than failing the
/// listing: one unreadable log must not hide the others.
pub fn list_sessions(home: &AbsPath) -> Result<Vec<SessionSummary>, RolloutError> {
    list_recent(home, usize::MAX)
}

/// The `limit` most recent sessions under `home`.
///
/// A surface that draws a page of sessions should ask for a page. Even reading
/// only the cache, a machine with thousands of sessions is thousands of opens,
/// and the ones past the first screen are opened for nothing.
pub fn list_recent(home: &AbsPath, limit: usize) -> Result<Vec<SessionSummary>, RolloutError> {
    Ok(log_paths(home)?
        .into_iter()
        .filter_map(|log| summarize(&log).ok())
        // A subagent's log is a turn of some other session. Showing it as a
        // session of its own offers to continue a conversation nobody had.
        .filter(|session| session.parent.is_none())
        .take(limit)
        .collect())
}

/// The most recent session that actually holds a conversation.
///
/// Sessions with no turns are skipped. Opening the interface writes a log
/// whether or not anybody says anything, so the newest file on disk is
/// routinely an empty one — resuming that instead would look exactly like keke
/// having lost the conversation the person meant.
pub fn latest_session(home: &AbsPath) -> Result<Option<SessionSummary>, RolloutError> {
    Ok(list_sessions(home)?
        .into_iter()
        .find(|session| session.turns > 0))
}

/// Read one session's log and rebuild what a session needs to continue it.
///
/// The history a resume starts from is the last `ModelRequest` plus everything
/// after it, so a log whose cache says where that line begins is read from
/// there. On a long session that is the difference between reading the last
/// turn and reading every turn twice over.
pub fn load_session(home: &AbsPath, id: SessionId) -> Result<ResumedSession, RolloutError> {
    let path = session_path(home, id)?;
    let meta = crate::meta::SessionMeta::refreshed(&path)?;
    meta.write(&path);

    let events: Vec<SessionEvent> = match meta.baseline {
        Some(from) => read_log_from(&path, from)?,
        // No step was ever logged, so the whole log is the tail: what a person
        // said before the first request still has to reach the resumed session.
        None => read_log(&path)?,
    }
    .into_iter()
    .map(|line| line.event)
    .collect();

    Ok(ResumedSession {
        id,
        cwd: meta.cwd.clone(),
        model: meta.current_model(),
        reasoning_effort: meta.reasoning_effort,
        approval_policy: meta
            .approval_policy
            .as_deref()
            .and_then(keke_config_types::ApprovalPolicy::parse),
        history: history_from_log(&events),
        usage: meta.usage,
        context_input: meta.context(),
        snapshots: meta.snapshots.clone(),
        path,
    })
}

/// Where one session's log is, in whichever project directory holds it.
///
/// The id does not say which project the session belongs to, so the directories
/// are what resolve it — by name, without opening anything. An id nothing on
/// disk answers to is a missing file.
fn session_path(home: &AbsPath, id: SessionId) -> Result<PathBuf, RolloutError> {
    log_paths(home)?
        .into_iter()
        .find(|log| log.id == id)
        .map(|log| log.path)
        .ok_or_else(|| RolloutError::Io {
            path: sessions_dir(home)
                .join(id.to_string())
                .display()
                .to_string(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no log for this session"),
        })
}

/// Describe one session from its cache, folding whatever the cache has not seen
/// and leaving the result behind for the next reader.
fn summarize(log: &LogPath) -> Result<SessionSummary, RolloutError> {
    let meta = crate::meta::SessionMeta::refreshed(&log.path)?;
    meta.write(&log.path);
    Ok(SessionSummary {
        id: log.id,
        path: log.path.clone(),
        cwd: meta.cwd,
        updated_at: meta.updated_at,
        summary: meta.summary,
        turns: meta.turns,
        parent: meta.parent,
    })
}

/// Rebuild the model-visible history from a log.
///
/// The last `ModelRequest` that carries a snapshot is the baseline — it is
/// the history the model actually saw as of that step — and everything
/// logged after it is replayed onto it in order. Only a turn's first step
/// logs a snapshot; later steps in the same turn log an empty `messages` and
/// are skipped here, since their contribution (a `ModelResponse` and any
/// `ToolCallEnd`s) is already replayed from the tail. A turn that was logged
/// but never reached the model (an error, a cancel before the first request)
/// contributes its input, so the person's words are never lost.
///
/// A `Rewound` is a baseline too, and an empty one counts: a rewind to before
/// the first prompt leaves nothing, and treating that as "no snapshot here"
/// would replay the whole log the person had just wound back.
#[must_use]
pub fn history_from_log(events: &[SessionEvent]) -> Vec<Message> {
    let baseline = events.iter().rposition(|event| match event {
        SessionEvent::ModelRequest { messages, .. } => !messages.is_empty(),
        // A files-only rewind left the conversation alone and is no baseline.
        SessionEvent::Rewound { history, .. } => history.is_some(),
        _ => false,
    });

    let (mut history, tail) = match baseline {
        Some(at) => match &events[at] {
            SessionEvent::ModelRequest { messages, .. } => (messages.clone(), &events[at + 1..]),
            SessionEvent::Rewound {
                history: Some(history),
                ..
            } => (history.clone(), &events[at + 1..]),
            _ => unreachable!("rposition matched a snapshot"),
        },
        None => (Vec::new(), events),
    };

    let mut results: Vec<ContentBlock> = Vec::new();
    for event in tail {
        match event {
            SessionEvent::TurnStart { input, .. } => {
                flush(&mut history, &mut results);
                history.push(input.clone());
            }
            SessionEvent::ModelResponse { message, .. } => {
                flush(&mut history, &mut results);
                history.push(message.clone());
            }
            SessionEvent::ToolCallEnd { result, .. } => {
                results.push(ContentBlock::ToolResult(result.clone()));
            }
            _ => {}
        }
    }
    flush(&mut history, &mut results);
    history
}

/// Close the open batch of tool results as one `Tool` message.
///
/// One message per batch rather than per result, because that is how the turn
/// loop writes them and a wire that pairs a call with its answer positionally
/// would otherwise see a different shape on resume than it did live.
fn flush(history: &mut Vec<Message>, results: &mut Vec<ContentBlock>) {
    if results.is_empty() {
        return;
    }
    history.push(Message {
        role: Role::Tool,
        content: std::mem::take(results),
    });
}

/// What a session has spent, summed over the turns that finished and the
/// subagents they delegated to.
///
/// `TurnEnd` and `SubagentEnd` only. A step's usage is counted again by the
/// `TurnEnd` that closes its turn, and a child's `TurnEnd`s are in the child's
/// log, which this never reads — so no token here is billed twice.
#[must_use]
pub fn usage_from_log(events: &[SessionEvent]) -> Usage {
    let mut total = Usage::default();
    for event in events {
        match event {
            SessionEvent::TurnEnd { usage, .. } | SessionEvent::SubagentEnd { usage, .. } => {
                total.add(*usage);
            }
            _ => {}
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use keke_protocol::StopReason;
    use keke_protocol::ToolCall;
    use keke_protocol::ToolCallId;
    use keke_protocol::ToolResult;
    use keke_protocol::TurnId;

    use super::*;

    fn result(id: &str) -> ToolResult {
        ToolResult::ok(ToolCallId::new(id), "done")
    }

    /// The baseline is what the model last saw, so anything a compaction
    /// elided stays elided rather than coming back on resume.
    #[test]
    fn the_last_model_request_is_the_baseline() {
        let turn = TurnId::new();
        let events = vec![
            SessionEvent::TurnStart {
                turn,
                input: Message::user("first"),
                approval_policy: None,
            },
            SessionEvent::ModelRequest {
                turn,
                messages: vec![Message::user("summary of everything before")],
                tools: Vec::new(),
                reasoning_effort: None,
                model: None,
            },
        ];
        let history = history_from_log(&events);
        assert_eq!(history, vec![Message::user("summary of everything before")]);
    }

    /// A call the model made and the answer it got are both model-visible, so
    /// both have to be there — a resumed session that dropped the result would
    /// send a call nobody answered.
    #[test]
    fn the_tail_after_the_last_request_is_replayed() {
        let turn = TurnId::new();
        let call = ToolCall {
            id: ToolCallId::new("c1"),
            name: "read_file".to_string(),
            arguments: serde_json::Value::Null,
        };
        let reply = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolCall(call.clone())],
        };
        let events = vec![
            SessionEvent::ModelRequest {
                turn,
                messages: vec![Message::user("read it")],
                tools: Vec::new(),
                reasoning_effort: None,
                model: None,
            },
            SessionEvent::ModelResponse {
                turn,
                message: reply.clone(),
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            },
            SessionEvent::ToolCallStart { turn, call },
            SessionEvent::ToolCallEnd {
                turn,
                result: result("c1"),
            },
        ];

        let history = history_from_log(&events);
        assert_eq!(history.len(), 3);
        assert_eq!(history[1], reply);
        assert_eq!(history[2].role, Role::Tool);
    }

    /// A turn's later steps log an empty `messages` snapshot (see `turn.rs`)
    /// to avoid re-logging the whole history on every step. Those events must
    /// not be picked as the baseline — the last one that actually carries a
    /// snapshot still is, and the empty ones are skipped over like any other
    /// tail event.
    #[test]
    fn a_later_steps_empty_snapshot_is_not_the_baseline() {
        let turn = TurnId::new();
        let call = ToolCall {
            id: ToolCallId::new("c1"),
            name: "read_file".to_string(),
            arguments: serde_json::Value::Null,
        };
        let reply = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolCall(call.clone())],
        };
        let events = vec![
            SessionEvent::ModelRequest {
                turn,
                messages: vec![Message::user("read it")],
                tools: Vec::new(),
                reasoning_effort: None,
                model: None,
            },
            SessionEvent::ModelResponse {
                turn,
                message: reply.clone(),
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            },
            SessionEvent::ToolCallStart { turn, call },
            SessionEvent::ToolCallEnd {
                turn,
                result: result("c1"),
            },
            // The second step's request: no fresh snapshot.
            SessionEvent::ModelRequest {
                turn,
                messages: Vec::new(),
                tools: Vec::new(),
                reasoning_effort: None,
                model: None,
            },
        ];

        let history = history_from_log(&events);
        assert_eq!(history.len(), 3);
        assert_eq!(history[0], Message::user("read it"));
        assert_eq!(history[1], reply);
        assert_eq!(history[2].role, Role::Tool);
    }

    /// Two results from one step are one message, the same shape the live turn
    /// loop writes.
    #[test]
    fn one_batch_of_results_is_one_message() {
        let turn = TurnId::new();
        let events = vec![
            SessionEvent::ToolCallEnd {
                turn,
                result: result("c1"),
            },
            SessionEvent::ToolCallEnd {
                turn,
                result: result("c2"),
            },
        ];
        let history = history_from_log(&events);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content.len(), 2);
    }

    /// A turn that failed before reaching the model still said something.
    #[test]
    fn an_input_that_never_reached_the_model_survives() {
        let turn = TurnId::new();
        let events = vec![
            SessionEvent::TurnStart {
                turn,
                input: Message::user("hello"),
                approval_policy: None,
            },
            SessionEvent::Error {
                turn: Some(turn),
                message: "the provider is down".to_string(),
            },
        ];
        assert_eq!(history_from_log(&events), vec![Message::user("hello")]);
    }

    /// The events one line each, as the recorder would have written them.
    fn as_log(events: &[SessionEvent]) -> String {
        let mut log = String::new();
        for event in events {
            let envelope = keke_protocol::SessionEventEnvelope {
                at: "2026-08-23T00:00:00Z".to_string(),
                event: event.clone(),
            };
            log.push_str(&serde_json::to_string(&envelope).expect("serialize"));
            log.push('\n');
        }
        log
    }

    fn turns_of(count: usize) -> Vec<SessionEvent> {
        (0..count)
            .map(|_| SessionEvent::TurnStart {
                turn: TurnId::new(),
                input: Message::user("hi"),
                approval_policy: None,
            })
            .collect()
    }

    fn empty_home() -> (tempfile::TempDir, AbsPath) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonicalize");
        let home = AbsPath::new(root).expect("absolute");
        std::fs::create_dir_all(sessions_dir(&home)).expect("sessions dir");
        (dir, home)
    }

    /// Write one session's log in the layout the recorder writes, with no
    /// cache beside it: every reader has to work from the log alone.
    fn write_session(home: &AbsPath, id: SessionId, events: &[SessionEvent]) {
        let dir = project_dir(home, Path::new("/Users/x/projects/keke")).join(id.to_string());
        std::fs::create_dir_all(&dir).expect("session dir");
        std::fs::write(dir.join(crate::meta::LOG_FILE), as_log(events)).expect("write");
    }

    /// Write a log holding `turns` turns, and return its home.
    fn home_with(sessions: &[(SessionId, usize)]) -> (tempfile::TempDir, AbsPath) {
        let (dir, home) = empty_home();
        for (id, turns) in sessions {
            write_session(&home, *id, &turns_of(*turns));
        }
        (dir, home)
    }

    /// Fold events into the cache the way a reader of that log would.
    fn folded(events: &[SessionEvent]) -> crate::meta::SessionMeta {
        let (_dir, home) = empty_home();
        let id = SessionId::new();
        write_session(&home, id, events);
        let path = project_dir(&home, Path::new("/Users/x/projects/keke"))
            .join(id.to_string())
            .join(crate::meta::LOG_FILE);
        crate::meta::SessionMeta::refreshed(&path).expect("folds")
    }

    /// A session's log sits under the project directory that holds the typing
    /// history, one level down in the directory the session owns.
    #[tokio::test]
    async fn a_log_is_written_under_the_project_prompt_history() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonicalize");
        let home = AbsPath::new(root).expect("absolute");
        let cwd = Path::new("/Users/x/projects/keke");
        let id = SessionId::new();

        let recorder = crate::RolloutRecorder::create(&home, cwd, id)
            .await
            .expect("creates");

        assert_eq!(
            recorder.path().parent().and_then(Path::parent),
            crate::prompt_history_path(&home, cwd).parent()
        );
        assert_eq!(
            recorder.path().parent().and_then(Path::file_name),
            Some(std::ffi::OsStr::new(&id.to_string())),
            "a session owns the directory its log lives in"
        );
    }

    /// Nobody retypes a UUID, so any prefix of one resolves.
    #[test]
    fn a_prefix_resolves_to_the_one_session_it_names() {
        let id = SessionId::new();
        let (_dir, home) = home_with(&[(id, 1)]);
        let typed: String = id.to_string().chars().take(8).collect();
        assert!(matches!(
            find_session(&home, &typed).expect("reads"),
            SessionMatch::One(session) if session.id == id
        ));
    }

    /// Invariant 8: a prefix that could mean either is an error, not a pick.
    #[test]
    fn a_prefix_matching_two_sessions_is_ambiguous() {
        let (_dir, home) = home_with(&[(SessionId::new(), 1), (SessionId::new(), 1)]);
        // Every id here is a UUIDv7 minted in the same millisecond range, so
        // the first character is shared; that is enough to be ambiguous.
        let shared = "0";
        assert!(matches!(
            find_session(&home, shared).expect("reads"),
            SessionMatch::Ambiguous(candidates) if candidates.len() == 2
        ));
    }

    /// Opening the interface writes a log whether or not anybody speaks.
    /// Resuming that instead of the last real conversation looks exactly like
    /// keke having lost it.
    #[test]
    fn an_empty_session_is_never_what_gets_resumed() {
        let spoken = SessionId::new();
        let empty = SessionId::new();
        let (_dir, home) = home_with(&[(spoken, 2), (empty, 0)]);
        assert!(empty > spoken, "the empty log has to be the newer one");

        let latest = latest_session(&home).expect("reads").expect("a session");
        assert_eq!(latest.id, spoken);
        assert!(matches!(
            find_session(&home, &empty.to_string()).expect("reads"),
            SessionMatch::None
        ));
    }

    /// The cache is derived, so throwing it away must change no answer. This
    /// is the property that keeps `meta.json` from becoming a second source of
    /// truth (`AGENTS.md` invariant 6).
    #[test]
    fn a_listing_is_the_same_with_the_cache_and_without_it() {
        let id = SessionId::new();
        let (_dir, home) = home_with(&[(id, 3)]);

        let cold = list_sessions(&home).expect("reads");
        let cache = project_dir(&home, Path::new("/Users/x/projects/keke"))
            .join(id.to_string())
            .join(crate::meta::META_FILE);
        assert!(cache.is_file(), "a reader leaves a cache behind");
        let warm = list_sessions(&home).expect("reads");
        std::fs::remove_file(&cache).expect("removes");
        let again = list_sessions(&home).expect("reads");

        for listing in [&warm, &again] {
            assert_eq!(listing.len(), cold.len());
            assert_eq!(listing[0].id, cold[0].id);
            assert_eq!(listing[0].turns, cold[0].turns);
            assert_eq!(listing[0].summary, cold[0].summary);
        }
    }

    /// The fold is incremental, so a session that grew by a turn must count
    /// that turn once — not again from the start, and not twice.
    #[test]
    fn folding_a_grown_log_counts_only_what_is_new() {
        let id = SessionId::new();
        let (_dir, home) = home_with(&[(id, 2)]);
        assert_eq!(list_sessions(&home).expect("reads")[0].turns, 2);

        let log = project_dir(&home, Path::new("/Users/x/projects/keke"))
            .join(id.to_string())
            .join(crate::meta::LOG_FILE);
        let mut text = std::fs::read_to_string(&log).expect("reads");
        text.push_str(&as_log(&turns_of(1)));
        std::fs::write(&log, text).expect("writes");

        assert_eq!(list_sessions(&home).expect("reads")[0].turns, 3);
        assert_eq!(list_sessions(&home).expect("reads")[0].turns, 3);
    }

    /// A subagent's log has the shape of a session and is not one: it is a
    /// turn of the session that spawned it. Offering it would invite a person
    /// to continue a conversation nobody had.
    #[test]
    fn a_subagent_s_log_is_not_offered_for_resume() {
        let (_dir, home) = empty_home();
        let parent = SessionId::new();
        let child = SessionId::new();
        write_session(&home, parent, &turns_of(1));
        let mut child_log = vec![SessionEvent::SessionStart {
            cwd: "/Users/x/projects/keke".to_string(),
            provider: "test".to_string(),
            model: "test-model".to_string(),
            parent: Some(parent),
        }];
        child_log.extend(turns_of(1));
        write_session(&home, child, &child_log);

        let listed: Vec<SessionId> = list_sessions(&home)
            .expect("reads")
            .into_iter()
            .map(|session| session.id)
            .collect();
        assert_eq!(listed, vec![parent]);
        assert!(matches!(
            find_session(&home, &child.to_string()).expect("reads"),
            SessionMatch::None
        ));
    }

    /// A child's tokens are the parent's, counted where the parent logged
    /// them and nowhere else — the child's own turns are in the child's log,
    /// which the parent's fold never reads.
    #[test]
    fn a_subagent_s_tokens_are_billed_to_its_parent_once() {
        let turn = TurnId::new();
        let spent = Usage {
            input_tokens: 100,
            output_tokens: 10,
            ..Usage::default()
        };
        let events = vec![
            SessionEvent::TurnEnd {
                turn,
                stop_reason: StopReason::EndTurn,
                usage: spent,
            },
            SessionEvent::SubagentEnd {
                turn,
                agent: "agent_1".to_string(),
                session: Some(SessionId::new()),
                status: "completed".to_string(),
                summary: "done".to_string(),
                usage: spent,
            },
        ];
        assert_eq!(folded(&events).usage.total(), 220);
        assert_eq!(usage_from_log(&events).total(), 220);
    }

    /// The history a resume rebuilds is the last request plus its tail, so
    /// reading from the cached offset has to produce exactly what reading the
    /// whole log produces.
    #[test]
    fn a_resume_from_the_cached_offset_rebuilds_the_same_history() {
        let (_dir, home) = empty_home();
        let id = SessionId::new();
        let turn = TurnId::new();
        let events = vec![
            SessionEvent::TurnStart {
                turn,
                input: Message::user("first"),
                approval_policy: Some("never".to_string()),
            },
            SessionEvent::ModelRequest {
                turn,
                messages: vec![Message::user("stale")],
                tools: Vec::new(),
                reasoning_effort: None,
                model: None,
            },
            SessionEvent::ModelRequest {
                turn,
                messages: vec![Message::user("what the model last saw")],
                tools: Vec::new(),
                reasoning_effort: None,
                model: Some("newer-model".to_string()),
            },
            SessionEvent::ModelResponse {
                turn,
                message: Message::assistant("answered"),
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 4_242,
                    ..Usage::default()
                },
            },
        ];
        write_session(&home, id, &events);

        let resumed = load_session(&home, id).expect("loads");
        assert_eq!(
            resumed.history,
            vec![
                Message::user("what the model last saw"),
                Message::assistant("answered"),
            ]
        );
        assert_eq!(resumed.model.as_deref(), Some("newer-model"));
        assert_eq!(resumed.context_input, 4_242);
        assert_eq!(
            resumed.approval_policy,
            Some(keke_config_types::ApprovalPolicy::Never),
            "the last turn's policy is before the baseline and still restored"
        );
    }

    /// Sessions written before a session owned a directory are still listed
    /// and still resumable. There is no migration to run.
    #[test]
    fn a_log_from_the_flat_layout_is_still_resumable() {
        let (_dir, home) = empty_home();
        let id = SessionId::new();
        let dir = project_dir(&home, Path::new("/Users/x/projects/keke"));
        std::fs::create_dir_all(&dir).expect("project dir");
        std::fs::write(dir.join(format!("{id}.jsonl")), as_log(&turns_of(2))).expect("write");

        assert_eq!(list_sessions(&home).expect("reads")[0].turns, 2);
        assert!(matches!(
            find_session(&home, &id.to_string()).expect("reads"),
            SessionMatch::One(session) if session.id == id
        ));
        assert!(
            dir.join(format!("{id}.meta.json")).is_file(),
            "a flat log shares its directory, so its cache is named after it"
        );
        assert!(
            !dir.join(crate::meta::META_FILE).exists(),
            "one cache per session: never one file the siblings overwrite"
        );
        // A second session in the same directory keeps its own.
        let other = SessionId::new();
        std::fs::write(dir.join(format!("{other}.jsonl")), as_log(&turns_of(5))).expect("write");
        let listed = list_sessions(&home).expect("reads");
        let turns: Vec<usize> = listed.iter().map(|session| session.turns).collect();
        assert_eq!(turns.iter().copied().max(), Some(5));
        assert_eq!(turns.iter().copied().min(), Some(2));
    }

    /// A surface that draws a page asks for a page, and pays for a page.
    #[test]
    fn a_limited_listing_stops_at_the_limit() {
        let (_dir, home) = home_with(&[
            (SessionId::new(), 1),
            (SessionId::new(), 1),
            (SessionId::new(), 1),
        ]);
        assert_eq!(list_recent(&home, 2).expect("reads").len(), 2);
        assert_eq!(list_sessions(&home).expect("reads").len(), 3);
    }

    /// Ids minted close together share their leading characters, because a
    /// UUIDv7 opens with a timestamp. A listing that printed eight of them
    /// would print one name for two sessions, and `keke resume` would refuse
    /// exactly what the listing told the person to type.
    #[test]
    fn a_listing_prints_enough_of_an_id_to_resume_by_it() {
        let ids: Vec<SessionId> = (0..8).map(|_| SessionId::new()).collect();
        let width = abbreviation(ids.iter().copied());

        let (_dir, home) = empty_home();
        for id in &ids {
            write_session(&home, *id, &turns_of(1));
        }

        let mut seen = std::collections::HashSet::new();
        for id in &ids {
            let printed = SessionSummary {
                id: *id,
                path: PathBuf::new(),
                updated_at: String::new(),
                summary: String::new(),
                turns: 1,
                parent: None,
                cwd: None,
            }
            .abbreviated(width);
            assert!(!printed.starts_with('-'), "{printed} opens on a dash");
            assert!(seen.insert(printed.clone()), "{printed} names two sessions");

            // What is printed is what resume takes back.
            assert!(
                matches!(
                    find_session(&home, &printed).expect("reads"),
                    SessionMatch::One(session) if session.id == *id
                ),
                "`{printed}` does not resolve to the session it names"
            );
        }
    }

    /// Nothing is gained by printing more of an id than tells them apart.
    #[test]
    fn one_session_is_abbreviated_to_the_minimum() {
        assert_eq!(abbreviation([SessionId::new()]), MIN_ABBREVIATION);
        assert_eq!(abbreviation([]), MIN_ABBREVIATION);
    }

    #[test]
    fn a_prefix_ignores_dashes_and_case() {
        assert_eq!(normalize("01A0-2D66"), "01a02d66");
    }

    /// A person switching approval modes mid-session means the switch to
    /// survive a resume, not the mode the session opened with.
    #[test]
    fn the_last_turn_s_approval_policy_is_what_resume_restores() {
        let events = vec![
            SessionEvent::TurnStart {
                turn: TurnId::new(),
                input: Message::user("first"),
                approval_policy: Some("on-request".to_string()),
            },
            SessionEvent::TurnStart {
                turn: TurnId::new(),
                input: Message::user("second"),
                approval_policy: Some("never".to_string()),
            },
        ];
        assert_eq!(folded(&events).approval_policy.as_deref(), Some("never"));
    }

    /// A log written before this field existed has no opinion, and resuming
    /// it must fall back to configuration rather than defaulting silently to
    /// one specific mode.
    #[test]
    fn a_log_with_no_approval_policy_restores_none() {
        let events = vec![SessionEvent::TurnStart {
            turn: TurnId::new(),
            input: Message::user("hi"),
            approval_policy: None,
        }];
        assert_eq!(folded(&events).approval_policy, None);
    }

    /// The summed turn figures answer "what did it cost"; the context window
    /// question needs the last single step, whose input is the whole context.
    #[test]
    fn the_context_figure_is_the_last_step_not_the_sum() {
        let step = Usage {
            input_tokens: 254_935,
            output_tokens: 1_188,
            ..Usage::default()
        };
        let earlier = Usage {
            input_tokens: 900_000,
            ..Usage::default()
        };
        let events = vec![
            SessionEvent::TurnEnd {
                turn: TurnId::new(),
                stop_reason: StopReason::EndTurn,
                usage: earlier,
            },
            SessionEvent::ModelResponse {
                turn: TurnId::new(),
                message: Message::assistant("latest"),
                stop_reason: StopReason::EndTurn,
                usage: step,
            },
            SessionEvent::TurnEnd {
                turn: TurnId::new(),
                stop_reason: StopReason::Cancelled,
                usage: step,
            },
        ];
        assert_eq!(folded(&events).context(), 254_935);
        assert_eq!(usage_from_log(&events).input_tokens, 1_154_935);

        // A provider that accounts only at turn end still gets a figure.
        let silent = [SessionEvent::TurnEnd {
            turn: TurnId::new(),
            stop_reason: StopReason::Cancelled,
            usage: step,
        }];
        assert_eq!(folded(&silent).context(), 254_935);
        assert_eq!(folded(&[]).context(), 0);
    }

    #[test]
    fn usage_sums_the_turns() {
        let turn = TurnId::new();
        let step = Usage {
            input_tokens: 10,
            output_tokens: 3,
            ..Usage::default()
        };
        let events = vec![
            SessionEvent::TurnEnd {
                turn,
                stop_reason: StopReason::EndTurn,
                usage: step,
            },
            SessionEvent::TurnEnd {
                turn,
                stop_reason: StopReason::EndTurn,
                usage: step,
            },
        ];
        assert_eq!(usage_from_log(&events).total(), 26);
    }
}
