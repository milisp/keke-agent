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
use keke_protocol::SessionEvent;
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
    /// The result's text, shown in the expanded view.
    pub detail: Option<String>,
}

/// A tool call waiting on approval.
///
/// Not a scrollback cell: a tool prompt is a question about what happens
/// next, not something said, so it lives beside the transcript in its own
/// slot — [`Transcript::open_permission`] — and is drawn in the panel under
/// it, the way the MCP picker is drawn beside the composer rather than
/// mixed into the cells it manages.
#[derive(Clone, Debug, PartialEq)]
pub struct PermissionCell {
    pub id: PermissionId,
    pub name: String,
    pub summary: String,
    pub reason: String,
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
    /// The tool call currently blocking the turn, if any. Cleared the moment
    /// it is answered — see [`PermissionCell`] for why this is not a cell.
    open_permission: Option<PermissionCell>,
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

    /// Every prompt a person sent, as `(cell index, text)` in the order they
    /// were sent. What the rewind overlay offers to go back to.
    pub fn user_prompts(&self) -> Vec<(usize, String)> {
        self.cells
            .iter()
            .enumerate()
            .filter_map(|(at, cell)| match cell {
                Cell::User(text) => Some((at, text.clone())),
                _ => None,
            })
            .collect()
    }

    /// Drop everything from cell `at` onwards, for a rewind.
    ///
    /// Unlike [`Self::clear`] this is not a person tidying the view: the agent
    /// is being told to forget the same messages, so what is left on screen is
    /// again exactly what the next request will carry.
    pub fn truncate(&mut self, at: usize) {
        self.cells.truncate(at);
        self.sealed = true;
    }

    /// Redraw the startup banner with lines that became known after it was
    /// first shown. Not a push: the banner stays the first cell either way.
    pub fn replace_banner(&mut self, lines: Vec<String>) {
        if let Some(Cell::Banner(banner)) = self
            .cells
            .iter_mut()
            .find(|cell| matches!(cell, Cell::Banner(_)))
        {
            *banner = lines;
        }
    }

    pub fn push(&mut self, cell: Cell) {
        self.cells.push(cell);
        self.sealed = true;
    }

