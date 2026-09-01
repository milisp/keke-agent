//! Status bar, context figure, and prompt history/readline behavior.

use std::sync::Arc;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use keke_acp::ScriptedConversation;
use keke_acp::Update;
use keke_protocol::ContentBlock;
use keke_protocol::ToolResult;
use keke_protocol::ToolStatus;

use crate::App;
use crate::CallState;
use crate::Cell;
use crate::tests::helpers::*;

/// Each request resends the whole conversation, so a step's input tokens are
/// the context size, not an increment: the context figure must track the most
/// recent step, while the cumulative total keeps adding.
#[test]
fn the_context_figure_is_the_latest_step_not_a_running_total() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::TokensUsed(keke_protocol::Usage {
        input_tokens: 100,
        output_tokens: 20,
        ..keke_protocol::Usage::default()
    }));
    app.apply(Update::TokensUsed(keke_protocol::Usage {
        input_tokens: 5,
        ..keke_protocol::Usage::default()
    }));
    assert_eq!(app.context_input(), 5);
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
    let app = App::new(Arc::new(scripted) as Arc<_>).0.with_history(
        &history,
        keke_protocol::Usage::default(),
        0,
    );

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
