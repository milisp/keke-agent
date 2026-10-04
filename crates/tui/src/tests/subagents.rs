//! Recorded child transcripts and status rows.

use crossterm::event::KeyCode;
use keke_acp::Update;

use crate::tests::helpers::*;

fn view(id: &str, status: Option<&str>, input_tokens: u64) -> keke_acp::SubagentView {
    keke_acp::SubagentView {
        title: None,
        id: id.to_string(),
        task: format!("find {id}\nin the parser"),
        status: status.map(str::to_string),
        input_tokens,
    }
}

/// An empty snapshot retires the previous parent's children and their clocks.
#[test]
fn an_empty_snapshot_retires_the_previous_child_rows() {
    let (mut app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());

    app.apply(Update::Subagents(vec![view("agent_1", None, 0)]));
    assert_eq!(app.subagents().len(), 1);
    assert!(app.subagent_elapsed("agent_1").is_some());

    app.apply(Update::Subagents(Vec::new()));
    assert!(app.subagents().is_empty());
    assert!(
        app.subagent_elapsed("agent_1").is_none(),
        "the row's clock must go with the row"
    );
}

/// Opening uses this frame's actual hit targets to choose the child's record.
#[test]
fn clicking_a_subagent_row_selects_its_record() {
    let (mut app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    app.apply(Update::Subagents(vec![
        view("agent_1", None, 120),
        view("agent_2", Some("completed"), 300),
    ]));
    app.set_subagent_rows(vec![(7, "agent_1".to_string()), (8, "agent_2".to_string())]);

    assert!(!app.open_subagent_at(9), "no row was drawn there");
    assert!(app.open_subagent().is_none());

    assert!(app.open_subagent_at(8));
    assert_eq!(app.open_subagent().expect("open").id, "agent_2");

    // The same row again closes it: the row is the only handle on the child view,
    // so it has to work both ways.
    assert!(app.open_subagent_at(8));
    assert!(app.open_subagent().is_none());
}

/// Escape returns from a child before it reaches the parent turn.
#[test]
fn escape_closes_the_subagent_view_before_it_interrupts_the_turn() {
    let (mut app, scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::TurnStarted);
    app.apply(Update::Subagents(vec![view("agent_1", None, 120)]));
    app.set_subagent_rows(vec![(7, "agent_1".to_string())]);
    assert!(app.open_subagent_at(7));

    app.handle_key(key(KeyCode::Esc));
    assert!(app.open_subagent().is_none());
    assert_eq!(
        scripted.cancel_count(),
        0,
        "the turn must survive closing a child view"
    );

    app.handle_key(key(KeyCode::Esc));
    assert_eq!(scripted.cancel_count(), 1);
}

/// Finished outcomes remain in history, even though their live rows disappear.
#[test]
fn a_finished_subagent_keeps_its_outcome_for_inspection() {
    let (mut app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    app.apply(Update::Subagents(vec![view("agent_1", None, 120)]));
    app.apply(Update::Subagents(vec![view(
        "agent_1",
        Some("failed"),
        400,
    )]));

    let row = &app.subagents()[0];
    assert_eq!(row.status.as_deref(), Some("failed"));
    assert_eq!(row.input_tokens, 400);
}

/// Starting over clears what the last session delegated: a row left behind
/// would refer to an agent no conversation on screen ever asked for.
#[test]
fn a_new_session_takes_the_subagent_rows_with_it() {
    let (mut app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    app.apply(Update::Subagents(vec![view("agent_1", None, 120)]));
    app.apply(Update::SessionReset);
    assert!(app.subagents().is_empty());
}

// --- /mcp -------------------------------------------------------------------

#[tokio::test]
async fn opening_a_child_reads_its_record_and_preserves_the_parent_view() {
    use keke_protocol::{Message, SessionEvent, TurnId};
    let (mut app, scripted, _updates, _local) = app_with(Vec::new());
    let events = vec![SessionEvent::TurnStart {
        turn: TurnId::new(),
        input: Message::user("full child task with details"),
        approval_policy: None,
    }];
    scripted.with_subagent_transcript("agent_1".to_string(), events.clone());
    app.transcript
        .push(crate::Cell::Assistant("parent response".to_string()));
    app.input.set_text("unfinished parent prompt");
    app.scroll.measure(200, 10);
    app.scroll.scroll_up(50);
    let parent_scroll = app.scroll;
    app.apply(Update::Subagents(vec![view("agent_1", None, 120)]));
    app.set_subagent_rows(vec![(7, "agent_1".to_string())]);
    assert!(app.open_subagent_at(7));
    tokio::task::yield_now().await;
    app.tick_subagent_recording();
    assert_eq!(
        app.subagent_recording.transcript.cells(),
        &[crate::Cell::User(
            "full child task with details".to_string()
        )]
    );
    app.handle_key(key(KeyCode::Char('x')));
    app.handle_paste("hidden paste");
    app.handle_key(key(KeyCode::PageUp));
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.input.text(), "unfinished parent prompt");
    assert_eq!(app.scroll, parent_scroll);
    assert_eq!(
        app.transcript.cells(),
        &[crate::Cell::Assistant("parent response".to_string())]
    );
    assert_eq!(scripted.cancel_count(), 0);
}

#[test]
fn keyboard_cycles_only_running_children_and_history_opens_completed_children() {
    use crossterm::event::{KeyEvent, KeyModifiers};
    let (mut app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    app.apply(Update::Subagents(vec![
        view("agent_1", None, 0),
        view("agent_2", Some("completed"), 0),
    ]));
    let next = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
    app.handle_key(next);
    assert_eq!(app.open_subagent().unwrap().id, "agent_1");
    app.handle_key(next);
    assert_eq!(app.open_subagent().unwrap().id, "agent_1");
    app.handle_key(key(KeyCode::Char('q')));
    app.input.set_text("/subagents");
    app.handle_key(key(KeyCode::Enter));
    assert!(app.subagent_history.is_some());
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.open_subagent().unwrap().id, "agent_2");
    app.apply(Update::Subagents(vec![
        view("agent_1", None, 0),
        view("agent_2", Some("completed"), 0),
    ]));
    assert!(
        app.open_subagent().is_some(),
        "history inspection stays open"
    );
    app.handle_key(key(KeyCode::Char('q')));
    assert!(app.open_subagent().is_none());
}

#[test]
fn the_child_view_renders_recorded_messages_instead_of_only_the_task() {
    use keke_protocol::{Message, SessionEvent, TurnId};
    use ratatui::{Terminal, backend::TestBackend};
    let (mut app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    app.apply(Update::Subagents(vec![view(
        "agent_1",
        Some("completed"),
        0,
    )]));
    app.set_subagent_rows(vec![(7, "agent_1".to_string())]);
    app.open_subagent_at(7);
    let events = vec![SessionEvent::TurnStart {
        turn: TurnId::new(),
        input: Message::user("recorded child message beyond the row title"),
        approval_policy: None,
    }];
    app.subagent_recording.transcript.replay_recorded(&events);
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::draw::draw(frame, &mut app))
        .unwrap();
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("recorded child message beyond the row title"));
    assert!(rendered.contains("Ctrl+G next agent"));
    assert!(!rendered.contains("approval_policy:"));
    assert!(!rendered.contains("turn start"));
    assert!(rendered.contains("› recorded child message"));
}

#[test]
fn completion_removes_the_live_title_and_returns_to_the_parent() {
    use ratatui::{Terminal, backend::TestBackend};
    let (mut app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    app.apply(Update::Subagents(vec![view("agent_1", None, 0)]));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| crate::draw::draw(frame, &mut app))
        .unwrap();
    app.handle_mouse(click(3, 23));
    assert_eq!(app.open_subagent().unwrap().id, "agent_1");
    app.apply(Update::Subagents(vec![view(
        "agent_1",
        Some("completed"),
        0,
    )]));
    assert!(app.open_subagent().is_none());
    assert_eq!(crate::draw::subagents::rows(&app), 0);
    terminal
        .draw(|frame| crate::draw::draw(frame, &mut app))
        .unwrap();
    assert!(
        !app.open_subagent_at(23),
        "completed row has no live click target"
    );
    app.input.set_text("/subagents");
    app.handle_key(key(KeyCode::Enter));
    terminal
        .draw(|frame| crate::draw::draw(frame, &mut app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let row = (0..24)
        .find(|y| {
            (0..80)
                .map(|x| buffer[(x, *y)].symbol())
                .collect::<String>()
                .contains("agent_1 · completed")
        })
        .unwrap();
    app.handle_mouse(click(3, row));
    assert_eq!(app.open_subagent().unwrap().id, "agent_1");
}

#[tokio::test]
async fn clicking_the_bottom_short_title_loads_that_childs_complete_messages() {
    use keke_protocol::{Message, SessionEvent, StopReason, TurnId, Usage};
    use ratatui::{Terminal, backend::TestBackend};
    let (mut app, scripted, _updates, _local) = app_with(Vec::new());
    let mut agent = view("agent_2", None, 0);
    agent.title = Some("Inspect parser".to_string());
    agent.task = "A very long instruction that must never be used as the row title".to_string();
    let turn = TurnId::new();
    scripted.with_subagent_transcript(
        agent.id.clone(),
        vec![
            SessionEvent::TurnStart {
                turn,
                input: Message::user(agent.task.clone()),
                approval_policy: None,
            },
            SessionEvent::ModelResponse {
                turn,
                message: Message::assistant("Complete child reply"),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            },
        ],
    );
    app.apply(Update::Subagents(vec![agent]));
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::draw::draw(frame, &mut app))
        .unwrap();
    let bottom: String = (0..100)
        .map(|x| terminal.backend().buffer()[(x, 29)].symbol())
        .collect();
    assert!(bottom.contains("Inspect parser"));
    assert!(!bottom.contains("very long instruction"));
    app.handle_mouse(click(3, 29));
    tokio::task::yield_now().await;
    app.tick_subagent_recording();
    terminal
        .draw(|frame| crate::draw::draw(frame, &mut app))
        .unwrap();
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Complete child reply"));
    assert!(rendered.contains("A very long instruction"));
}