    /// Append visible text, growing the assistant message already in progress.
    ///
    /// One turn of prose is one cell: a delta per cell would make wrapping and
    /// copy-out wrong for anyone whose provider chunks by token.
    ///
    /// A whitespace-only delta after a seal never opens a new cell. Some
    /// providers emit one between tool calls as a heartbeat; `replay` already
    /// drops such text (it only pushes a message with non-blank `text()`), so
    /// letting it through live would plant an invisible `Cell::Assistant`
    /// between two tool calls that only the live path ever sees — breaking
    /// the run they'd otherwise group into, live but not on resume.
    pub fn push_text_delta(&mut self, delta: &str) {
        match self.cells.last_mut() {
            Some(Cell::Assistant(text)) if !self.sealed => text.push_str(delta),
            _ if delta.trim().is_empty() => {}
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
            name: display_tool_name(&call.name).to_string(),
            summary: headline(&call.arguments, &self.cwd_prefix),
            // Shell controls belong in the event log, not beside stdout.
            arguments: if display_tool_name(&call.name) == "bash" {
                call.arguments
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            } else {
                expanded_arguments(&call.arguments, None)
            },
            state: CallState::Running,
            detail: None,
        }));
    }

    /// Put a tool the vendor ran for itself in the scrollback.
    ///
    /// Pushed already finished: it is reported after the fact, so there is no
    /// running phase to draw and nothing later will revise it. The id is
    /// synthetic — no engine call owns it — and deliberately not `Running`, so
    /// [`Self::finish_tool`] can never mistake it for an open call.
    pub fn hosted_tool(
        &mut self,
        name: &str,
        query: Option<&str>,
        metadata: Option<&keke_protocol::HostedToolMetadata>,
    ) {
        self.sealed = true;
        let mut details = Vec::new();
        if let Some(q) = query.filter(|q| !q.trim().is_empty()) {
            details.push(format!("query={q}"));
        }
        if let Some(meta) = metadata {
            details.extend(
                meta.queries
                    .iter()
                    .filter(|q| !q.trim().is_empty())
                    .map(|q| format!("query={q}")),
            );
            if let Some(url) = &meta.url {
                details.push(format!("url={url}"));
            }
            if let Some(pattern) = &meta.pattern {
                details.push(format!("pattern={pattern}"));
            }
        }
        let action = metadata.and_then(|m| m.action.as_deref());
        let title = match action {
            Some("open_page") => "Open",
            Some("find_in_page") => "Find",
            Some("search") => "Search",
            _ if name == "web_search" => "Search",
            _ => name,
        };
        let summary = match action {
            Some("open_page") => metadata.and_then(|m| m.url.clone()).unwrap_or_default(),
            Some("find_in_page") => metadata.and_then(|m| m.pattern.clone()).unwrap_or_default(),
            _ => query
                .map(str::to_string)
                .or_else(|| metadata.map(|m| m.queries.join(", ")))
                .unwrap_or_default(),
        };
        self.cells.push(Cell::Tool(ToolCell {
            id: ToolCallId::new(format!("hosted:{name}")),
            name: title.to_string(),
            summary,
            arguments: details.join("\n"),
            state: CallState::Finished(ToolStatus::Ok),
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
        self.open_permission = Some(PermissionCell {
            id,
            name: display_tool_name(&call.name).to_string(),
            summary: headline(&call.arguments, &self.cwd_prefix),
            reason,
        });
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

    /// Answer the prompt, or the plan's cell if it was that instead.
    pub fn answer_permission(&mut self, id: &PermissionId, answer: PermissionAnswer) {
        if self.open_permission.as_ref().is_some_and(|p| &p.id == id) {
            self.open_permission = None;
            return;
        }
        for cell in self.cells.iter_mut().rev() {
            if let Cell::Plan(plan) = cell
                && &plan.id == id
            {
                plan.answer = Some(answer);
                return;
            }
        }
    }

    /// The id of whatever is blocking the turn — a tool prompt or a plan.
    pub fn open_permission_id(&self) -> Option<PermissionId> {
        if let Some(prompt) = &self.open_permission {
            return Some(prompt.id.clone());
        }
        self.cells.iter().rev().find_map(|cell| match cell {
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

    /// The tool call currently blocking the turn, if any.
    pub fn open_permission(&self) -> Option<&PermissionCell> {
        self.open_permission.as_ref()
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

    /// Replay recorded conversation through the same cells used by a live session.
    ///
    /// Request snapshots and compaction summaries are model context, rather
    /// than new conversation: replaying them would duplicate messages or hide
    /// the earlier conversation a person is opening this record to inspect.
    pub fn replay_recorded(&mut self, events: &[SessionEvent]) {
        for event in events {
            match event {
                SessionEvent::TurnStart { input, .. }
                | SessionEvent::ModelResponse { message: input, .. }
                | SessionEvent::ContextMessage { message: input, .. } => {
                    self.replay_recorded_message(input);
                }
                SessionEvent::ToolCallStart { call, .. } => self.start_recorded_tool(call),
                SessionEvent::ToolCallEnd { result, .. } => {
                    self.finish_tool(result);
                }
                SessionEvent::HostedToolCall {
                    name,
                    query,
                    metadata,
                    ..
                } => {
                    self.hosted_tool(name, query.as_deref(), metadata.as_ref());
                }
                SessionEvent::Error { message, .. } => self.push(Cell::Error(message.clone())),
                SessionEvent::Rewound {
                    history: Some(_), ..
                } => {
                    self.push(Cell::Notice("Conversation rewound".to_string()));
                }
                _ => {}
            }
        }
        self.seal();
    }

    fn start_recorded_tool(&mut self, call: &ToolCall) {
        // ModelResponse and ToolCallStart describe the same call. Only the
        // latter can survive an interrupted response, so accept either source.
        if !self
            .cells
            .iter()
            .any(|cell| matches!(cell, Cell::Tool(tool) if tool.id == call.id))
        {
            self.start_tool(call);
        }
    }

    fn replay_recorded_message(&mut self, message: &Message) {
        if message.role == Role::System {
            return;
        }
        for block in &message.content {
            match block {
                ContentBlock::Text { text } if !text.trim().is_empty() => {
                    self.push(match message.role {
                        Role::User => Cell::User(text.clone()),
                        Role::Assistant => Cell::Assistant(text.clone()),
                        _ => Cell::Notice(text.clone()),
                    });
                }
                ContentBlock::Thinking { text, .. } if !text.trim().is_empty() => {
                    self.push(Cell::Notice(format!("Thinking…\n{text}")));
                }
                ContentBlock::ToolCall(call) => self.start_recorded_tool(call),
                ContentBlock::ToolResult(result) => {
                    self.finish_tool(result);
                }
                _ => {}
            }
        }
    }

    /// The newest thing on screen that can be opened, if there is one.
    ///
    /// The keyboard's answer to a click: the newest finished call or run.
    pub fn last_expandable(&self) -> Option<usize> {
        self.cells
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, cell)| match cell {
                Cell::Tool(tool) if !matches!(tool.state, CallState::Running) => {
                    if tool.name == "bash" {
                        return has_compact_detail(tool).then_some(index);
                    }
                    // Only the first call of a run carries the header.
                    match index.checked_sub(1).map(|before| &self.cells[before]) {
                        Some(before) if groups_with(before, &tool.name) => None,
                        _ if has_compact_detail(tool)
                            || self
                                .cells
                                .get(index + 1)
                                .is_some_and(|next| groups_with(next, &tool.name)) =>
                        {
                            Some(index)
                        }
                        _ => None,
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

/// Keep the sandbox escalation detail out of the command's TUI label.
fn display_tool_name(name: &str) -> &str {
    if name == "bash_unsandboxed" {
        "bash"
    } else {
        name
    }
}

/// Whether a finished call belongs in the run being gathered.
pub(crate) fn groups_with(cell: &Cell, anchor: &str) -> bool {
    matches!(cell, Cell::Tool(tool)
        if !matches!(tool.state, CallState::Running) && same_run(&tool.name, anchor))
}

/// Whether a single call has a body in the compact transcript.
pub(crate) fn has_compact_detail(tool: &ToolCell) -> bool {
    if is_exploration_tool(&tool.name) {
        return false;
    }
    tool.detail
        .as_ref()
        .is_some_and(|detail| !detail.is_empty())
        || (tool.name != "bash" && !is_diff_tool(&tool.name) && !tool.arguments.is_empty())
}

/// A read-only exploration tool: safe to fold into one "Exploring" run
/// alongside other exploration tools, since none of them changes anything a
/// person would need to review individually.
pub(crate) fn is_exploration_tool(name: &str) -> bool {
    matches!(name, "read_file" | "list_dir" | "grep")
}

/// Whether two tool calls belong in the same run for display purposes.
///
/// Exploration tools (`read_file`, `list_dir`, `grep`) group with each other
/// regardless of order — a person skimming `read, list, read` wants one
/// "Exploring" run, not three headers for a search that never wrote anything.
/// A diff tool (`edit`, `write_file`, `apply_patch`) never groups, not even with itself: its
/// diff is the one thing a person needs to see to trust the change, and two
/// diffs sharing one header with no label between them are indistinguishable.
/// Commands never group: each command and its output must be independently
/// visible and toggleable. Other tools group with themselves.
pub(crate) fn same_run(a: &str, b: &str) -> bool {
    if is_exploration_tool(a) && is_exploration_tool(b) {
        true
    } else if is_diff_tool(a) || is_diff_tool(b) || a == "bash" || b == "bash" {
        false
    } else {
        a == b
    }
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
        "apply_patch" => ("Edited", "files"),
        "list_dir" => ("Listed", "directories"),
        "grep" => ("Searched", "patterns"),
        "bash" => ("Ran", "commands"),
        other => (other, "calls"),
    }
}

/// Whether a tool's detail is a diff worth showing without a click.
///
/// A clean `read_file` or `grep` folding away is the point — nothing to
/// review. A clean `edit`/`write_file` is exactly the opposite: the diff is
/// the one thing a person needs to see to trust what the agent just did, so
/// it stays open even on success.
pub(crate) fn is_diff_tool(name: &str) -> bool {
    matches!(name, "edit" | "write_file" | "apply_patch")
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
        if let Some(patch) = fields.get("patch").and_then(serde_json::Value::as_str) {
            let files: Vec<_> = patch
                .lines()
                .filter_map(|line| {
                    ["*** Add File: ", "*** Update File: ", "*** Delete File: "]
                        .iter()
                        .find_map(|prefix| line.strip_prefix(prefix))
                })
                .collect();
            match files.as_slice() {
                [path] => return one_line(&relative_to_cwd(path, cwd_prefix), 120),
                [] => {}
                _ => return format!("{} files", files.len()),
            }
        }
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

/// Collapse tool arguments to one line, for [`headline`]'s fallback when no
/// field stood out as salient.
///
/// Objects are shown as `key=value` pairs because the fields a person needs to
/// judge a call — a path, a command — are almost always top level, and pretty
/// JSON would push the next cell off the screen.
pub(crate) fn summarize_arguments(arguments: &serde_json::Value) -> String {
    one_line(&render_arguments(arguments, None, one_line_scalar), 160)
}

/// Tool arguments for expansion. Callers may omit a field already shown in
/// full elsewhere; transcript cells keep every field because a collapsed
/// headline can truncate long commands or paths.
///
/// Preserve nested values and line breaks so expanding a call reveals the
/// recorded arguments rather than another summary.
pub(crate) fn expanded_arguments(arguments: &serde_json::Value, skip: Option<&str>) -> String {
    render_arguments(arguments, skip, |value| match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    })
}

fn render_arguments(
    arguments: &serde_json::Value,
    skip: Option<&str>,
    scalar: impl Fn(&serde_json::Value) -> String,
) -> String {
    match arguments {
        serde_json::Value::Null => String::new(),
        serde_json::Value::Object(fields) => fields
            .iter()
            .filter(|(key, _)| Some(key.as_str()) != skip)
            .map(|(key, value)| format!("{key}={}", scalar(value)))
            .collect::<Vec<_>>()
            .join(" "),
        other => scalar(other),
    }
}

fn one_line_scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => one_line(text, 60),
        other => trimmed_scalar(other),
    }
}

fn trimmed_scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.trim().to_string(),
        serde_json::Value::Array(items) => format!("[{} items]", items.len()),
        serde_json::Value::Object(fields) => format!("{{{} fields}}", fields.len()),
        other => other.to_string(),
    }
}

/// The result's text, for the expanded view. Not `one_line`d for the same
/// reason as [`scalar`]: `push_block` reflows it like a paragraph, so a
/// multi-line `stdout` reads as itself rather than one truncated row.
fn full_text(result: &ToolResult) -> Option<String> {
    let text = result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then(|| text.trim().to_string())
}

/// Preserve tool output for expansion, preferring reviewable diffs for edits.
fn detail_line(name: &str, result: &ToolResult) -> Option<String> {
    match name {
        "edit" | "write_file" => diff_hunk(result).or_else(|| full_text(result)),
        "apply_patch" => patch_diff(result).or_else(|| full_text(result)),
        _ => full_text(result),
    }
}

/// The per-file diffs carried by `apply_patch`, with a path heading so a
/// multi-file change remains legible in the transcript.
fn patch_diff(result: &ToolResult) -> Option<String> {
    let changes = result.value.as_ref()?.get("changes")?.as_array()?;
    let mut sections = Vec::new();
    for change in changes {
        let path = change.get("path")?.as_str()?;
        let diff = change.get("diff")?;
        let hunk = diff.get("hunk")?.as_str()?.trim_end_matches('\n');
        let added = diff
            .get("added")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let removed = diff
            .get("removed")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let destination = change.get("moved_to").and_then(serde_json::Value::as_str);
        let heading = destination.map_or_else(|| path.to_string(), |to| format!("{path} → {to}"));
        let verb = match change.get("kind").and_then(serde_json::Value::as_str) {
            Some("added") => "Added",
            Some("deleted") => "Deleted",
            _ => "Updated",
        };
        if hunk.is_empty() {
            sections.push(format!("{verb} {heading} (+{added} -{removed})"));
        } else {
            sections.push(format!("{verb} {heading} (+{added} -{removed})\n{hunk}"));
        }
    }
    (!sections.is_empty()).then(|| sections.join("\n\n"))
}

/// The unified diff a write carried in [`ToolResult::value`], if any.
///
/// `content` only ever holds the terse summary the model sees (`edited
/// path (+1 -2, ...)`); the lines that actually changed live in `value`
/// instead, since that field is never charged against the model's context.
fn diff_hunk(result: &ToolResult) -> Option<String> {
    let hunk = result
        .value
        .as_ref()?
        .get("diff")?
        .get("hunk")?
        .as_str()?
        .trim_end_matches('\n');
    (!hunk.is_empty()).then(|| hunk.to_string())
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
mod push_text_delta_tests {
    use super::*;

    #[test]
    fn a_blank_delta_between_sealed_tools_does_not_split_the_run() {
        let mut transcript = Transcript::default();
        transcript.start_tool(&ToolCall {
            id: ToolCallId::new("c1"),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "a.rs"}),
        });
        transcript.finish_tool(&ToolResult::ok(ToolCallId::new("c1"), "ok"));
        transcript.seal();
        transcript.push_text_delta("");
        transcript.push_text_delta("   ");
        transcript.start_tool(&ToolCall {
            id: ToolCallId::new("c2"),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "b.rs"}),
        });

        let tool_count = transcript
            .cells()
            .iter()
            .filter(|cell| matches!(cell, Cell::Tool(_)))
            .count();
        assert_eq!(
            transcript.cells().len(),
            tool_count,
            "no stray cell in between: {:?}",
            transcript.cells()
        );
    }

    #[test]
    fn real_text_between_tool_calls_still_opens_its_own_cell() {
        let mut transcript = Transcript::default();
        transcript.push_text_delta("hello");
        assert_eq!(transcript.cells().len(), 1);
    }
}

#[cfg(test)]
mod detail_line_tests {
    use super::*;

    fn result_with_value(value: serde_json::Value) -> ToolResult {
        ToolResult {
            id: ToolCallId::new("c1"),
            status: ToolStatus::Ok,
            content: vec![ContentBlock::text("edited file.rs (+1 -1, 1 replacement)")],
            value: Some(value),
        }
    }

    #[test]
    fn read_file_keeps_output_for_expansion() {
        let result = ToolResult::ok(ToolCallId::new("c1"), "1\thello\n2\tworld\n");
        assert_eq!(
            detail_line("read_file", &result).as_deref(),
            Some("1\thello\n2\tworld")
        );
    }

    #[test]
    fn list_dir_keeps_output_for_expansion() {
        let result = ToolResult::ok(ToolCallId::new("c1"), "src/\nCargo.toml\n");
        assert_eq!(
            detail_line("list_dir", &result).as_deref(),
            Some("src/\nCargo.toml")
        );
    }

    #[test]
    fn grep_keeps_output_for_expansion() {
        let result = ToolResult::ok(ToolCallId::new("c1"), "src/lib.rs:1:foo\n");
        assert_eq!(
            detail_line("grep", &result).as_deref(),
            Some("src/lib.rs:1:foo")
        );
    }

    #[test]
    fn edit_detail_is_the_diff_hunk_not_the_model_summary() {
        let result = result_with_value(serde_json::json!({
            "path": "file.rs",
            "replacements": 1,
            "diff": { "added": 1, "removed": 1, "hunk": "-old\n+new\n" },
        }));
        assert_eq!(detail_line("edit", &result).as_deref(), Some("-old\n+new"));
    }

    #[test]
    fn write_file_without_a_diff_falls_back_to_the_model_summary() {
        let result = result_with_value(serde_json::json!({
            "path": "file.rs",
            "bytes": 12,
            "created": true,
            "diff": null,
        }));
        assert_eq!(
            detail_line("write_file", &result).as_deref(),
            Some("edited file.rs (+1 -1, 1 replacement)")
        );
    }

    #[test]
    fn diff_details_preserve_source_trailing_whitespace() {
        let hunk = "   +1  hello  \t\n";
        let result = result_with_value(serde_json::json!({
            "diff": {"added": 1, "removed": 0, "hunk": hunk},
        }));
        for tool in ["edit", "write_file"] {
            assert_eq!(
                detail_line(tool, &result).as_deref(),
                Some("   +1  hello  \t")
            );
        }
        let result = result_with_value(serde_json::json!({
            "changes": [{"path": "a.rs", "diff": {"added": 1, "removed": 0, "hunk": hunk}}],
        }));
        assert_eq!(
            detail_line("apply_patch", &result).as_deref(),
            Some("Updated a.rs (+1 -0)\n   +1  hello  \t")
        );
    }

    #[test]
    fn patch_headlines_name_files_instead_of_flattening_source() {
        let arguments = serde_json::json!({
            "patch": "*** Begin Patch\n*** Add File: src/new.rs\n+secret source\n*** End Patch"
        });
        assert_eq!(headline(&arguments, &["src".to_string()]), "new.rs");
        let arguments = serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: a.rs\n@@\n-old\n+new\n*** Delete File: b.rs\n*** End Patch"
        });
        assert_eq!(headline(&arguments, &[]), "2 files");
    }

    #[test]
    fn mixed_patches_keep_moves_and_empty_file_changes() {
        let result = result_with_value(serde_json::json!({
            "changes": [
                {"path": "old.rs", "moved_to": "new.rs", "kind": "updated", "diff": {"added": 0, "removed": 0, "hunk": ""}},
                {"path": "empty.rs", "kind": "added", "diff": {"added": 0, "removed": 0, "hunk": ""}},
                {"path": "deleted.rs", "kind": "deleted", "diff": {"added": 0, "removed": 0, "hunk": ""}},
                {"path": "changed.rs", "kind": "updated", "diff": {"added": 1, "removed": 0, "hunk": "   +1  code\n"}},
            ],
        }));
        let detail = detail_line("apply_patch", &result).unwrap();
        for heading in [
            "Updated old.rs → new.rs (+0 -0)",
            "Added empty.rs (+0 -0)",
            "Deleted deleted.rs (+0 -0)",
            "Updated changed.rs (+1 -0)",
        ] {
            assert!(detail.contains(heading), "{detail}");
        }
    }

    #[test]
    fn a_run_commands_multi_line_output_stays_multi_line() {
        let result = ToolResult::ok(ToolCallId::new("c1"), "line one\nline two\nline three\n");
        assert_eq!(
            detail_line("run_command", &result).as_deref(),
            Some("line one\nline two\nline three")
        );
    }
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

#[cfg(test)]
mod recorded_replay_tests {
    use super::*;
    use keke_protocol::{StopReason, TurnId, Usage};

    #[test]
    fn recorded_replay_keeps_conversation_before_compaction_without_snapshot_duplicates() {
        let turn = TurnId::new();
        let call = ToolCall {
            id: ToolCallId::new("read"),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "src/main.rs", "options": {"lines": [1, 2]}}),
        };
        let reply = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::thinking("Inspect the file."),
                ContentBlock::text("Reading."),
                ContentBlock::ToolCall(call.clone()),
            ],
        };
        let result = ToolResult {
            id: call.id.clone(),
            status: ToolStatus::Ok,
            content: vec![
                ContentBlock::text("first line"),
                ContentBlock::text("second line"),
            ],
            value: None,
        };
        let events = vec![
            SessionEvent::TurnStart {
                turn,
                input: Message::user("Find the entry point."),
                approval_policy: None,
            },
            SessionEvent::ModelRequest {
                turn,
                messages: vec![Message::user("Find the entry point.")],
                tools: vec![],
                reasoning_effort: None,
                model: None,
            },
            SessionEvent::ModelResponse {
                turn,
                message: reply,
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            },
            SessionEvent::ToolCallStart { turn, call },
            SessionEvent::ToolCallEnd { turn, result },
            SessionEvent::Compacted {
                turn,
                summary: Message::user("Summary"),
                removed_messages: 3,
            },
        ];
        let mut transcript = Transcript::default();
        transcript.replay_recorded(&events);
        assert_eq!(transcript.cells().len(), 4);
        assert_eq!(
            transcript.cells()[0],
            Cell::User("Find the entry point.".into())
        );
        assert_eq!(
            transcript.cells()[1],
            Cell::Notice("Thinking…\nInspect the file.".into())
        );
        assert_eq!(transcript.cells()[2], Cell::Assistant("Reading.".into()));
        let Cell::Tool(tool) = &transcript.cells()[3] else {
            panic!("expected existing tool cell")
        };
        assert_eq!(tool.state, CallState::Finished(ToolStatus::Ok));
        assert_eq!(tool.detail.as_deref(), Some("first line\nsecond line"));
        assert!(tool.arguments.contains("path=src/main.rs"));
        assert!(tool.arguments.contains("options={\"lines\":[1,2]}"));
    }

    #[test]
    fn recorded_replay_keeps_messages_removed_by_a_rewind() {
        let mut transcript = Transcript::default();
        transcript.replay_recorded(&[
            SessionEvent::TurnStart {
                turn: TurnId::new(),
                input: Message::user("Original prompt"),
                approval_policy: None,
            },
            SessionEvent::Rewound {
                scope: keke_protocol::RewindScope::Conversation,
                history: Some(vec![]),
                prompt: "Original prompt".into(),
                removed_messages: 1,
                restored_files: vec![],
                undo: None,
            },
        ]);
        assert_eq!(
            transcript.cells(),
            &[
                Cell::User("Original prompt".into()),
                Cell::Notice("Conversation rewound".into())
            ]
        );
    }

    #[test]
    fn shell_display_keeps_the_command_without_execution_controls() {
        let mut transcript = Transcript::default();
        transcript.start_tool(&ToolCall {
            id: ToolCallId::new("shell"),
            name: "bash_unsandboxed".into(),
            arguments: serde_json::json!({
                "command": "echo first\necho second",
                "background": false,
                "timeout_ms": 1000,
                "justification": "needs network"
            }),
        });
        let Cell::Tool(tool) = &transcript.cells()[0] else {
            panic!("expected shell cell")
        };
        assert_eq!(tool.name, "bash");
        assert_eq!(tool.arguments, "echo first\necho second");
    }

    #[test]
    fn recorded_replay_keeps_an_interrupted_tool_visibly_unfinished() {
        let mut transcript = Transcript::default();
        transcript.replay_recorded(&[SessionEvent::ToolCallStart {
            turn: TurnId::new(),
            call: ToolCall {
                id: ToolCallId::new("pending"),
                name: "bash".into(),
                arguments: serde_json::json!({"command": "echo first\necho second"}),
            },
        }]);
        let Cell::Tool(tool) = &transcript.cells()[0] else {
            panic!("expected existing tool cell")
        };
        assert_eq!(tool.state, CallState::Running);
        assert_eq!(tool.arguments, "echo first\necho second");
    }
}
