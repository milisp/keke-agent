//! Slash commands, approval modes, and plan mode.

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use keke_acp::PermissionAnswer;
use keke_acp::PermissionId;
use keke_acp::Update;
use keke_config_types::ApprovalPolicy;
use keke_config_types::SessionMode;
use keke_protocol::ReasoningEffort;
use keke_protocol::ServiceTier;
use keke_protocol::StopReason;
use keke_protocol::ToolCall;
use keke_protocol::ToolCallId;

use crate::Cell;
use crate::Turn;
use crate::app::plan::PlanFocus;
use crate::app::plan::PlanRow;
use crate::tests::helpers::*;

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
async fn shift_tab_walks_one_ladder_through_plan_and_the_policies() {
    let (mut app, scripted, mut updates, _local) = app_with_commands(Vec::new(), Vec::new());
    assert_eq!(app.approval_policy(), ApprovalPolicy::OnRequest);
    assert_eq!(app.session_mode(), SessionMode::Default);

    app.handle_key(key(KeyCode::BackTab));
    assert_eq!(app.approval_policy(), ApprovalPolicy::OnFailure);
    // The gesture is silent: the status bar already says which rung is on, and
    // a line per tap would push the conversation off screen to repeat it.
    assert!(app.transcript.is_empty(), "{:?}", app.transcript.cells());

    app.handle_key(shift(KeyCode::Tab));
    assert_eq!(app.approval_policy(), ApprovalPolicy::Never);

    // The tightest rung: plan mode, with the policy back at on-request, since
    // a rung must mean one thing rather than one thing plus what was under it.
    app.handle_key(key(KeyCode::BackTab));
    drain(&mut app, &mut updates, 1).await;
    assert_eq!(app.session_mode(), SessionMode::Plan);
    assert_eq!(app.approval_policy(), ApprovalPolicy::OnRequest);

    app.handle_key(key(KeyCode::BackTab));
    drain(&mut app, &mut updates, 1).await;
    assert_eq!(app.session_mode(), SessionMode::Default);
    assert_eq!(app.approval_policy(), ApprovalPolicy::OnRequest);

    // The surface's idea of the rung is worthless unless the agent has it too.
    assert_eq!(
        scripted.policies(),
        vec![
            ApprovalPolicy::OnFailure,
            ApprovalPolicy::Never,
            ApprovalPolicy::OnRequest,
            ApprovalPolicy::OnRequest,
        ]
    );
    assert_eq!(
        scripted.modes(),
        vec![SessionMode::Plan, SessionMode::Default]
    );
}

/// The flag is drawn from what the seam last said, never from the fact that
/// this surface asked: the agent enters and leaves plan mode on its own.
#[tokio::test]
async fn the_status_bar_shows_plan_only_once_the_seam_says_so() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    assert!(!status_bar(&app).contains("plan"));

    app.request_session_mode(SessionMode::Plan);
    assert!(
        !status_bar(&app).contains("plan"),
        "asking for a mode is not being in it"
    );

    app.apply(Update::ModeChanged(SessionMode::Plan));
    assert!(status_bar(&app).contains("plan"));
}

/// Nobody on this surface touched a key: the agent entered plan mode itself.
#[tokio::test]
async fn a_mode_change_the_surface_did_not_ask_for_still_changes_the_bar() {
    let (mut app, scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::ModeChanged(SessionMode::Plan));
    assert!(status_bar(&app).contains("plan"));
    assert!(
        scripted.modes().is_empty(),
        "the surface asked for nothing here"
    );

    app.apply(Update::ModeChanged(SessionMode::Default));
    assert!(!status_bar(&app).contains("plan"));
}

/// `/plan` asks for the mode; with a description it sends the work too, since
/// "plan this" is one thought.
#[tokio::test]
async fn the_plan_command_asks_for_the_mode_and_can_carry_the_prompt() {
    let (mut app, scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());

    type_text(&mut app, "/plan");
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(scripted.modes(), vec![SessionMode::Plan]);
    assert!(app.transcript.is_empty());

    type_text(&mut app, "/plan add caching");
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(
        app.transcript.last(),
        Some(&Cell::User("add caching".to_string()))
    );
    tokio::task::yield_now().await;
    assert_eq!(scripted.prompts(), vec!["add caching".to_string()]);
}

