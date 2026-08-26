//! State tests. Every one drives the app the way a key or an update would and
//! asserts on what a person would see — never on how a cell is stored.

use std::sync::Arc;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use crossterm::event::MouseEvent;
use keke_acp::PermissionAnswer;
use keke_acp::PermissionId;
use keke_acp::ScriptedConversation;
use keke_acp::Update;
use keke_config_types::ApprovalPolicy;
use keke_protocol::ContentBlock;
use keke_protocol::ReasoningEffort;
use keke_protocol::StopReason;
use keke_protocol::ToolCall;
use keke_protocol::ToolCallId;
use keke_protocol::ToolResult;
use keke_protocol::ToolStatus;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::App;
use crate::CallState;
use crate::Cell;
use crate::Turn;

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::new(id),
        name: name.to_string(),
        arguments: serde_json::json!({ "path": "src/lib.rs" }),
    }
}

/// An app wired to a scripted agent, plus both update streams.
fn app_with(
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

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn mouse(kind: crossterm::event::MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn click(column: u16, row: u16) -> MouseEvent {
    mouse(
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column,
        row,
    )
}

fn wheel(up: bool) -> MouseEvent {
    let kind = if up {
        crossterm::event::MouseEventKind::ScrollUp
    } else {
        crossterm::event::MouseEventKind::ScrollDown
    };
    mouse(kind, 0, 0)
}

fn control(ch: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
}

/// Copying is a command now, not a key: Ctrl-Y is gone so that the terminal
/// keeps its own selection.
fn copy_command(app: &mut App) {
    type_text(app, "/copy");
    app.handle_key(key(KeyCode::Enter));
}

fn type_text(app: &mut App, text: &str) {
    for ch in text.chars() {
        app.handle_key(key(KeyCode::Char(ch)));
    }
}

/// Drain what the scripted agent produced for one prompt.
async fn drain(app: &mut App, updates: &mut UnboundedReceiver<Update>, count: usize) {
    for _ in 0..count {
        let update = updates.recv().await.expect("scripted update");
        app.apply(update);
    }
}

#[tokio::test]
async fn typing_and_pressing_enter_sends_the_prompt() {
    let (mut app, scripted, mut updates, _local) = app_with(vec![vec![
        Update::TurnStarted,
        Update::TextDelta("hi".to_string()),
        Update::TurnEnded(StopReason::EndTurn),
    ]]);

    type_text(&mut app, "hello");
    app.handle_key(key(KeyCode::Enter));

    assert_eq!(
        app.transcript.last(),
        Some(&Cell::User("hello".to_string()))
    );
    assert!(
        app.input.is_empty(),
        "a sent prompt must not stay in the box"
    );

    drain(&mut app, &mut updates, 3).await;
    assert_eq!(scripted.prompts(), vec!["hello".to_string()]);
    assert_eq!(app.turn(), Turn::Idle);
    assert!(
        app.transcript
            .cells()
            .contains(&Cell::Assistant("hi".to_string()))
    );
}

#[tokio::test]
async fn an_empty_prompt_is_not_sent() {
    let (mut app, scripted, _updates, _local) = app_with(Vec::new());
    app.handle_key(key(KeyCode::Enter));
    assert!(scripted.prompts().is_empty());
    assert!(app.transcript.is_empty());
}

#[tokio::test]
async fn shift_enter_writes_a_newline_instead_of_sending() {
    let (mut app, scripted, _updates, _local) = app_with(Vec::new());
    type_text(&mut app, "one");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
    type_text(&mut app, "two");

    assert_eq!(app.input.text(), "one\ntwo");
    assert!(scripted.prompts().is_empty());
}

#[test]
fn one_turn_of_prose_is_one_cell() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::TurnStarted);
    app.apply(Update::TextDelta("hel".to_string()));
    app.apply(Update::TextDelta("lo".to_string()));
    app.apply(Update::TurnEnded(StopReason::EndTurn));
    app.apply(Update::TurnStarted);
    app.apply(Update::TextDelta("again".to_string()));

    let prose: Vec<_> = app
        .transcript
        .cells()
        .iter()
        .filter_map(|cell| match cell {
            Cell::Assistant(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(prose, vec!["hello", "again"]);
}

#[test]
fn a_tool_result_revises_the_cell_the_call_opened() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::ToolCallStarted(call("c1", "read_file")));
    app.apply(Update::ToolCallEnded(ToolResult::ok(
        ToolCallId::new("c1"),
        "42 lines",
    )));

    let tools: Vec<_> = app
        .transcript
        .cells()
        .iter()
        .filter_map(|cell| match cell {
            Cell::Tool(tool) => Some(tool),
            _ => None,
        })
        .collect();
    assert_eq!(tools.len(), 1, "the result must not add a second cell");
    assert_eq!(tools[0].state, CallState::Finished(ToolStatus::Ok));
    assert_eq!(tools[0].detail.as_deref(), Some("42 lines"));
}

#[test]
fn a_result_for_an_unknown_call_is_reported_rather_than_dropped() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::ToolCallEnded(ToolResult::ok(
        ToolCallId::new("ghost"),
        "done",
    )));
    assert!(matches!(app.transcript.last(), Some(Cell::Error(_))));
}

