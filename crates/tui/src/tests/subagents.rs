//! Subagent popup and status rows.

use crossterm::event::KeyCode;
use keke_acp::Update;

use crate::tests::helpers::*;

fn view(id: &str, status: Option<&str>, input_tokens: u64) -> keke_acp::SubagentView {
    keke_acp::SubagentView {
        id: id.to_string(),
        task: format!("find {id}\nin the parser"),
        status: status.map(str::to_string),
        input_tokens,
    }
}

/// The rows are a snapshot of what is running, not a log of what ran: an agent
/// the engine stopped sending is one whose result is in the transcript, and a
/// status line that kept it would be showing something that is no longer true.
#[test]
fn a_collected_subagent_leaves_the_status_rows() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());

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

/// A row is one line and can only show the first line of the task. Clicking it
/// is how the rest is read, so the click has to be answered somewhere — and by
/// the row that was actually drawn there.
#[test]
fn clicking_a_subagent_row_opens_the_task_it_was_given() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::Subagents(vec![
        view("agent_1", None, 120),
        view("agent_2", Some("completed"), 300),
    ]));
    app.set_subagent_rows(vec![(7, "agent_1".to_string()), (8, "agent_2".to_string())]);

    assert!(!app.open_subagent_at(9), "no row was drawn there");
    assert!(app.open_subagent().is_none());

    assert!(app.open_subagent_at(8));
    assert_eq!(app.open_subagent().expect("open").id, "agent_2");

    // The same row again closes it: the row is the only handle on the popup,
    // so it has to work both ways.
    assert!(app.open_subagent_at(8));
    assert!(app.open_subagent().is_none());
}

/// Escape reaches the popup before it reaches the turn. Subagents only exist
/// while a turn is running, so an escape that always interrupted would leave
/// the popup with no way to close.
#[test]
fn escape_closes_the_subagent_popup_before_it_interrupts_the_turn() {
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
        "the turn must survive closing a popup"
    );

    app.handle_key(key(KeyCode::Esc));
    assert_eq!(scripted.cancel_count(), 1);
}

/// A subagent that finished but has not been collected is still on screen, and
/// must not be drawn as though it were still working.
#[test]
fn a_finished_subagent_keeps_its_outcome_until_it_is_collected() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
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
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::Subagents(vec![view("agent_1", None, 120)]));
    app.apply(Update::SessionReset);
    assert!(app.subagents().is_empty());
}

// --- /mcp -------------------------------------------------------------------
