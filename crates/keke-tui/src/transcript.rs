//! What the scrollback holds.
//!
//! The transcript is an ordered list of cells rather than a string, because a
//! tool call has to be *revised* when its result arrives. A surface that
//! appended a line per event would leave the reader scrolling back to find out
//! whether the edit it approved actually happened.

use keke_acp::PermissionAnswer;
use keke_acp::PermissionId;
use keke_protocol::ContentBlock;
use keke_protocol::Message;
use keke_protocol::Role;
use keke_protocol::ToolCall;
use keke_protocol::ToolCallId;
use keke_protocol::ToolResult;
use keke_protocol::ToolStatus;

/// How a tool call is currently doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallState {
    Running,
    Finished(ToolStatus),
}

/// A tool call and, once known, its outcome.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCell {
    pub id: ToolCallId,
    pub name: String,
    /// The one thing worth reading in the arguments — a path, a command.
    /// This is the collapsed view: `read src/app.rs`, not `read path=…`.
    pub summary: String,
    /// Every argument as `key=value`, for the expanded view.
    pub arguments: String,
    pub state: CallState,
    /// First line of the result, so success is confirmable without expanding.
    pub detail: Option<String>,
}

/// An approval request and, once given, the answer.
#[derive(Clone, Debug, PartialEq)]
pub struct PermissionCell {
    pub id: PermissionId,
    pub name: String,
    pub summary: String,
    pub reason: String,
    /// `None` while the turn is blocked on this prompt.
    pub answer: Option<PermissionAnswer>,
}

/// A plan the agent asked to leave plan mode with.
///
/// It is a cell rather than an overlay because a plan is something the agent
/// said: it belongs in the scrollback with everything else said this session,
/// where it can be scrolled back to, selected, and copied long after it was
/// answered. It carries the permission id, so answering the plan answers the
/// call that proposed it.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanCell {
    pub id: PermissionId,
    pub text: String,
    /// Where the plan was saved, when it could be.
    pub path: Option<std::path::PathBuf>,
    /// `None` while the turn is blocked on it.
    pub answer: Option<PermissionAnswer>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Cell {
    Plan(PlanCell),
    User(String),
    Assistant(String),
    Tool(ToolCell),
    Permission(PermissionCell),
    /// A `Update::Failed`, or a local error. Never terminal.
    Error(String),
    /// Out-of-band host chatter — a login URL, a device code.
    Notice(String),
    /// The startup banner: pre-laid-out lines, shown once at the top of a
    /// fresh scrollback. Held as whole lines rather than wrapped prose since
    /// its icon column has to stay aligned with the text beside it.
    Banner(Vec<String>),
}

#[derive(Debug, Default)]
pub struct Transcript {
    cells: Vec<Cell>,
    /// Set by [`Transcript::seal`]; makes the next delta open a new cell
    /// instead of extending the one already on screen.
    sealed: bool,
    /// Path components from the workspace root down to the directory the
    /// session was launched from. Tool paths arrive workspace-relative — the
    /// root is what the model needs, unambiguous regardless of where a
    /// person happened to launch keke — but a person reads paths against
    /// where they are sitting, so the transcript re-roots them for display.
    /// Empty when the session was launched from the workspace root itself,
    /// which is the common case and needs no rewriting.
    cwd_prefix: Vec<String>,
}