#[tokio::test]
async fn a_permission_prompt_takes_the_letter_keys() {
    let (mut app, scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::PermissionRequested {
        id: PermissionId("p1".to_string()),
        call: call("c1", "bash"),
        reason: "runs a command".to_string(),
    });
    assert_eq!(app.turn(), Turn::AwaitingPermission);

    // 'y' answers; it must not land in the input box.
    app.handle_key(key(KeyCode::Char('y')));
    assert!(app.input.is_empty());
    assert_eq!(
        scripted.answers(),
        vec![(PermissionId("p1".to_string()), PermissionAnswer::Allow)]
    );
    assert_eq!(app.turn(), Turn::Running);
    assert!(app.open_permission_id().is_none());
}

#[test]
fn an_answered_prompt_keeps_the_decision_on_screen() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::PermissionRequested {
        id: PermissionId("p1".to_string()),
        call: call("c1", "bash"),
        reason: "runs a command".to_string(),
    });
    app.answer_permission(PermissionAnswer::Deny);

    let Some(Cell::Permission(prompt)) = app.transcript.last() else {
        panic!("the prompt must stay in the scrollback");
    };
    assert_eq!(prompt.answer, Some(PermissionAnswer::Deny));
}

#[test]
fn ctrl_c_cancels_a_turn_before_it_quits() {
    let (mut app, scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::TurnStarted);
    app.apply(Update::ToolCallStarted(call("c1", "bash")));

    app.handle_key(control('c'));
    assert_eq!(scripted.cancel_count(), 1);
    assert!(
        !app.should_quit(),
        "the first Ctrl-C must not exit mid-turn"
    );
    assert_eq!(app.turn(), Turn::Idle);

    app.handle_key(control('c'));
    assert!(app.should_quit());
}

#[test]
fn esc_cancels_a_turn_while_busy() {
    let (mut app, scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::TurnStarted);
    app.apply(Update::ToolCallStarted(call("c1", "bash")));

    app.handle_key(key(KeyCode::Esc));
    assert_eq!(scripted.cancel_count(), 1);
    assert_eq!(app.turn(), Turn::Idle);
    assert!(!app.should_quit());
}

#[test]
fn a_cancelled_turn_stops_every_running_spinner() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::TurnStarted);
    app.apply(Update::ToolCallStarted(call("c1", "bash")));
    app.apply(Update::ToolCallStarted(call("c2", "grep")));
    app.handle_key(control('c'));

    let running = app
        .transcript
        .cells()
        .iter()
        .filter(|cell| matches!(cell, Cell::Tool(tool) if tool.state == CallState::Running))
        .count();
    assert_eq!(running, 0);
}

#[test]
fn hiding_thinking_is_a_filter_not_a_deletion() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::ThinkingDelta("weighing options".to_string()));
    app.apply(Update::TextDelta("the answer".to_string()));

    let none = std::collections::HashSet::new();
    let shown = crate::draw::transcript::render(app.transcript.cells(), 40, true, &none);
    let hidden = crate::draw::transcript::render(app.transcript.cells(), 40, false, &none);
    assert!(shown.lines.len() > hidden.lines.len());
    // The cell is still there; only the rendering changed.
    app.handle_key(control('t'));
    assert!(!app.show_thinking());
    assert!(
        app.transcript
            .cells()
            .iter()
            .any(|cell| matches!(cell, Cell::Thinking(_)))
    );
}

#[tokio::test]
async fn copying_takes_the_last_reply_and_hands_it_over_once() {
    let (mut app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    app.apply(Update::TextDelta("first answer".to_string()));
    app.apply(Update::TurnEnded(StopReason::EndTurn));
    app.apply(Update::TextDelta("second answer".to_string()));

    copy_command(&mut app);
    assert_eq!(app.take_pending_copy().as_deref(), Some("second answer"));
    // Taken once: a copy that repeated itself every frame would fight the
    // terminal for the clipboard.
    assert_eq!(app.take_pending_copy(), None);
}

#[tokio::test]
async fn copying_says_so_in_the_status_bar_and_not_in_the_conversation() {
    let (mut app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    app.apply(Update::TextDelta("an answer".to_string()));
    let before = app.transcript.len();

    copy_command(&mut app);
    assert!(app.flash().is_some());
    // A line in the transcript reads as something the agent said.
    assert_eq!(app.transcript.len(), before);
}

#[tokio::test]
async fn copying_nothing_flashes_rather_than_copying_an_empty_clipboard() {
    let (mut app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    copy_command(&mut app);
    assert_eq!(app.take_pending_copy(), None);
    assert_eq!(app.flash(), Some("nothing to copy yet"));
}

/// A prompt taller than the box scrolls inside it rather than hiding the
/// cursor, and the count under the transcript is how a reader who scrolled
/// away learns output is still arriving.
#[test]
fn a_long_prompt_keeps_its_cursor_on_screen() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    for at in 0..20 {
        app.input
            .set_text(&format!("{}line {at}", app.input.text() + "\n"));
    }
    let rows = crate::draw::input::rows(&app);
    // Bounded: the transcript keeps the screen no matter how long the prompt.
    assert_eq!(rows, crate::draw::input::MAX_ROWS + 2);

    let (row, _) = app.input.cursor();
    let visible = usize::from(rows - 2);
    assert!(row >= visible, "the cursor is past the bottom of the box");
}

/// With the mouse captured (the default), the wheel reaches keke as its own
/// event rather than a faked arrow key, so an empty composer is free to give
/// plain Up/Down to history recall — what most people reach for first.
#[test]
fn the_arrows_recall_history_when_nothing_is_typed_and_the_mouse_is_captured() {
    let (app, _scripted, _updates, _local) = app_with(Vec::new());
    let mut app = app.with_prompt_history(crate::PromptHistory::new(vec!["old".to_string()]));
    assert!(app.mouse_capture());

    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.input.text(), "old");
}

/// Once the mouse is handed back with `/mouse`, a terminal not in
/// mouse-reporting mode turns the wheel into arrow keys, so an empty composer
/// has to give plain Up/Down to the transcript instead.
#[test]
fn the_arrows_scroll_the_conversation_when_the_mouse_is_released() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.toggle_mouse_capture();
    app.scroll.measure(100, 10);

    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.scroll.pinned_top(), Some(89));
    assert!(app.input.is_empty(), "the prompt box was left alone");
    app.handle_key(key(KeyCode::Down));
    assert!(app.scroll.is_following());
}