/// The plan is in the scrollback, numbered, with the file it was saved to —
/// not in a window over it. Numbered because a comment says "line 7", so line
/// 7 has to be something a person can see and point at.
#[tokio::test]
async fn the_plan_is_drawn_into_the_transcript_with_its_lines_numbered() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.apply(exit_plan_mode("alpha\nbravo"));

    let drawn = rendered(&app);
    assert!(drawn.iter().any(|line| line.contains("   1 alpha")));
    assert!(drawn.iter().any(|line| line.contains("   2 bravo")));
}

/// A plan is a document a person reads, edits, and shows to somebody else, so
/// it outlives the process that received it — and the surface says where.
#[tokio::test]
async fn a_plan_is_saved_under_the_keke_home_and_its_path_is_shown() {
    let home = tempfile::tempdir().expect("a temp home");
    let (app, _scripted, _updates, _local) = app_with(Vec::new());
    let app = &mut app
        .with_config_home(keke_paths::AbsPath::new(home.path()).expect("an absolute temp path"));

    app.apply(exit_plan_mode("# Rewrite the parser\n\nstep one"));
    let path = app
        .transcript
        .last_plan()
        .expect("a plan in the scrollback")
        .path
        .clone()
        .expect("a saved plan");
    assert_eq!(path, home.path().join("plans/rewrite-the-parser.md"));
    assert!(
        std::fs::read_to_string(&path)
            .expect("the plan on disk")
            .contains("step one")
    );
}

/// In plan mode the policy governs nothing yet — no command runs until the
/// plan is approved — and the policy that will govern the work is chosen at
/// approval, so the bar does not answer a question nobody has been asked.
#[tokio::test]
async fn the_status_bar_drops_the_policy_while_planning() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    assert!(status_bar(&app).contains("on-request"));

    app.apply(Update::ModeChanged(SessionMode::Plan));
    assert!(!status_bar(&app).contains("on-request"));

    app.apply(Update::ModeChanged(SessionMode::Default));
    assert!(status_bar(&app).contains("on-request"));
}

/// Approving is the moment a person decides how much of the plan may happen
/// without them, so the panel under it asks while they read, and Enter
/// answers with the row they landed on rather than with the policy
/// underneath plan mode.
#[tokio::test]
async fn approving_a_plan_carries_it_out_under_the_row_chosen_on_the_panel() {
    let (mut app, scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::ModeChanged(SessionMode::Plan));
    app.apply(exit_plan_mode("do the thing"));
    assert_eq!(
        app.plan_review().expect("a plan").row(),
        PlanRow::ManualApprove,
        "manually approving edits is the default row"
    );

    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Down));
    assert_eq!(app.plan_review().expect("a plan").row(), PlanRow::AutoMode);
    assert!(scripted.answers().is_empty(), "picking is not answering");

    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.approval_policy(), ApprovalPolicy::Never);
    assert_eq!(
        scripted.answers(),
        vec![(
            PermissionId("plan-1".to_string()),
            PermissionAnswer::Allow,
            None
        )]
    );
}

#[tokio::test]
async fn exit_plan_mode_opens_the_plan_for_review() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.apply(exit_plan_mode("## Step one\n\nread the parser"));

    let review = app.plan_review().expect("a plan to review");
    assert!(review.text().contains("read the parser"));
    assert!(!review.is_empty());
    assert_eq!(app.turn(), Turn::AwaitingPermission);
}

#[tokio::test]
async fn approving_the_plan_allows_the_call_and_telling_keke_denies_it() {
    let (mut app, scripted, _updates, _local) = app_with(Vec::new());

    app.apply(exit_plan_mode("do the thing"));
    app.handle_key(key(KeyCode::Enter));
    assert!(app.plan_review().is_none());
    assert_eq!(
        scripted.answers(),
        vec![(
            PermissionId("plan-1".to_string()),
            PermissionAnswer::Allow,
            None
        )]
    );
    // Approving is the person's answer to "may I leave plan mode" — the same
    // request quitting makes — so it asks for the mode change immediately
    // rather than waiting on the agent to echo it back.
    assert_eq!(scripted.modes(), vec![SessionMode::Default]);

    app.apply(exit_plan_mode("do the other thing"));
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(
        app.plan_focus(),
        PlanFocus::Composer,
        "the row that sends the plan back opens the composer"
    );
    type_text(&mut app, "narrower, please");
    app.handle_key(key(KeyCode::Enter));
    assert!(app.plan_review().is_none());
    assert_eq!(
        scripted.answers().last(),
        Some(&(
            PermissionId("plan-1".to_string()),
            PermissionAnswer::Deny,
            Some("narrower, please".to_string())
        ))
    );
}