impl Transcript {
    /// Re-root tool paths for display against `cwd` rather than the
    /// workspace root. Failure to resolve either just means no rewriting —
    /// paths still display, workspace-relative, as they always did.
    pub fn with_cwd(cwd: &keke_paths::AbsPath) -> Self {
        let cwd_prefix = keke_config::resolve_workspace_root(cwd.as_path())
            .ok()
            .and_then(|root| cwd.strip_root(&root).ok())
            .map(|rel| {
                rel.as_str()
                    .split('/')
                    .filter(|part| !part.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            cwd_prefix,
            ..Self::default()
        }
    }

    /// Drop everything shown so far, keeping the cwd rewrite in force.
    ///
    /// A person clearing the view is not asking the agent to forget — only
    /// the on-screen record resets, so a fresh `Transcript::default()` here
    /// would be wrong twice over: it drops the rollout along with the cwd
    /// rewrite this session was constructed with.
    pub fn clear(&mut self) {
        self.cells.clear();
        self.sealed = false;
    }

    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub fn last(&self) -> Option<&Cell> {
        self.cells.last()
    }

    /// Whether a person has sent anything yet. The startup banner pushes a
    /// `Cell::Banner` before the first prompt, so this is not `!is_empty()` —
    /// it specifically means "the conversation has started".
    pub fn has_user_message(&self) -> bool {
        self.cells.iter().any(|cell| matches!(cell, Cell::User(_)))
    }

    pub fn push(&mut self, cell: Cell) {
        self.cells.push(cell);
        self.sealed = true;
    }

    /// Append visible text, growing the assistant message already in progress.
    ///
    /// One turn of prose is one cell: a delta per cell would make wrapping and
    /// copy-out wrong for anyone whose provider chunks by token.
    pub fn push_text_delta(&mut self, delta: &str) {
        match self.cells.last_mut() {
            Some(Cell::Assistant(text)) if !self.sealed => text.push_str(delta),
            _ => {
                self.cells.push(Cell::Assistant(delta.to_string()));
                self.sealed = false;
            }
        }
    }

    /// Close the open prose cell so the next delta starts a fresh one.
    ///
    /// Called at turn boundaries and whenever a tool interrupts, which is what
    /// keeps two separate answers from fusing into one paragraph.
    pub fn seal(&mut self) {
        self.sealed = true;
    }

    pub fn start_tool(&mut self, call: &ToolCall) {
        self.sealed = true;
        self.cells.push(Cell::Tool(ToolCell {
            id: call.id.clone(),
            name: call.name.clone(),
            summary: headline(&call.arguments, &self.cwd_prefix),
            arguments: summarize_arguments(&call.arguments),
            state: CallState::Running,
            detail: None,
        }));
    }

    /// Revise the cell the call opened. Returns whether one was found.
    pub fn finish_tool(&mut self, result: &ToolResult) -> bool {
        let Some(cell) = self.cells.iter_mut().rev().find_map(|cell| match cell {
            Cell::Tool(tool) if tool.id == result.id && tool.state == CallState::Running => {
                Some(tool)
            }
            _ => None,
        }) else {
            return false;
        };
        let name = cell.name.clone();
        cell.state = CallState::Finished(result.status);
        cell.detail = detail_line(&name, result);
        true
    }

    pub fn request_permission(&mut self, id: PermissionId, call: &ToolCall, reason: String) {
        self.sealed = true;
        self.cells.push(Cell::Permission(PermissionCell {
            id,
            name: call.name.clone(),
            summary: headline(&call.arguments, &self.cwd_prefix),
            reason,
            answer: None,
        }));
    }

    /// Put a proposed plan in the scrollback, where it stays.
    pub fn request_plan(
        &mut self,
        id: PermissionId,
        text: String,
        path: Option<std::path::PathBuf>,
    ) {
        self.sealed = true;
        self.cells.push(Cell::Plan(PlanCell {
            id,
            text,
            path,
            answer: None,
        }));
    }

    /// Record the answer in place, so the scrollback shows what was decided.
    pub fn answer_permission(&mut self, id: &PermissionId, answer: PermissionAnswer) {
        for cell in self.cells.iter_mut().rev() {
            match cell {
                Cell::Permission(prompt) if &prompt.id == id => {
                    prompt.answer = Some(answer);
                    return;
                }
                Cell::Plan(plan) if &plan.id == id => {
                    plan.answer = Some(answer);
                    return;
                }
                _ => {}
            }
        }
    }

    /// The id of whatever is blocking the turn — a tool prompt or a plan.
    pub fn open_permission_id(&self) -> Option<PermissionId> {
        self.cells.iter().rev().find_map(|cell| match cell {
            Cell::Permission(prompt) if prompt.answer.is_none() => Some(prompt.id.clone()),
            Cell::Plan(plan) if plan.answer.is_none() => Some(plan.id.clone()),
            _ => None,
        })
    }

    /// The last plan this session saw, answered or not.
    pub fn last_plan(&self) -> Option<&PlanCell> {
        self.cells.iter().rev().find_map(|cell| match cell {
            Cell::Plan(plan) => Some(plan),
            _ => None,
        })
    }

    /// The prompt currently blocking the turn, if any.
    pub fn open_permission(&self) -> Option<&PermissionCell> {
        self.cells.iter().rev().find_map(|cell| match cell {
            Cell::Permission(prompt) if prompt.answer.is_none() => Some(prompt),
            _ => None,
        })
    }

    /// Rebuild the visible transcript from a resumed session's history.
    ///
    /// Reads the same messages the engine resumes with, so the screen and the
    /// next request agree about what was said. A tool call is drawn from the
    /// assistant message that made it and finished by the result that answered
    /// it, exactly as the live path does — a call whose result never made it
    /// into the log stays visibly unfinished rather than being drawn as a
    /// success nobody recorded.
    pub fn replay(&mut self, history: &[Message]) {
        for message in history {
            match message.role {
                // The system prompt is not something a person said, and showing
                // it would bury the conversation under it on every resume.
                Role::System => {}
                Role::User => {
                    let text = message.text();
                    if !text.trim().is_empty() {
                        self.push(Cell::User(text));
                    }
                }
                Role::Assistant => {
                    let text = message.text();
                    if !text.trim().is_empty() {
                        self.push(Cell::Assistant(text));
                    }
                    for block in &message.content {
                        if let ContentBlock::ToolCall(call) = block {
                            self.start_tool(call);
                        }
                    }
                }
                Role::Tool => {
                    for block in &message.content {
                        if let ContentBlock::ToolResult(result) = block {
                            self.finish_tool(result);
                        }
                    }
                }
            }
        }
        self.seal();
    }

    /// The newest thing on screen that can be opened, if there is one.
    ///
    /// The keyboard's answer to a click: after a run of calls scrolls past,
    /// what a person reaches for is that run — not one picked from a list.
    pub fn last_expandable(&self) -> Option<usize> {
        self.cells
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, cell)| match cell {
                Cell::Tool(tool) if !matches!(tool.state, CallState::Running) => {
                    let verb = verb(&tool.name).0;
                    // Only the first call of a run carries the header.
                    match index.checked_sub(1).map(|before| &self.cells[before]) {
                        Some(before) if groups_with(before, verb) => None,
                        _ => Some(index),
                    }
                }
                _ => None,
            })
    }