/// The count of what is below is a button: clicking it goes back to the tail.
#[test]
fn clicking_the_count_below_follows_the_conversation_again() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.scroll.measure(100, 10);
    app.scroll.scroll_up(20);
    app.set_follow_button(Some((10, 9, 16)));

    // A click beside it is not a click on it.
    app.handle_mouse(click(2, 9));
    assert!(!app.scroll.is_following());

    app.handle_mouse(click(12, 9));
    assert!(app.scroll.is_following());
}

#[test]
fn the_wheel_scrolls_the_conversation() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.scroll.measure(100, 10);
    app.handle_mouse(wheel(true));
    assert_eq!(app.scroll.pinned_top(), Some(87));
    // Back at the bottom is following again, not a pin that happens to equal it.
    app.handle_mouse(wheel(false));
    assert!(app.scroll.is_following());
}

#[test]
fn scrolling_back_counts_what_is_below_and_following_does_not() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.scroll.measure(50, 10);
    assert_eq!(app.scroll.below(), 0);

    app.scroll.scroll_up(5);
    assert_eq!(app.scroll.below(), 5);
    app.handle_key(control('l'));
    assert_eq!(app.scroll.below(), 0);
}

#[test]
fn a_failure_does_not_end_the_conversation() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::TurnStarted);
    app.apply(Update::Failed("provider said no".to_string()));
    assert_eq!(app.turn(), Turn::Idle);
    assert!(!app.should_quit());
}

#[tokio::test]
async fn a_prompt_that_never_left_surfaces_as_an_error() {
    // The scripted conversation always succeeds, so drive the local channel the
    // way `submit` would on a transport failure.
    let (mut app, _scripted, _updates, mut local) = app_with(vec![vec![Update::TurnStarted]]);
    type_text(&mut app, "hi");
    app.handle_key(key(KeyCode::Enter));
    // Nothing failed, so nothing should arrive locally.
    assert!(local.try_recv().is_err());
    app.apply(Update::Failed("pipe closed".to_string()));
    assert!(matches!(app.transcript.last(), Some(Cell::Error(_))));
}

#[test]
fn scrolling_back_pins_the_view_while_output_arrives() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    for index in 0..50 {
        app.transcript
            .push(Cell::Assistant(format!("line {index}")));
    }
    app.scroll.measure(100, 10);
    app.scroll.page_up();
    let pinned = app.scroll.pinned_top();
    assert!(pinned.is_some(), "paging up must leave the tail");

    // More output arrives; the view must not jump.
    app.apply(Update::TextDelta("new".to_string()));
    app.scroll.measure(120, 10);
    assert_eq!(app.scroll.pinned_top(), pinned);
    assert!(!app.scroll.is_following());
}

#[test]
fn paging_down_to_the_bottom_resumes_following() {
    let mut scroll = crate::Scrollback::default();
    scroll.measure(100, 10);
    scroll.page_up();
    assert!(!scroll.is_following());
    for _ in 0..20 {
        scroll.page_down();
    }
    assert!(
        scroll.is_following(),
        "reaching the bottom must go live again"
    );
}

#[test]
fn ctrl_l_jumps_back_to_the_tail() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.scroll.measure(100, 10);
    app.handle_key(control('c'));
    app.scroll.page_up();
    app.handle_key(control('l'));
    assert!(app.scroll.is_following());
}

#[test]
fn the_input_box_edits_multibyte_text_by_character() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    type_text(&mut app, "日本語");
    app.handle_key(key(KeyCode::Left));
    app.handle_key(key(KeyCode::Backspace));
    assert_eq!(app.input.text(), "日語");
    app.handle_key(key(KeyCode::Home));
    app.handle_key(key(KeyCode::Delete));
    assert_eq!(app.input.text(), "語");
}

#[test]
fn a_login_notice_is_shown_without_printing_over_the_interface() {
    use keke_auth_api::LoginUi;

    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    let (ui, mut notices) = crate::TuiLoginUi::new();
    ui.open_browser("https://auth.example/authorize");

    let notice = notices.try_recv().expect("a notice");
    app.apply_notice(notice);
    let Some(Cell::Notice(text)) = app.transcript.last() else {
        panic!("the URL must reach the transcript");
    };
    assert!(text.contains("https://auth.example/authorize"));
}

