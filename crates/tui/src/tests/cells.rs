//! Cell rendering: tool-call collapsing, runs, diffs, scrolling, and
//! basic input/selection behavior.

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use keke_acp::PermissionAnswer;
use keke_acp::PermissionId;
use keke_acp::Update;
use keke_protocol::ContentBlock;
use keke_protocol::StopReason;
use keke_protocol::ToolCall;
use keke_protocol::ToolCallId;
use keke_protocol::ToolResult;
use keke_protocol::ToolStatus;

use crate::CallState;
use crate::Cell;
use crate::Turn;
use crate::tests::helpers::*;

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
    app.apply(Update::ToolCallStarted(call("c1", "bash")));
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
fn a_hosted_tool_call_is_shown_as_a_finished_call() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::HostedToolCall {
        name: "web_search".to_string(),
        query: Some("latest xAI Grok bot news".to_string()),
    });

    let tools: Vec<_> = app
        .transcript
        .cells()
        .iter()
        .filter_map(|cell| match cell {
            Cell::Tool(tool) => Some(tool),
            _ => None,
        })
        .collect();
    assert_eq!(tools.len(), 1, "the vendor's search must be on screen");
    assert_eq!(tools[0].name, "web_search");
    assert_eq!(tools[0].summary, "latest xAI Grok bot news");
    // Nothing will revise it, so it must never sit on screen as running.
    assert_eq!(tools[0].state, CallState::Finished(ToolStatus::Ok));
}

#[test]
fn a_hosted_tool_call_is_not_revised_by_a_later_result() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::HostedToolCall {
        name: "web_search".to_string(),
        query: None,
    });
    app.apply(Update::ToolCallEnded(ToolResult::ok(
        ToolCallId::new("hosted:web_search"),
        "done",
    )));

    assert!(
        app.transcript
            .cells()
            .iter()
            .any(|cell| matches!(cell, Cell::Error(_))),
        "a stray result must not be absorbed by the hosted call's cell"
    );
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
        vec![(
            PermissionId("p1".to_string()),
            PermissionAnswer::Allow,
            None
        )]
    );
    assert_eq!(app.turn(), Turn::Running);
    assert!(app.open_permission_id().is_none());
}

#[test]
fn answering_a_prompt_clears_it_rather_than_leaving_it_in_the_scrollback() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::PermissionRequested {
        id: PermissionId("p1".to_string()),
        call: call("c1", "bash"),
        reason: "runs a command".to_string(),
    });
    assert!(app.open_permission().is_some());

    app.answer_permission(PermissionAnswer::Deny);

    assert!(app.open_permission().is_none());
    assert!(
        app.transcript.cells().is_empty(),
        "the prompt was never a scrollback cell"
    );
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
    let rows = crate::draw::input::rows(&app, 80);
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
fn expanded_arguments_keep_their_line_breaks() {
    let expanded = crate::transcript::expanded_arguments(
        &serde_json::json!({
            "command": "echo hi\nsleep 1",
            "timeout": 30,
        }),
        Some("command"),
    );
    assert!(!expanded.contains("command="));
    assert_eq!(expanded, "timeout=30");
}

#[test]
fn wrapped_text_keeps_its_block_shape() {
    let cells = vec![Cell::User("a b c d e f g h i j".to_string())];
    let lines = crate::draw::transcript::render(&cells, 12, &Default::default()).lines;
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
    finished_commands(&mut app, 3);

    let lines = rendered(&app);
    assert!(
        lines.iter().any(|line| line.contains("Ran 3 commands")),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("echo 1")),
        "a collapsed run must not still list its calls: {lines:?}"
    );
}

#[test]
fn expanding_a_run_shows_every_call_in_it_and_collapsing_hides_them_again() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    finished_commands(&mut app, 3);
    // The map of what is on screen is a frame's, so draw one first.
    crate::draw::transcript::render(app.transcript.cells(), 80, app.expanded());
    app.toggle_last_expandable();

    let lines = rendered(&app);
    assert!(
        lines.iter().any(|line| line.contains("echo 2")),
        "{lines:?}"
    );
    app.toggle_last_expandable();
    assert!(
        !rendered(&app).iter().any(|line| line.contains("echo 2")),
        "expanding must be reversible"
    );
}

