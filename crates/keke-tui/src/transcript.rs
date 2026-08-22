//! What the scrollback holds.
//!
//! The transcript is an ordered list of cells rather than a string, because a
//! tool call has to be *revised* when its result arrives. A surface that
//! appended a line per event would leave the reader scrolling back to find out
//! whether the edit it approved actually happened.

use keke_acp::PermissionAnswer;
use keke_acp::PermissionId;
use keke_protocol::ContentBlock;
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
    /// One line describing the arguments, for the collapsed view.
    pub summary: String,
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

#[derive(Clone, Debug, PartialEq)]
pub enum Cell {
    User(String),
    Assistant(String),
    /// Reasoning. Held separately from `Assistant` so hiding it is a filter
    /// rather than a re-parse of prose that may legitimately contain anything.
    Thinking(String),
    Tool(ToolCell),
    Permission(PermissionCell),
    /// A `Update::Failed`, or a local error. Never terminal.
    Error(String),
    /// Out-of-band host chatter — a login URL, a device code.
    Notice(String),
}

#[derive(Debug, Default)]
pub struct Transcript {
    cells: Vec<Cell>,
    /// Set by [`Transcript::seal`]; makes the next delta open a new cell
    /// instead of extending the one already on screen.
    sealed: bool,
}

impl Transcript {
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

    pub fn push_thinking_delta(&mut self, delta: &str) {
        match self.cells.last_mut() {
            Some(Cell::Thinking(text)) if !self.sealed => text.push_str(delta),
            _ => {
                self.cells.push(Cell::Thinking(delta.to_string()));
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
            summary: summarize_arguments(&call.arguments),
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
        cell.state = CallState::Finished(result.status);
        cell.detail = first_line(result);
        true
    }

    pub fn request_permission(&mut self, id: PermissionId, call: &ToolCall, reason: String) {
        self.sealed = true;
        self.cells.push(Cell::Permission(PermissionCell {
            id,
            name: call.name.clone(),
            summary: summarize_arguments(&call.arguments),
            reason,
            answer: None,
        }));
    }

    /// Record the answer in place, so the scrollback shows what was decided.
    pub fn answer_permission(&mut self, id: &PermissionId, answer: PermissionAnswer) {
        if let Some(cell) = self.cells.iter_mut().rev().find_map(|cell| match cell {
            Cell::Permission(prompt) if &prompt.id == id => Some(prompt),
            _ => None,
        }) {
            cell.answer = Some(answer);
        }
    }

    /// The prompt currently blocking the turn, if any.
    pub fn open_permission(&self) -> Option<&PermissionCell> {
        self.cells.iter().rev().find_map(|cell| match cell {
            Cell::Permission(prompt) if prompt.answer.is_none() => Some(prompt),
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