#[test]
fn a_refusal_is_shown_rather_than_ending_silently() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::TurnStarted);
    app.apply(Update::TurnEnded(StopReason::Refusal {
        message: "policy".to_string(),
    }));
    assert!(matches!(app.transcript.last(), Some(Cell::Error(_))));
}

#[test]
fn tool_arguments_collapse_to_one_line() {
    let summary = crate::transcript::summarize_arguments(&serde_json::json!({
        "command": "echo hi\nsleep 1",
        "items": [1, 2, 3],
    }));
    assert!(!summary.contains('\n'));
    assert!(summary.contains("command=echo hi sleep 1"));
    assert!(summary.contains("items=[3 items]"));
}

#[test]
fn wrapped_text_keeps_its_block_shape() {
    let cells = vec![Cell::User("a b c d e f g h i j".to_string())];
    let lines = crate::draw::transcript::render(&cells, 12, true, &Default::default()).lines;
    let rendered: Vec<String> = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect();
    assert!(rendered[0].starts_with("› "));
    assert!(
        rendered
            .iter()
            .skip(1)
            .take_while(|line| !line.is_empty())
            .all(|line| line.starts_with("  ")),
        "continuation lines must line up under the first: {rendered:?}"
    );
}

#[test]
fn content_blocks_other_than_text_do_not_fake_a_detail_line() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::ToolCallStarted(call("c1", "read_file")));
    app.apply(Update::ToolCallEnded(ToolResult {
        id: ToolCallId::new("c1"),
        status: ToolStatus::Ok,
        content: vec![ContentBlock::text("   ")],
        value: None,
    }));
    let Some(Cell::Tool(tool)) = app.transcript.last() else {
        panic!("the tool cell must be there");
    };
    assert_eq!(tool.detail, None);
}

// --- slash commands and approval modes --------------------------------------

/// The same helper, with a command list a person can complete against.
fn app_with_commands(
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

fn shift(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::SHIFT)
}

#[tokio::test]
async fn typing_a_slash_opens_the_command_menu() {
    let (mut app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    type_text(&mut app, "/hel");

    let names: Vec<&str> = app
        .completions()
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(names, vec!["help"]);

    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.input.text(), "/help ");
}