/// Quitting the plan is not requesting changes: it asks to leave the mode.
#[tokio::test]
async fn quitting_the_plan_denies_it_and_asks_to_leave_plan_mode() {
    let (mut app, scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::ModeChanged(SessionMode::Plan));
    app.apply(exit_plan_mode("do the thing"));

    app.handle_key(key(KeyCode::Esc));
    assert!(app.plan_review().is_none());
    assert_eq!(
        scripted.answers(),
        vec![(
            PermissionId("plan-1".to_string()),
            PermissionAnswer::Deny,
            None
        )]
    );
    assert_eq!(scripted.modes(), vec![SessionMode::Default]);
}

/// An agent that left plan mode without writing anything is not an error: the
/// same surface opens, and every action on it still works.
#[tokio::test]
async fn an_empty_plan_still_opens_a_reviewable_surface() {
    let (mut app, scripted, _updates, _local) = app_with(Vec::new());
    app.apply(Update::PermissionRequested {
        id: PermissionId("plan-1".to_string()),
        call: ToolCall {
            id: ToolCallId::new("t1"),
            name: "exit_plan_mode".to_string(),
            arguments: serde_json::json!({}),
        },
        reason: String::new(),
    });

    let review = app.plan_review().expect("a surface even with no plan");
    assert!(review.is_empty());

    app.handle_key(key(KeyCode::Char('y')));
    assert_eq!(app.take_pending_copy(), None, "there is nothing to copy");

    app.handle_key(key(KeyCode::Enter));
    assert_eq!(
        scripted.answers(),
        vec![(
            PermissionId("plan-1".to_string()),
            PermissionAnswer::Allow,
            None
        )]
    );
}

#[tokio::test]
async fn the_plan_review_takes_the_keyboard_from_the_composer() {
    let home = tempfile::tempdir().expect("a temporary directory");
    let home = keke_paths::AbsPath::new(home.path()).expect("an absolute home");
    let (app, _scripted, _updates, _local) = app_with(Vec::new());
    let mut app = app.with_config_home(home);
    app.apply(exit_plan_mode("line one"));

    type_text(&mut app, "hello");
    assert!(
        app.input.is_empty(),
        "a keystroke must answer the prompt, not vanish into a box nobody is looking at"
    );

    app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
    assert!(
        app.take_pending_edit().is_some(),
        "a keystroke on the preview must fire its shortcut, not fall through"
    );
}

/// Selecting "tell Keke what to change" hands the keyboard to the composer,
/// and Esc hands it back without answering the plan.
#[tokio::test]
async fn telling_keke_what_to_change_hands_focus_to_the_composer_and_escape_returns_it() {
    let (mut app, _scripted, _updates, _local) = app_with(Vec::new());
    app.apply(exit_plan_mode("alpha"));
    assert_eq!(app.plan_focus(), PlanFocus::Preview);

    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.plan_focus(), PlanFocus::Composer);

    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.plan_focus(), PlanFocus::Preview);
    assert!(
        app.plan_review().is_some(),
        "escape left the composer, not the plan"
    );
}

/// The preview's single letters are shortcuts; in the composer they are words.
#[tokio::test]
async fn typing_in_the_composer_does_not_fire_the_previews_actions() {
    let (mut app, scripted, _updates, _local) = app_with(Vec::new());
    app.apply(exit_plan_mode("alpha"));

    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));
    type_text(&mut app, "say more here");

    assert_eq!(app.input.text(), "say more here");
    assert!(app.plan_review().is_some(), "nothing answered the plan");
    assert!(scripted.answers().is_empty());
}

