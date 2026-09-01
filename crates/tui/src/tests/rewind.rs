//! Double-tap escape to rewind, snapshotting the working tree per turn.

use crossterm::event::KeyCode;
use keke_acp::Update;
use keke_protocol::RewindScope;
use keke_protocol::StopReason;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::App;
use crate::Cell;
use crate::tests::helpers::*;

/// Say two things, and let the scripted agent answer each.
async fn two_turns(app: &mut App, updates: &mut UnboundedReceiver<Update>) {
    for text in ["first", "second"] {
        type_text(app, text);
        app.handle_key(key(KeyCode::Enter));
        drain(app, updates, 3).await;
    }
}

fn two_answers() -> Vec<Vec<Update>> {
    vec![
        vec![
            Update::TurnStarted,
            Update::TextDelta("one".to_string()),
            Update::TurnEnded(StopReason::EndTurn),
        ],
        vec![
            Update::TurnStarted,
            Update::TextDelta("two".to_string()),
            Update::TurnEnded(StopReason::EndTurn),
        ],
    ]
}

/// Let whatever the surface asked the agent come back and be applied.
///
/// The overlay is filled in over the seam — what can be gone back to, and what
/// a restore would touch — so a test that did not pump this would be asserting
/// on a half-open overlay no person would ever see.
async fn settle(app: &mut App, local: &mut UnboundedReceiver<Update>) {
    for _ in 0..8 {
        tokio::task::yield_now().await;
        while let Ok(update) = local.try_recv() {
            app.apply(update);
        }
    }
}

/// Esc Esc, and let the list of what was said arrive.
async fn open_rewind(app: &mut App, local: &mut UnboundedReceiver<Update>) {
    app.handle_key(key(KeyCode::Esc));
    app.handle_key(key(KeyCode::Esc));
    settle(app, local).await;
}

#[tokio::test]
async fn one_escape_does_not_open_the_rewind() {
    let (mut app, _scripted, mut updates, _local) = app_with(two_answers());
    two_turns(&mut app, &mut updates).await;

    app.handle_key(key(KeyCode::Esc));

    assert!(
        app.rewind().is_none(),
        "esc is the key for 'never mind'; one press must not wind anything back"
    );
}

#[tokio::test]
async fn two_escapes_offer_everything_that_was_said() {
    let (mut app, _scripted, mut updates, mut local) = app_with(two_answers());
    two_turns(&mut app, &mut updates).await;

    open_rewind(&mut app, &mut local).await;

    let rewind = app.rewind().expect("the overlay opens on the second esc");
    let offered: Vec<&str> = rewind
        .points()
        .iter()
        .map(|point| point.text.as_str())
        .collect();
    assert_eq!(
        offered,
        vec!["second", "first"],
        "newest first: the thing just said is what people go back to"
    );
}

#[tokio::test]
async fn a_key_between_the_two_escapes_ends_the_gesture() {
    let (mut app, _scripted, mut updates, _local) = app_with(two_answers());
    two_turns(&mut app, &mut updates).await;

    app.handle_key(key(KeyCode::Esc));
    app.handle_key(key(KeyCode::Char('a')));
    app.handle_key(key(KeyCode::Esc));

    assert!(
        app.rewind().is_none(),
        "two escs are one gesture only when nothing was said in between"
    );
    assert_eq!(app.input.text(), "a");
}

#[tokio::test]
async fn escape_winds_nothing_back_before_anything_was_said() {
    let (mut app, _scripted, _updates, _local) = app_with(vec![]);

    app.handle_key(key(KeyCode::Esc));
    app.handle_key(key(KeyCode::Esc));

    assert!(app.rewind().is_none(), "there is nothing to go back to");
}

#[tokio::test]
async fn a_busy_turn_still_takes_escape_as_an_interrupt() {
    let (mut app, scripted, _updates, _local) = app_with(vec![vec![Update::TurnStarted]]);
    type_text(&mut app, "first");
    app.handle_key(key(KeyCode::Enter));
    app.apply(Update::TurnStarted);

    app.handle_key(key(KeyCode::Esc));

    assert_eq!(scripted.cancel_count(), 1);
    assert!(app.rewind().is_none());
}

/// Choosing a prompt is not choosing what to put back: the second question is
/// asked out loud, because keke cannot infer which of the two a person means.
#[tokio::test]
async fn choosing_a_prompt_asks_what_to_put_back() {
    let (mut app, scripted, mut updates, mut local) = app_with(two_answers());
    two_turns(&mut app, &mut updates).await;

    open_rewind(&mut app, &mut local).await;
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app, &mut local).await;

    let rewind = app.rewind().expect("the overlay is still open");
    let offered: Vec<&str> = rewind.choices().iter().map(|choice| choice.label).collect();
    assert_eq!(
        offered,
        vec!["conversation only", "files only", "conversation and files"],
        "all three are always shown, so a missing one never reads as a missing feature"
    );
    assert!(
        scripted.rewinds().is_empty(),
        "choosing a prompt must not wind anything back on its own"
    );
}

/// A scripted agent holds no snapshots, so the two file choices say why they
/// would do nothing rather than being quietly dropped from the list.
#[tokio::test]
async fn a_turn_with_no_snapshot_says_so_instead_of_hiding_the_choice() {
    let (mut app, _scripted, mut updates, mut local) = app_with(two_answers());
    two_turns(&mut app, &mut updates).await;

    open_rewind(&mut app, &mut local).await;
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app, &mut local).await;

    let rewind = app.rewind().expect("the overlay is still open");
    let choices = rewind.choices();
    assert!(
        choices[0].unavailable.is_none(),
        "the words are always there"
    );
    assert!(
        choices[1].unavailable.is_some(),
        "files only: nothing to put back"
    );
    assert!(choices[2].unavailable.is_some());
    assert_eq!(
        rewind.selected(),
        0,
        "the highlight starts on a choice that would actually do something"
    );
    assert!(
        rewind.decision().is_some(),
        "enter on an available choice must have something to carry out"
    );
}

