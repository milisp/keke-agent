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

/// Where a home layout keeps its rollout logs.
#[must_use]
pub fn sessions_dir(home: &AbsPath) -> PathBuf {
    home.as_path().join("sessions")
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
}

impl SessionSummary {
    /// The leading part of the id, which is what `--list` prints and what a
    /// person types back. Long enough to be unique in practice, short enough to
    /// copy by eye.
    #[must_use]
    pub fn short_id(&self) -> String {
        self.id.to_string().chars().take(8).collect()
    }
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
    pub cwd: Option<String>,
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
/// A full id works, and so does any prefix of one — a UUID is not something
/// anyone retypes correctly, and `--list` prints the short form for exactly
/// this. Matching is case-insensitive and ignores dashes, so a prefix copied
/// with or without them behaves the same.
pub fn find_session(home: &AbsPath, typed: &str) -> Result<SessionMatch, RolloutError> {
    let needle = normalize(typed);
    if needle.is_empty() {
        return Ok(SessionMatch::None);
    }
    let matched: Vec<SessionSummary> = list_sessions(home)?
        .into_iter()
        // A log with no turns holds no conversation, so it is not a candidate
        // for continuing one — and counting it would make a prefix ambiguous
        // against something nobody could have meant.
        .filter(|session| session.turns > 0)
        .filter(|session| normalize(&session.id.to_string()).starts_with(&needle))
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

/// Every session under `home`, newest first.
///
/// A file that cannot be read at all is skipped rather than failing the
/// listing: one unreadable log must not hide the others.
pub fn list_sessions(home: &AbsPath) -> Result<Vec<SessionSummary>, RolloutError> {
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

    let mut sessions: Vec<SessionSummary> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|path| summarize(&path).ok())
        .collect();
    // Ids are UUIDv7, so their string form already sorts chronologically; the
    // log's own name is a steadier key than a mtime a backup tool may rewrite.
    sessions.sort_by_key(|session| std::cmp::Reverse(session.id));
    Ok(sessions)
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
pub fn load_session(home: &AbsPath, id: SessionId) -> Result<ResumedSession, RolloutError> {
    let path = sessions_dir(home).join(format!("{id}.jsonl"));
    let envelopes = read_log(&path)?;
    let events: Vec<SessionEvent> = envelopes.into_iter().map(|line| line.event).collect();
    Ok(ResumedSession {
        id,
        cwd: started_in(&events),
        history: history_from_log(&events),
        usage: usage_from_log(&events),
        path,
    })
}

fn summarize(path: &Path) -> Result<SessionSummary, RolloutError> {
    let id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| uuid::Uuid::parse_str(stem).ok())
        .map(SessionId::from)
        .ok_or_else(|| RolloutError::Io {
            path: path.display().to_string(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "a session log is named by its id",
            ),
        })?;

    let envelopes = read_log(path)?;
    let updated_at = envelopes
        .last()
        .map(|line| line.at.clone())
        .unwrap_or_default();
    let events: Vec<SessionEvent> = envelopes.into_iter().map(|line| line.event).collect();
    let turns = events
        .iter()
        .filter(|event| matches!(event, SessionEvent::TurnStart { .. }))
        .count();
    let summary = events
        .iter()
        .find_map(|event| match event {
            SessionEvent::TurnStart { input, .. } => Some(one_line(&input.text())),
            _ => None,
        })
        .unwrap_or_default();

    Ok(SessionSummary {
        id,
        path: path.to_path_buf(),
        cwd: started_in(&events),
        updated_at,
        summary,
        turns,
    })
}

fn started_in(events: &[SessionEvent]) -> Option<String> {
    events.iter().find_map(|event| match event {
        SessionEvent::SessionStart { cwd, .. } => Some(cwd.clone()),
        _ => None,
    })
}

/// Rebuild the model-visible history from a log.
///
/// The last `ModelRequest` is the baseline — it is the history the model
/// actually saw — and everything logged after it is replayed onto it in order.
/// A turn that was logged but never reached the model (an error, a cancel
/// before the first request) contributes its input, so the person's words are
/// never lost.
#[must_use]
pub fn history_from_log(events: &[SessionEvent]) -> Vec<Message> {
    let baseline = events
        .iter()
        .rposition(|event| matches!(event, SessionEvent::ModelRequest { .. }));

    let (mut history, tail) = match baseline {
        Some(at) => match &events[at] {
            SessionEvent::ModelRequest { messages, .. } => (messages.clone(), &events[at + 1..]),
            _ => unreachable!("rposition matched a model request"),
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

/// What a session has spent, summed over the turns that finished.
#[must_use]
pub fn usage_from_log(events: &[SessionEvent]) -> Usage {
    let mut total = Usage::default();
    for event in events {
        if let SessionEvent::TurnEnd { usage, .. } = event {
            total.add(*usage);
        }
    }
    total
}

fn one_line(text: &str) -> String {
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() <= 60 {
        return flattened;
    }
    let kept: String = flattened.chars().take(59).collect();
    format!("{kept}…")
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
            },
            SessionEvent::Error {
                turn: Some(turn),
                message: "the provider is down".to_string(),
            },
        ];
        assert_eq!(history_from_log(&events), vec![Message::user("hello")]);
    }

    /// Write a log holding `turns` turns, and return its home.
    fn home_with(sessions: &[(SessionId, usize)]) -> (tempfile::TempDir, AbsPath) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonicalize");
        let home = AbsPath::new(root).expect("absolute");
        std::fs::create_dir_all(sessions_dir(&home)).expect("sessions dir");

        for (id, turns) in sessions {
            let mut log = String::new();
            for _ in 0..*turns {
                let envelope = keke_protocol::SessionEventEnvelope {
                    at: "2026-08-23T00:00:00Z".to_string(),
                    event: SessionEvent::TurnStart {
                        turn: TurnId::new(),
                        input: Message::user("hi"),
                    },
                };
                log.push_str(&serde_json::to_string(&envelope).expect("serialize"));
                log.push('\n');
            }
            std::fs::write(sessions_dir(&home).join(format!("{id}.jsonl")), log).expect("write");
        }
        (dir, home)
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

    #[test]
    fn a_prefix_ignores_dashes_and_case() {
        assert_eq!(normalize("01A0-2D66"), "01a02d66");
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