    /// Mark every still-running call cancelled.
    ///
    /// A cancelled turn may never deliver `ToolCallEnded`, and a spinner that
    /// never stops reads as a hang rather than as the abort it was.
    pub fn cancel_running_tools(&mut self) {
        for cell in &mut self.cells {
            if let Cell::Tool(tool) = cell
                && tool.state == CallState::Running
            {
                tool.state = CallState::Finished(ToolStatus::Cancelled);
            }
        }
    }
}

/// Whether a finished call belongs in the run being gathered.
pub(crate) fn groups_with(cell: &Cell, run: &str) -> bool {
    matches!(cell, Cell::Tool(tool)
        if !matches!(tool.state, CallState::Running) && verb(&tool.name).0 == run)
}

/// Past tense and the plural noun for a tool, so a run of calls reads as one
/// sentence: `Read 3 files`, `Ran 2 commands`.
///
/// Keyed on the tool's own name, which is what a tool declares about itself;
/// an unknown tool still groups, under its own name.
pub(crate) fn verb(name: &str) -> (&str, &str) {
    match name {
        "read_file" => ("Read", "files"),
        "write_file" => ("Wrote", "files"),
        "list_dir" => ("Listed", "directories"),
        "grep" => ("Searched", "patterns"),
        "bash" => ("Ran", "commands"),
        other => (other, "calls"),
    }
}

/// The fields worth showing alone, in the order a reader would want them.
///
/// A call is nearly always about one thing — the file, the command — and the
/// rest is machinery. Naming that field is what turns `read path=src/app.rs`
/// into `read src/app.rs`; everything else waits behind an expand.
const SALIENT: [&str; 6] = ["command", "path", "file_path", "pattern", "query", "url"];

/// One line for the collapsed view of a call.
///
/// Falls back to the full `key=value` form when no field stands out, so a tool
/// keke has never heard of still shows something rather than nothing. Nothing
/// here is keyed on a vendor: these are argument names, not tool identities.
pub(crate) fn headline(arguments: &serde_json::Value, cwd_prefix: &[String]) -> String {
    const PATH_KEYS: [&str; 2] = ["path", "file_path"];
    if let serde_json::Value::Object(fields) = arguments {
        for key in SALIENT {
            if let Some(serde_json::Value::String(text)) = fields.get(key)
                && !text.trim().is_empty()
            {
                let text = if PATH_KEYS.contains(&key) {
                    relative_to_cwd(text, cwd_prefix)
                } else {
                    text.clone()
                };
                return one_line(&text, 120);
            }
        }
    }
    summarize_arguments(arguments)
}