#[tokio::test]
async fn rewinding_hands_the_prompt_back_and_drops_what_it_led_to() {
    let (mut app, scripted, mut updates, mut local) = app_with(two_answers());
    two_turns(&mut app, &mut updates).await;

    open_rewind(&mut app, &mut local).await;
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app, &mut local).await;
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app, &mut local).await;

    assert_eq!(
        app.input.text(),
        "second",
        "the words come back to be edited"
    );
    assert_eq!(
        app.transcript.cells(),
        &[
            Cell::User("first".to_string()),
            Cell::Assistant("one".to_string()),
        ],
        "the turn the prompt started goes with it"
    );
    assert!(app.rewind().is_none());
    assert_eq!(
        scripted.rewinds(),
        vec![(1, RewindScope::Conversation)],
        "the agent forgets too, or the next answer is given against a withdrawn message"
    );
    assert_eq!(
        scripted.prompts(),
        vec!["first".to_string()],
        "what the agent still holds is what is left on screen"
    );
}

#[tokio::test]
async fn rewinding_further_back_drops_everything_after_it() {
    let (mut app, scripted, mut updates, mut local) = app_with(two_answers());
    two_turns(&mut app, &mut updates).await;

    open_rewind(&mut app, &mut local).await;
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app, &mut local).await;
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app, &mut local).await;

    assert_eq!(app.input.text(), "first");
    assert!(
        app.transcript.cells().is_empty(),
        "winding back to the first prompt leaves the conversation empty"
    );
    assert_eq!(scripted.rewinds(), vec![(0, RewindScope::Conversation)]);
    assert!(scripted.prompts().is_empty());
}

/// Esc steps back out of the second question rather than throwing away the
/// answer to the first.
#[tokio::test]
async fn escape_backs_out_of_the_confirm_step_before_closing() {
    let (mut app, scripted, mut updates, mut local) = app_with(two_answers());
    two_turns(&mut app, &mut updates).await;

    open_rewind(&mut app, &mut local).await;
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app, &mut local).await;
    app.handle_key(key(KeyCode::Esc));

    assert!(
        matches!(
            app.rewind().map(crate::rewind::Rewind::phase),
            Some(crate::rewind::Phase::Picking { .. })
        ),
        "the first esc goes back to the list"
    );
    app.handle_key(key(KeyCode::Esc));
    assert!(app.rewind().is_none(), "the second closes the overlay");
    assert!(scripted.rewinds().is_empty());
}

#[tokio::test]
async fn cancelling_the_overlay_leaves_the_conversation_alone() {
    let (mut app, scripted, mut updates, mut local) = app_with(two_answers());
    two_turns(&mut app, &mut updates).await;
    let before = app.transcript.cells().to_vec();

    open_rewind(&mut app, &mut local).await;
    app.handle_key(key(KeyCode::Esc));
    settle(&mut app, &mut local).await;

    assert!(app.rewind().is_none());
    assert_eq!(app.transcript.cells(), before.as_slice());
    assert!(scripted.rewinds().is_empty());
    assert!(app.input.is_empty());
}

/// Putting the files back is still a rewind of the words: the prompt comes
/// back to the composer to be edited, and is not asked again on its own.
#[tokio::test]
async fn restoring_both_hands_the_prompt_back_rather_than_asking_it_again() {
    let (mut app, scripted, mut updates, mut local) = app_with(two_answers());
    scripted.with_snapshots(vec!["src/lib.rs".to_string()]);
    two_turns(&mut app, &mut updates).await;

    open_rewind(&mut app, &mut local).await;
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app, &mut local).await;
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Down));
    let rewind = app.rewind().expect("the overlay is still open");
    assert_eq!(
        rewind.decision().map(|(_, scope)| scope),
        Some(RewindScope::Both),
        "two steps down from the first choice is conversation and files"
    );
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app, &mut local).await;

    assert_eq!(
        app.input.text(),
        "second",
        "the words come back to be edited"
    );
    assert_eq!(
        scripted.rewinds(),
        vec![(1, RewindScope::Both)],
        "the files go back with the conversation"
    );
    assert_eq!(
        scripted.prompts(),
        vec!["first".to_string()],
        "the prompt is handed back, never re-sent"
    );
    assert_eq!(
        app.transcript.cells(),
        &[
            Cell::User("first".to_string()),
            Cell::Assistant("one".to_string()),
        ],
    );
}

/// The Enter that carries out a rewind is not also the Enter that sends what
/// it handed back — a terminal repeating a held key would otherwise ask the
/// very question the person was taking back.
#[tokio::test]
async fn the_enter_that_rewinds_does_not_send_the_prompt_it_hands_back() {
    let (mut app, scripted, mut updates, mut local) = app_with(two_answers());
    two_turns(&mut app, &mut updates).await;

    open_rewind(&mut app, &mut local).await;
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app, &mut local).await;
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app, &mut local).await;
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app, &mut local).await;

    assert_eq!(
        app.input.text(),
        "second",
        "the words stay in the composer to be edited"
    );
    assert_eq!(
        scripted.prompts(),
        vec!["first".to_string()],
        "nothing was asked again"
    );
}
