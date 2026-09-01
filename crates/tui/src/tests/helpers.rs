//! Shared test helpers used across the `tests` submodules.

use std::sync::Arc;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use crossterm::event::MouseEvent;
use keke_acp::PermissionId;
use keke_acp::ScriptedConversation;
use keke_acp::Update;
use keke_protocol::ToolCall;
use keke_protocol::ToolCallId;
use keke_protocol::ToolResult;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::App;

pub(super) fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::new(id),
        name: name.to_string(),
        arguments: serde_json::json!({ "path": "src/lib.rs" }),
    }
}

/// An app wired to a scripted agent, plus both update streams.
pub(super) fn app_with(
    script: Vec<Vec<Update>>,
) -> (
    App,
    Arc<ScriptedConversation>,
    UnboundedReceiver<Update>,
    UnboundedReceiver<Update>,
) {
    let (scripted, updates) = ScriptedConversation::new(script);
    let scripted = Arc::new(scripted);
    let (app, local) = App::new(Arc::clone(&scripted) as Arc<_>);
    (app, scripted, updates, local)
}

pub(super) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub(super) fn mouse(kind: crossterm::event::MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

pub(super) fn click(column: u16, row: u16) -> MouseEvent {
    mouse(
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column,
        row,
    )
}

pub(super) fn wheel(up: bool) -> MouseEvent {
    let kind = if up {
        crossterm::event::MouseEventKind::ScrollUp
    } else {
        crossterm::event::MouseEventKind::ScrollDown
    };
    mouse(kind, 0, 0)
}

pub(super) fn control(ch: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
}

/// Copying is a command now, not a key: Ctrl-Y is gone so that the terminal
/// keeps its own selection.
pub(super) fn copy_command(app: &mut App) {
    type_text(app, "/copy");
    app.handle_key(key(KeyCode::Enter));
}

pub(super) fn type_text(app: &mut App, text: &str) {
    for ch in text.chars() {
        app.handle_key(key(KeyCode::Char(ch)));
    }
}

/// Drain what the scripted agent produced for one prompt.
pub(super) async fn drain(app: &mut App, updates: &mut UnboundedReceiver<Update>, count: usize) {
    for _ in 0..count {
        let update = updates.recv().await.expect("scripted update");
        app.apply(update);
    }
}

/// The same helper, with a command list a person can complete against.
pub(super) fn app_with_commands(
    script: Vec<Vec<Update>>,
    plugins: Vec<crate::PluginCommand>,
) -> (
    App,
    Arc<ScriptedConversation>,
    UnboundedReceiver<Update>,
    UnboundedReceiver<Update>,
) {
    let (app, scripted, updates, local) = app_with(script);
    (
        app.with_commands(crate::SlashCommands::new(plugins)),
        scripted,
        updates,
        local,
    )
}

pub(super) fn shift(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::SHIFT)
}

pub(super) fn exit_plan_mode(plan: &str) -> Update {
    Update::PermissionRequested {
        id: PermissionId("plan-1".to_string()),
        call: ToolCall {
            id: ToolCallId::new("t1"),
            name: "exit_plan_mode".to_string(),
            arguments: serde_json::json!({ "plan": plan }),
        },
        reason: "the plan is ready".to_string(),
    }
}

/// What the status bar reads, as one string.
pub(super) fn status_bar(app: &App) -> String {
    crate::draw::status::spans(app)
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// Flatten a rendered transcript to plain strings.
pub(super) fn rendered(app: &App) -> Vec<String> {
    crate::draw::transcript::render(app.transcript.cells(), 80, app.expanded())
        .lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect()
}

/// Run `count` reads through, each on its own path, all successful.
pub(super) fn finished_reads(app: &mut App, count: usize) {
    for index in 0..count {
        let id = format!("c{index}");
        app.apply(Update::ToolCallStarted(ToolCall {
            id: ToolCallId::new(&id),
            name: "read_file".to_string(),
            arguments: serde_json::json!({ "path": format!("src/f{index}.rs") }),
        }));
        app.apply(Update::ToolCallEnded(ToolResult::ok(
            ToolCallId::new(&id),
            "12 lines",
        )));
    }
}

/// A run of a tool with side effects, unlike [`finished_reads`] — exploration
/// runs stay open by default (see `default_open`), so the default-collapse
/// tests need a tool that still folds away on a clean finish.
pub(super) fn finished_commands(app: &mut App, count: usize) {
    for index in 0..count {
        let id = format!("c{index}");
        app.apply(Update::ToolCallStarted(ToolCall {
            id: ToolCallId::new(&id),
            name: "bash".to_string(),
            arguments: serde_json::json!({ "command": format!("echo {index}") }),
        }));
        app.apply(Update::ToolCallEnded(ToolResult::ok(
            ToolCallId::new(&id),
            "12 lines",
        )));
    }
}