/// `/view-plan` brings the last plan back after it was answered.
#[tokio::test]
async fn view_plan_reopens_the_last_plan_as_a_record() {
    let (mut app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    app.apply(exit_plan_mode("alpha\nbravo"));
    app.handle_key(key(KeyCode::Enter));
    assert!(app.plan_review().is_none());

    let plan = app.transcript.last_plan().expect("the plan is still there");
    assert!(plan.text.contains("bravo"));
    assert_eq!(plan.answer, Some(PermissionAnswer::Allow));
}

/// A plan stays in the scrollback after it is answered, so `/view-plan` is a
/// scroll rather than a resurrection — and nothing there can be answered again.
#[tokio::test]
async fn an_answered_plan_stays_in_the_scrollback_and_takes_no_more_answers() {
    let (mut app, scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    app.apply(exit_plan_mode("alpha"));
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));
    app.handle_key(key(KeyCode::Enter));
    let answered = scripted.answers();
    assert!(app.plan_review().is_none(), "the review is over");

    type_text(&mut app, "/show-plan");
    app.handle_key(key(KeyCode::Enter));
    app.handle_key(key(KeyCode::Char('a')));
    assert_eq!(scripted.answers(), answered, "nothing new was answered");

    let plan = app.transcript.last_plan().expect("the plan is still there");
    assert_eq!(plan.text, "alpha");
    assert_eq!(plan.answer, Some(PermissionAnswer::Deny));
    assert!(scripted.modes().is_empty());
}

/// Nothing to show is said the way every other nothing-to-do is said.
#[tokio::test]
async fn view_plan_with_no_plan_flashes_rather_than_opening_an_empty_overlay() {
    let (mut app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());

    type_text(&mut app, "/plan-view");
    app.handle_key(key(KeyCode::Enter));
    assert!(app.plan_review().is_none());
    assert!(app.flash().is_some_and(|text| text.contains("no plan")));
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

/// A bare `/fast` is a switch, not a cycle: on, then off again, and the agent
/// hears both — a surface that only changed what it drew would leave the
/// allowance being spent at a speed nobody could turn off.
#[tokio::test]
async fn the_fast_command_toggles_and_reaches_the_agent() {
    let (mut app, scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());

    type_text(&mut app, "/fast");
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.service_tier(), Some(ServiceTier::Fast));
    assert!(app.transcript.is_empty(), "a clean switch says nothing");

    type_text(&mut app, "/fast");
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(
        app.service_tier(),
        None,
        "off is reachable by tapping again"
    );

    assert_eq!(
        scripted.service_tiers(),
        vec![Some(ServiceTier::Fast), None],
        "the surface's idea of the queue is worthless unless the agent has it"
    );
}

/// A queue can be named outright, and a typo must move nothing — the same
/// standard `/effort` is held to, for the same reason: a misspelling that
/// quietly bought a different speed is invisible until the bill.
#[tokio::test]
async fn the_fast_command_names_a_queue_and_refuses_a_typo() {
    let (mut app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());

    type_text(&mut app, "/fast flex");
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.service_tier(), Some(ServiceTier::Flex));

    type_text(&mut app, "/fast fsat");
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(
        app.service_tier(),
        Some(ServiceTier::Flex),
        "a typo must not move the queue"
    );
    assert!(matches!(app.transcript.last(), Some(Cell::Error(_))));

    type_text(&mut app, "/fast off");
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.service_tier(), None);
}

/// `/new` is the name people reach for; it does what `/clear` does.
#[tokio::test]
async fn new_reaches_the_agent_and_resets_usage_too() {
    let (mut app, scripted, mut updates, _local) = app_with_commands(Vec::new(), Vec::new());
    app.apply(Update::TextDelta("some output".to_string()));
    app.apply(Update::TokensUsed(keke_protocol::Usage {
        input_tokens: 100,
        ..keke_protocol::Usage::default()
    }));
    assert!(!app.transcript.is_empty());
    assert!(app.usage().total() > 0);

    type_text(&mut app, "/new");
    app.handle_key(key(KeyCode::Enter));
    drain(&mut app, &mut updates, 1).await;

    assert_eq!(
        scripted.new_session_count(),
        1,
        "unlike /clear, /new must tell the agent to forget too"
    );
    assert!(app.transcript.is_empty(), "{:?}", app.transcript.cells());
    assert_eq!(app.usage().total(), 0);
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