#[test]
fn a_successful_edit_shows_its_diff_without_expanding() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::ToolCallStarted(ToolCall {
        id: ToolCallId::new("c1"),
        name: "edit".to_string(),
        arguments: serde_json::json!({
            "path": "src/f0.rs",
            "old_string": "old",
            "new_string": "new",
        }),
    }));
    app.apply(Update::ToolCallEnded(ToolResult {
        id: ToolCallId::new("c1"),
        status: ToolStatus::Ok,
        content: vec![ContentBlock::text(
            "edited src/f0.rs (+1 -1, 1 replacement)",
        )],
        value: Some(serde_json::json!({
            "path": "src/f0.rs",
            "replacements": 1,
            "diff": { "added": 1, "removed": 1, "hunk": "-old\n+new\n" },
        })),
    }));

    let lines = rendered(&app);
    assert!(
        lines.iter().any(|line| line.contains("-old")),
        "an edit's diff should not need an extra click to see: {lines:?}"
    );
    assert!(lines.iter().any(|line| line.contains("+new")), "{lines:?}");

    // GitHub tints the whole row, not just the text — a row's background,
    // not its foreground colour, is what says "this line changed".
    let drawn = crate::draw::transcript::render(app.transcript.cells(), 80, app.expanded());
    let added_row = drawn
        .lines
        .iter()
        .find(|line| line.spans.iter().any(|span| span.content.contains("+new")))
        .expect("the added line must be drawn");
    assert!(
        added_row.spans.iter().any(|span| span.style.bg.is_some()),
        "an added line should carry a background tint, not just coloured text"
    );

    // The diff already says what changed; the raw `old_string=... new_string=...`
    // args dump would just say it again, less legibly.
    assert!(
        !lines.iter().any(|line| line.contains("old_string=")),
        "an edit's raw arguments are redundant once its diff is shown: {lines:?}"
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
fn a_running_call_is_open_without_any_toggle() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::ToolCallStarted(call("c0", "read_file")));

    let lines = rendered(&app);
    assert!(
        lines.iter().any(|line| line.contains("src/lib.rs")),
        "a call in flight is shown open, as it happens, with no toggle needed: {lines:?}"
    );
}

#[test]
fn a_run_that_errored_stays_open_after_it_finishes() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::ToolCallStarted(call("bad", "read_file")));
    app.apply(Update::ToolCallEnded(ToolResult {
        id: ToolCallId::new("bad"),
        status: ToolStatus::Error,
        content: Vec::new(),
        value: None,
    }));

    let lines = rendered(&app);
    assert!(
        lines.iter().any(|line| line.contains("src/lib.rs")),
        "an error stays open by default, not just its collapsed marker: {lines:?}"
    );
}

#[test]
fn a_clean_run_folds_away_once_it_finishes() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    finished_commands(&mut app, 3);

    let lines = rendered(&app);
    assert!(
        !lines.iter().any(|line| line.contains("echo 2")),
        "a run that finished cleanly folds away without a manual collapse: {lines:?}"
    );
}

#[test]
fn a_clean_exploration_run_stays_open() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    finished_reads(&mut app, 3);

    let lines = rendered(&app);
    assert!(
        lines.iter().any(|line| line.contains("src/f2.rs")),
        "an exploration run stays open even on a clean finish, so what it read is visible without a click: {lines:?}"
    );
}

#[test]
fn toggling_a_run_that_errored_collapses_it() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::ToolCallStarted(call("bad", "read_file")));
    app.apply(Update::ToolCallEnded(ToolResult {
        id: ToolCallId::new("bad"),
        status: ToolStatus::Error,
        content: Vec::new(),
        value: None,
    }));
    crate::draw::transcript::render(app.transcript.cells(), 80, app.expanded());
    app.toggle_last_expandable();

    let lines = rendered(&app);
    assert!(
        lines.iter().any(|line| line.starts_with('✗')),
        "the collapsed header still reports the failure: {lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("path=src/lib.rs")),
        "the toggle flips relative to the default-open state: {lines:?}"
    );
}

#[test]
fn a_reasoning_delta_never_reaches_the_transcript() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::ThinkingDelta("weighing options".to_string()));
    assert!(
        !rendered(&app)
            .iter()
            .any(|line| line.contains("weighing options")),
        "reasoning text is not a transcript cell; only turn_status marks it"
    );
    assert!(app.is_thinking());

    app.apply(Update::TextDelta("the answer".to_string()));
    assert!(!app.is_thinking(), "prose ends the thinking state");
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