/// The menu closes once the name is settled, so arguments are ordinary typing.
#[tokio::test]
async fn the_menu_closes_once_arguments_are_being_typed() {
    let (mut app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    type_text(&mut app, "/effort high");
    assert!(app.completions().is_empty());
}

#[tokio::test]
async fn a_command_runs_instead_of_reaching_the_model() {
    let (mut app, scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    type_text(&mut app, "/help");
    app.handle_key(key(KeyCode::Enter));

    assert!(scripted.prompts().is_empty(), "a command is not a prompt");
    assert!(matches!(app.transcript.last(), Some(Cell::Notice(text)) if text.contains("/effort")));
}

#[tokio::test]
async fn an_unknown_command_is_reported_rather_than_sent() {
    let (mut app, scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    type_text(&mut app, "/nope");
    app.handle_key(key(KeyCode::Enter));

    assert!(scripted.prompts().is_empty());
    assert!(matches!(app.transcript.last(), Some(Cell::Error(text)) if text.contains("/nope")));
}

/// A prompt that opens with a path is prose, not a command.
#[tokio::test]
async fn a_leading_path_is_still_a_prompt() {
    let (mut app, scripted, mut updates, _local) = app_with_commands(
        vec![vec![
            Update::TurnStarted,
            Update::TurnEnded(StopReason::EndTurn),
        ]],
        Vec::new(),
    );
    type_text(&mut app, "/usr/bin/env is missing");
    app.handle_key(key(KeyCode::Enter));
    drain(&mut app, &mut updates, 2).await;

    assert_eq!(
        scripted.prompts(),
        vec!["/usr/bin/env is missing".to_string()]
    );
}

#[tokio::test]
async fn a_plugin_command_sends_its_file_as_the_prompt() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("review.md");
    std::fs::write(&path, "Review the diff.").expect("writing the command file");

    let (mut app, scripted, mut updates, _local) = app_with_commands(
        vec![vec![
            Update::TurnStarted,
            Update::TurnEnded(StopReason::EndTurn),
        ]],
        vec![crate::SlashCommand::prompt(
            "reviewer",
            "review",
            "review the diff",
            path,
        )],
    );
    type_text(&mut app, "/review carefully");
    app.handle_key(key(KeyCode::Enter));
    drain(&mut app, &mut updates, 2).await;

    assert_eq!(
        scripted.prompts(),
        vec!["Review the diff.\n\ncarefully".to_string()]
    );
    // What the person typed is what they see; the body went to the model.
    assert!(
        app.transcript
            .cells()
            .contains(&Cell::User("/review carefully".to_string()))
    );
}

#[tokio::test]
async fn a_command_file_that_cannot_be_read_is_reported_not_sent() {
    let (mut app, scripted, _updates, _local) = app_with_commands(
        Vec::new(),
        vec![crate::SlashCommand::prompt(
            "reviewer",
            "review",
            "review the diff",
            "/nonexistent/review.md",
        )],
    );
    type_text(&mut app, "/review");
    app.handle_key(key(KeyCode::Enter));

    assert!(scripted.prompts().is_empty());
    assert!(matches!(app.transcript.last(), Some(Cell::Error(_))));
}

#[tokio::test]
async fn shift_tab_cycles_the_approval_mode_and_tells_the_agent() {
    let (mut app, scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    assert_eq!(app.approval_policy(), ApprovalPolicy::OnRequest);

    app.handle_key(key(KeyCode::BackTab));
    assert_eq!(app.approval_policy(), ApprovalPolicy::OnFailure);
    // The gesture is silent: the status bar already says which mode is on, and
    // a line per tap would push the conversation off screen to repeat it.
    assert!(app.transcript.is_empty(), "{:?}", app.transcript.cells());
    app.handle_key(shift(KeyCode::Tab));
    assert_eq!(app.approval_policy(), ApprovalPolicy::Never);
    app.handle_key(key(KeyCode::BackTab));
    assert_eq!(app.approval_policy(), ApprovalPolicy::OnRequest);

    // The surface's idea of the mode is worthless unless the agent has it too.
    assert_eq!(
        scripted.policies(),
        vec![
            ApprovalPolicy::OnFailure,
            ApprovalPolicy::Never,
            ApprovalPolicy::OnRequest,
        ]
    );
}

#[tokio::test]
async fn clear_empties_the_screen_and_quit_leaves() {
    let (mut app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    app.apply(Update::TextDelta("hi".to_string()));

    type_text(&mut app, "/clear");
    app.handle_key(key(KeyCode::Enter));
    assert!(app.transcript.is_empty());

    type_text(&mut app, "/quit");
    app.handle_key(key(KeyCode::Enter));
    assert!(app.should_quit());
}

/// The level the agent is asked for must follow what the surface shows, and a
/// typo must move neither.
#[tokio::test]
async fn the_effort_command_names_a_level_and_refuses_a_typo() {
    let (mut app, scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());

    type_text(&mut app, "/effort xhigh");
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.reasoning_effort(), Some(ReasoningEffort::XHigh));
    assert!(app.transcript.is_empty(), "a clean switch says nothing");

    type_text(&mut app, "/effort hgih");
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(
        app.reasoning_effort(),
        Some(ReasoningEffort::XHigh),
        "a typo must not move the level"
    );
    assert!(matches!(app.transcript.last(), Some(Cell::Error(_))));

    // Unset is reachable again: the model's own default is a level too.
    type_text(&mut app, "/effort default");
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.reasoning_effort(), None);

    assert_eq!(
        scripted.efforts(),
        vec![Some(ReasoningEffort::XHigh), None],
        "the surface's idea of the level is worthless unless the agent has it"
    );
}

/// `/new` is the name people reach for; it does what `/clear` does.
#[tokio::test]
async fn new_clears_the_screen_like_clear() {
    let (mut app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    app.apply(Update::TextDelta("some output".to_string()));
    assert!(!app.transcript.is_empty());

    type_text(&mut app, "/new");
    app.handle_key(key(KeyCode::Enter));

    assert!(app.transcript.is_empty(), "{:?}", app.transcript.cells());
}

/// A person watching a turn wants the clock and the cost, and wants the clock
/// to stop rather than keep running once the answer is up.
#[tokio::test]
async fn a_turn_is_timed_and_its_tokens_are_counted() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    assert_eq!(app.elapsed(), None);
    assert!(!app.is_timing());

    app.apply(Update::TurnStarted);
    assert!(app.is_timing());
    app.apply(Update::TokensUsed(keke_protocol::Usage {
        input_tokens: 100,
        output_tokens: 20,
        ..keke_protocol::Usage::default()
    }));
    app.apply(Update::TokensUsed(keke_protocol::Usage {
        input_tokens: 5,
        ..keke_protocol::Usage::default()
    }));
    app.apply(Update::TurnEnded(StopReason::EndTurn));

    assert!(!app.is_timing(), "the clock stops when the turn does");
    assert!(app.elapsed().is_some(), "how long it took is still shown");
    assert_eq!(app.usage().total(), 125);
}

/// A resumed session shows what was said, so the screen and the next request
/// agree about the conversation.
#[tokio::test]
async fn a_resumed_history_is_replayed_onto_the_screen() {
    use keke_protocol::Message;
    use keke_protocol::Role;

    let asked = call("c1", "read_file");
    let history = vec![
        Message::user("read it"),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolCall(asked.clone())],
        },
        Message {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult(ToolResult::ok(
                asked.id.clone(),
                "the file",
            ))],
        },
        Message::assistant("here it is"),
    ];

    let (scripted, _updates) = ScriptedConversation::new(Vec::new());
    let app = App::new(Arc::new(scripted) as Arc<_>)
        .0
        .with_history(&history, keke_protocol::Usage::default());

    let cells = app.transcript.cells();
    assert!(matches!(&cells[0], Cell::User(text) if text == "read it"));
    assert!(matches!(
        &cells[1],
        Cell::Tool(tool) if tool.state == CallState::Finished(ToolStatus::Ok)
    ));
    assert!(matches!(&cells[2], Cell::Assistant(text) if text == "here it is"));
}

/// Ctrl-P brings back what was typed before, newest first.
#[test]
fn ctrl_p_recalls_the_last_prompt() {
    let (app, _scripted, _updates, _local) = app_with(vec![vec![]]);
    let mut app = app.with_prompt_history(crate::PromptHistory::new(vec![
        "first".to_string(),
        "second".to_string(),
    ]));

    app.handle_key(control('p'));
    assert_eq!(app.input.text(), "second");
    app.handle_key(control('p'));
    assert_eq!(app.input.text(), "first");
    // The oldest is the end of the road, not a wrap-around to the newest.
    app.handle_key(control('p'));
    assert_eq!(app.input.text(), "first");
}