/// Re-root a workspace-relative path so it reads against `cwd_prefix` — the
/// directory the session was launched from — instead of the workspace root.
///
/// A path under the prefix loses it (`crates/keke-tools/src/lib.rs` becomes
/// `src/lib.rs` when launched from `crates/keke-tools`); one outside it grows
/// `..` segments, so a glance at the leading `../` is what tells a reader the
/// call reached outside where they are sitting — the one case where showing
/// the fuller path earns its keep.
fn relative_to_cwd(path: &str, cwd_prefix: &[String]) -> String {
    if cwd_prefix.is_empty() {
        return path.to_string();
    }
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let common = parts
        .iter()
        .zip(cwd_prefix.iter())
        .take_while(|(part, prefix)| **part == prefix.as_str())
        .count();
    let ups = cwd_prefix.len() - common;
    let mut rerooted: Vec<&str> = std::iter::repeat_n("..", ups).collect();
    rerooted.extend(&parts[common..]);
    if rerooted.is_empty() {
        ".".to_string()
    } else {
        rerooted.join("/")
    }
}

/// Collapse tool arguments to one line.
///
/// Objects are shown as `key=value` pairs because the fields a person needs to
/// judge a call — a path, a command — are almost always top level, and pretty
/// JSON would push the next cell off the screen.
pub(crate) fn summarize_arguments(arguments: &serde_json::Value) -> String {
    let rendered = match arguments {
        serde_json::Value::Null => String::new(),
        serde_json::Value::Object(fields) => fields
            .iter()
            .map(|(key, value)| format!("{key}={}", scalar(value)))
            .collect::<Vec<_>>()
            .join(" "),
        other => scalar(other),
    };
    one_line(&rendered, 160)
}

fn scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => one_line(text, 60),
        serde_json::Value::Array(items) => format!("[{} items]", items.len()),
        serde_json::Value::Object(fields) => format!("{{{} fields}}", fields.len()),
        other => other.to_string(),
    }
}

fn first_line(result: &ToolResult) -> Option<String> {
    let text = result
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })?
        .trim();
    (!text.is_empty()).then(|| one_line(text, 120))
}

/// The collapsed line shown once a call finishes.
///
/// Most tools read fine as their own first line of output. `grep` does not: a
/// hit's raw `file:line:code` text is the noisiest line in the transcript, and
/// as a "detail" it reads as if that one line were the whole story. A count is
/// what a reader actually wants before deciding whether to expand.
fn detail_line(name: &str, result: &ToolResult) -> Option<String> {
    if name == "grep" {
        return grep_summary(result);
    }
    first_line(result)
}

fn grep_summary(result: &ToolResult) -> Option<String> {
    let text = result
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })?
        .trim();
    if text.is_empty() || text.starts_with("no matches") {
        return (!text.is_empty()).then(|| one_line(text, 120));
    }
    let truncated = text
        .lines()
        .next_back()
        .is_some_and(|line| line.starts_with('…'));
    let count = text
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('…'))
        .count();
    let noun = if count == 1 { "match" } else { "matches" };
    Some(if truncated {
        format!("{count}+ {noun}")
    } else {
        format!("{count} {noun}")
    })
}

/// Flatten to a single line and ellipsize, counting characters rather than
/// bytes so a multi-byte path is not cut mid-codepoint.
fn one_line(text: &str, limit: usize) -> String {
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() <= limit {
        return flattened;
    }
    let kept: String = flattened.chars().take(limit.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod cwd_display_tests {
    use super::*;

    fn prefix(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    #[test]
    fn strips_the_prefix_for_a_path_under_cwd() {
        let cwd_prefix = prefix(&["crates", "keke-tools"]);
        assert_eq!(
            relative_to_cwd("crates/keke-tools/src/write_file.rs", &cwd_prefix),
            "src/write_file.rs"
        );
    }

    #[test]
    fn climbs_out_for_a_path_outside_cwd() {
        let cwd_prefix = prefix(&["crates", "keke-tools"]);
        assert_eq!(
            relative_to_cwd("crates/keke-core/src/lib.rs", &cwd_prefix),
            "../keke-core/src/lib.rs"
        );
    }

    #[test]
    fn leaves_paths_alone_when_launched_from_the_workspace_root() {
        assert_eq!(
            relative_to_cwd("crates/keke-core/src/lib.rs", &[]),
            "crates/keke-core/src/lib.rs"
        );
    }

    #[test]
    fn headline_reroots_the_path_argument() {
        let cwd_prefix = prefix(&["crates", "keke-tools"]);
        let arguments = serde_json::json!({"path": "crates/keke-tools/src/write_file.rs"});
        assert_eq!(headline(&arguments, &cwd_prefix), "src/write_file.rs");
    }
}
