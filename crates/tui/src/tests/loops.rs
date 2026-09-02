//! `/loop`: a prompt that keeps being sent.

use crossterm::event::KeyCode;

use crate::Cell;
use crate::tests::helpers::*;

#[tokio::test]
async fn a_loop_sends_its_prompt_at_once_and_stays_registered() {
    let (mut app, scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    type_text(&mut app, "/loop 5m run the tests");
    app.handle_key(key(KeyCode::Enter));
    tokio::task::yield_now().await;

    assert_eq!(scripted.prompts(), vec!["run the tests".to_string()]);
    assert_eq!(app.schedule.tasks().len(), 1);
}

#[tokio::test]
async fn a_bad_interval_starts_nothing() {
    let (mut app, scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    type_text(&mut app, "/loop soon run the tests");
    app.handle_key(key(KeyCode::Enter));

    assert!(scripted.prompts().is_empty());
    assert!(app.schedule.is_empty());
    assert!(matches!(app.transcript.last(), Some(Cell::Error(text)) if text.contains("60s")));
}

#[tokio::test]
async fn a_loop_is_stopped_by_the_id_it_was_given() {
    let (mut app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    type_text(&mut app, "/loop 5m run the tests");
    app.handle_key(key(KeyCode::Enter));
    let id = app.schedule.tasks()[0].id;

    type_text(&mut app, &format!("/loop stop {id}"));
    app.handle_key(key(KeyCode::Enter));
    assert!(app.schedule.is_empty());
}

#[tokio::test]
async fn listing_names_the_loops_and_how_to_stop_them() {
    let (mut app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    type_text(&mut app, "/loop 5m run the tests");
    app.handle_key(key(KeyCode::Enter));

    type_text(&mut app, "/loop list");
    app.handle_key(key(KeyCode::Enter));
    let Some(Cell::Notice(text)) = app.transcript.last() else {
        panic!("expected a notice");
    };
    assert!(text.contains("run the tests"), "{text}");
    assert!(text.contains("every 5m"), "{text}");
    assert!(text.contains("/loop stop"), "{text}");
}

/// A loop is a standing instruction about this conversation, so retiring the
/// conversation retires it too.
#[tokio::test]
async fn a_new_session_stops_the_loops() {
    let (mut app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    type_text(&mut app, "/loop 5m run the tests");
    app.handle_key(key(KeyCode::Enter));

    type_text(&mut app, "/new");
    app.handle_key(key(KeyCode::Enter));
    assert!(app.schedule.is_empty());
}