/// Walking forward again ends at the person's own unsent draft.
#[test]
fn the_down_arrow_gives_the_interrupted_draft_back() {
    let (app, _scripted, _updates, _local) = app_with(vec![vec![]]);
    let mut app = app.with_prompt_history(crate::PromptHistory::new(vec!["old".to_string()]));

    type_text(&mut app, "half typed");
    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.input.text(), "old");
    app.handle_key(key(KeyCode::Down));
    assert_eq!(app.input.text(), "half typed");
}

/// A multi-line prompt is edited with the arrow keys before it is left, so the
/// history only takes over from the edge lines.
#[test]
fn the_arrows_move_within_a_multi_line_prompt_first() {
    let (app, _scripted, _updates, _local) = app_with(vec![vec![]]);
    let mut app = app.with_prompt_history(crate::PromptHistory::new(vec!["old".to_string()]));

    type_text(&mut app, "one");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
    type_text(&mut app, "two");

    app.handle_key(key(KeyCode::Up));
    assert_eq!(
        app.input.text(),
        "one\ntwo",
        "the first Up stays in the box"
    );
    assert_eq!(app.input.cursor().0, 0);
    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.input.text(), "old");
}

/// What was just sent is at the top of the history, without a restart.
#[tokio::test]
async fn a_submitted_prompt_joins_the_history() {
    let (app, _scripted, _updates, _local) = app_with(vec![vec![]]);
    let mut app = app.with_prompt_history(crate::PromptHistory::default());

    type_text(&mut app, "do the thing");
    app.handle_key(key(KeyCode::Enter));
    assert!(app.input.is_empty());

    app.handle_key(control('p'));
    assert_eq!(app.input.text(), "do the thing");
}

/// The readline bindings a terminal person already has everywhere else.
#[test]
fn emacs_keys_move_and_delete_in_the_prompt() {
    let (app, _scripted, _updates, _local) = app_with(vec![vec![]]);
    let mut app = app;

    type_text(&mut app, "git commit -m wip");
    app.handle_key(control('w'));
    assert_eq!(app.input.text(), "git commit -m ");

    app.handle_key(control('a'));
    assert_eq!(app.input.cursor().1, 0);
    app.handle_key(control('e'));
    assert_eq!(app.input.cursor().1, "git commit -m ".chars().count());

    app.handle_key(control('b'));
    app.handle_key(control('k'));
    assert_eq!(app.input.text(), "git commit -m");

    app.handle_key(control('u'));
    assert!(app.input.is_empty());
}

/// Flatten a rendered transcript to plain strings.
fn rendered(app: &App) -> Vec<String> {
    crate::draw::transcript::render(app.transcript.cells(), 80, true, app.expanded())
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
fn finished_reads(app: &mut App, count: usize) {
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

#[test]
fn a_call_is_named_by_what_it_acted_on_not_by_its_argument_names() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    finished_reads(&mut app, 1);

    let header = rendered(&app)
        .into_iter()
        .find(|line| line.contains("Read"))
        .expect("the call must be drawn");
    assert!(header.contains("src/f0.rs"), "{header}");
    assert!(!header.contains("path="), "{header}");
}

#[test]
fn a_run_of_finished_calls_collapses_to_one_countable_line() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    finished_reads(&mut app, 3);

    let lines = rendered(&app);
    assert!(
        lines.iter().any(|line| line.contains("Read 3 files")),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("src/f1.rs")),
        "a collapsed run must not still list its calls: {lines:?}"
    );
}

#[test]
fn expanding_a_run_shows_every_call_in_it_and_collapsing_hides_them_again() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    finished_reads(&mut app, 3);
    // The map of what is on screen is a frame's, so draw one first.
    crate::draw::transcript::render(app.transcript.cells(), 80, true, app.expanded());
    app.toggle_last_expandable();

    let lines = rendered(&app);
    assert!(
        lines.iter().any(|line| line.contains("src/f2.rs")),
        "{lines:?}"
    );
    app.toggle_last_expandable();
    assert!(
        !rendered(&app).iter().any(|line| line.contains("src/f2.rs")),
        "expanding must be reversible"
    );
}

#[test]
fn a_failure_in_a_run_is_visible_without_expanding_it() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    finished_reads(&mut app, 2);
    app.apply(Update::ToolCallStarted(call("bad", "read_file")));
    app.apply(Update::ToolCallEnded(ToolResult {
        id: ToolCallId::new("bad"),
        status: ToolStatus::Error,
        content: Vec::new(),
        value: None,
    }));

    let lines = rendered(&app);
    assert!(
        lines.iter().any(|line| line.starts_with('✗')),
        "a collapsed run reports the worst status in it: {lines:?}"
    );
}

#[test]
fn a_finished_thought_collapses_while_the_one_still_arriving_stays_open() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::ThinkingDelta("weighing options".to_string()));
    assert!(
        rendered(&app)
            .iter()
            .any(|line| line.contains("weighing options")),
        "the thought being streamed is the one being read"
    );

    app.apply(Update::TextDelta("the answer".to_string()));
    let lines = rendered(&app);
    assert!(
        !lines.iter().any(|line| line.contains("weighing options")),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|line| line.contains("Thought")),
        "{lines:?}"
    );
}

/// A drag over the transcript is a selection, and letting go copies it —
/// which is what a captured mouse owes the person it took drag-select from.
#[test]
fn dragging_across_a_line_copies_what_was_dragged_over() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.selection.set_rows(0, vec!["hello world".to_string()]);

    app.handle_mouse(mouse(
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        0,
        0,
    ));
    app.handle_mouse(mouse(
        crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        4,
        0,
    ));
    app.handle_mouse(mouse(
        crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
        4,
        0,
    ));

    assert_eq!(app.take_pending_copy().as_deref(), Some("hello"));
}

/// A drag down the screen takes the rows between its ends whole.
#[test]
fn dragging_over_several_rows_copies_them_in_reading_order() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.selection.set_rows(
        0,
        vec![
            "first line".to_string(),
            "second line".to_string(),
            "third line".to_string(),
        ],
    );

    app.handle_mouse(mouse(
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        6,
        0,
    ));
    app.handle_mouse(mouse(
        crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        4,
        2,
    ));
    app.handle_mouse(mouse(
        crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
        4,
        2,
    ));

    assert_eq!(
        app.take_pending_copy().as_deref(),
        Some("line\nsecond line\nthird")
    );
}

/// The same gesture without the drag is still a click, so a captured mouse
/// keeps expanding tool calls.
#[test]
fn a_press_and_release_in_one_place_is_a_click_not_a_selection() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.selection.set_rows(0, vec!["hello world".to_string()]);
    app.set_toggles(vec![(0, 7)]);

    app.handle_mouse(mouse(
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        3,
        0,
    ));
    app.handle_mouse(mouse(
        crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
        3,
        0,
    ));

    assert_eq!(app.take_pending_copy(), None);
    assert!(app.expanded().contains(&7), "the click must reach the row");
}

/// A wide character occupies two cells, and the cursor sits after both.
#[test]
fn the_cursor_is_measured_in_cells_not_characters() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());

    type_text(&mut app, "你好a");
    assert_eq!(app.input.cursor().1, 3);
    assert_eq!(app.input.cursor_display().1, 5);

    app.handle_key(key(KeyCode::Left));
    assert_eq!(app.input.cursor_display().1, 4);
    app.handle_key(key(KeyCode::Left));
    assert_eq!(app.input.cursor_display().1, 2);
}

/// A paste is one edit: its newlines make lines, not submits.
#[test]
fn a_pasted_block_keeps_its_line_breaks_instead_of_submitting() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());

    app.handle_paste("你好\r\n世界\rthere");
    assert_eq!(app.input.text(), "你好\n世界\nthere");
    assert_eq!(app.input.cursor(), (2, 5));
}

/// A paste lands at the cursor, not at the end of the buffer.
#[test]
fn a_paste_lands_where_the_cursor_is() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());

    type_text(&mut app, "ab");
    app.handle_key(key(KeyCode::Left));
    app.handle_paste("中");
    assert_eq!(app.input.text(), "a中b");
    assert_eq!(app.input.cursor_display().1, 3);
}

/// A model's ladder, as a provider would publish it.
fn served(id: &str, name: &str, efforts: &[ReasoningEffort]) -> keke_provider_api::ModelInfo {
    let mut model = keke_provider_api::ModelInfo::new(id);
    model.display_name = name.to_string();
    model.context_window = Some(272_000);
    model.reasoning_efforts = efforts.to_vec();
    model
}

fn app_with_models() -> (
    App,
    Arc<ScriptedConversation>,
    UnboundedReceiver<Update>,
    UnboundedReceiver<Update>,
) {
    let (app, scripted, updates, local) = app_with_commands(Vec::new(), Vec::new());
    (
        app.with_models(
            "gpt-5.6-sol",
            vec![
                served(
                    "gpt-5.6-sol",
                    "GPT-5.6-Sol",
                    &[ReasoningEffort::Low, ReasoningEffort::Ultra],
                ),
                served(
                    "gpt-5.2",
                    "GPT-5.2",
                    &[ReasoningEffort::Low, ReasoningEffort::High],
                ),
            ],
        ),
        scripted,
        updates,
        local,
    )
}

/// A person asking what they can switch to gets a list that stays put and can
/// be chosen from, not a paragraph that scrolls away with the conversation.
#[tokio::test]
async fn the_model_command_opens_a_picker_over_what_the_provider_serves() {
    let (mut app, _scripted, _updates, _local) = app_with_models();

    type_text(&mut app, "/model");
    app.handle_key(key(KeyCode::Enter));

    assert!(app.model_picker().is_some());
    let ids: Vec<&str> = app
        .picker_models()
        .iter()
        .map(|model| model.id.as_str())
        .collect();
    assert_eq!(ids, vec!["gpt-5.6-sol", "gpt-5.2"]);
    // It opens on the model in force, not at the top: the commonest reason to
    // look is to see where you are.
    assert_eq!(app.picker_selected(), 0);
    assert!(app.transcript.is_empty());
}

/// Typing narrows the list, and enter switches to what is highlighted — the
/// whole point of the overlay is not having to retype an id you just read.
#[tokio::test]
async fn the_picker_filters_as_you_type_and_switches_on_enter() {
    let (mut app, scripted, _updates, _local) = app_with_models();

    type_text(&mut app, "/model");
    app.handle_key(key(KeyCode::Enter));
    for ch in "5.2".chars() {
        app.handle_key(key(KeyCode::Char(ch)));
    }

    let ids: Vec<&str> = app
        .picker_models()
        .iter()
        .map(|model| model.id.as_str())
        .collect();
    assert_eq!(ids, vec!["gpt-5.2"]);

    app.handle_key(key(KeyCode::Enter));
    assert!(app.model_picker().is_none());
    assert_eq!(app.model(), "gpt-5.2");
    assert_eq!(scripted.models(), vec!["gpt-5.2".to_string()]);
}

/// Esc leaves the session exactly as it was. A picker that switched on the way
/// out would make looking at the list a change.
#[tokio::test]
async fn escaping_the_picker_switches_nothing() {
    let (mut app, scripted, _updates, _local) = app_with_models();

    type_text(&mut app, "/model");
    app.handle_key(key(KeyCode::Enter));
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Esc));

    assert!(app.model_picker().is_none());
    assert_eq!(app.model(), "gpt-5.6-sol");
    assert!(scripted.models().is_empty());
}

/// A filter matching nothing has no row under the cursor, so enter means
/// nothing — the alternative is switching to whatever happened to be last.
#[tokio::test]
async fn a_picker_matching_nothing_accepts_nothing() {
    let (mut app, scripted, _updates, _local) = app_with_models();

    type_text(&mut app, "/model");
    app.handle_key(key(KeyCode::Enter));
    for ch in "zzz".chars() {
        app.handle_key(key(KeyCode::Char(ch)));
    }
    app.handle_key(key(KeyCode::Enter));

    assert!(app.model_picker().is_some());
    assert!(scripted.models().is_empty());
}

/// A provider that published no list leaves nothing to open, and says so
/// rather than showing an empty box.
#[tokio::test]
async fn the_model_command_without_a_list_says_so() {
    let (app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    let mut app = app.with_models("some-model", Vec::new());

    type_text(&mut app, "/model");
    app.handle_key(key(KeyCode::Enter));

    assert!(app.model_picker().is_none());
    let Some(Cell::Notice(text)) = app.transcript.last() else {
        panic!("expected a notice, got {:?}", app.transcript.last());
    };
    assert!(text.contains("published no model list"), "{text}");
}

#[tokio::test]
async fn the_model_command_switches_and_tells_the_agent() {
    let (mut app, scripted, _updates, _local) = app_with_models();

    type_text(&mut app, "/model gpt-5.2");
    app.handle_key(key(KeyCode::Enter));

    assert_eq!(app.model(), "gpt-5.2");
    assert_eq!(scripted.models(), vec!["gpt-5.2".to_string()]);
    assert!(app.transcript.is_empty());
}

/// Invariant 8: a model the provider does not serve is refused here, where the
/// person can still see what they typed, rather than on the next prompt.
#[tokio::test]
async fn a_model_the_provider_does_not_serve_is_refused() {
    let (mut app, scripted, _updates, _local) = app_with_models();

    type_text(&mut app, "/model gpt-4o");
    app.handle_key(key(KeyCode::Enter));

    assert_eq!(
        app.model(),
        "gpt-5.6-sol",
        "a refusal must not move the model"
    );
    assert!(scripted.models().is_empty());
    assert!(matches!(app.transcript.last(), Some(Cell::Error(text)) if text.contains("gpt-4o")));
}

/// With no list there is nothing to check against, so nothing is refused:
/// keke has no grounds to say a model does not exist.
#[tokio::test]
async fn without_a_list_any_model_is_accepted() {
    let (mut app, scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());

    type_text(&mut app, "/model something-keke-never-heard-of");
    app.handle_key(key(KeyCode::Enter));

    assert_eq!(app.model(), "something-keke-never-heard-of");
    assert_eq!(
        scripted.models(),
        vec!["something-keke-never-heard-of".to_string()]
    );
}

/// Cycling must stay on the ladder the model published, or every second tap
/// buys a request the endpoint will reject.
#[tokio::test]
async fn cycling_the_effort_stays_on_the_current_model_ladder() {
    let (mut app, _scripted, _updates, _local) = app_with_models();

    type_text(&mut app, "/effort");
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.reasoning_effort(), Some(ReasoningEffort::Low));

    type_text(&mut app, "/effort");
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(
        app.reasoning_effort(),
        Some(ReasoningEffort::Ultra),
        "gpt-5.6-sol offers low and ultra, so medium is not on this ladder"
    );

    type_text(&mut app, "/effort");
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.reasoning_effort(), None);
}

/// A level carried over from the previous model would be sent anyway and
/// rejected, so it is dropped where the cause is still on screen.
#[tokio::test]
async fn switching_to_a_model_without_the_current_level_drops_it() {
    let (mut app, scripted, _updates, _local) = app_with_models();

    type_text(&mut app, "/effort ultra");
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.reasoning_effort(), Some(ReasoningEffort::Ultra));

    type_text(&mut app, "/model gpt-5.2");
    app.handle_key(key(KeyCode::Enter));

    assert_eq!(app.reasoning_effort(), None);
    assert!(
        matches!(app.transcript.last(), Some(Cell::Notice(text)) if text.contains("does not take"))
    );
    assert_eq!(
        scripted.efforts(),
        vec![Some(ReasoningEffort::Ultra), None],
        "the agent must be told the level was dropped, not just the surface"
    );
}
