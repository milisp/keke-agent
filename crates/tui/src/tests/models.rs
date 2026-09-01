//! `/model` and `/provider` switching.

use std::sync::Arc;

use crossterm::event::KeyCode;
use keke_acp::ScriptedConversation;
use keke_acp::Update;
use keke_protocol::ReasoningEffort;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::App;
use crate::Cell;
use crate::tests::helpers::*;

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
            "test-provider",
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
    let mut app = app.with_models("test-provider", "some-model", Vec::new());

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

/// An app whose host told it which provider instances are registered.
fn app_with_providers() -> (
    App,
    Arc<ScriptedConversation>,
    UnboundedReceiver<Update>,
    UnboundedReceiver<Update>,
) {
    let (app, scripted, updates, local) = app_with_models();
    (
        app.with_provider_routes(vec![
            crate::ProviderChoice {
                route: "test-provider".to_string(),
                display_name: "Test Provider".to_string(),
            },
            crate::ProviderChoice {
                route: "xai".to_string(),
                display_name: "xAI (API key)".to_string(),
            },
        ]),
        scripted,
        updates,
        local,
    )
}

/// The same question `/model` answers, about a different list: bare
/// `/provider` is somebody asking what there is, so it opens on the route in
/// force rather than printing a paragraph that scrolls away.
#[tokio::test]
async fn the_provider_command_opens_a_picker_over_the_registered_routes() {
    let (mut app, _scripted, _updates, _local) = app_with_providers();

    type_text(&mut app, "/provider");
    app.handle_key(key(KeyCode::Enter));

    assert!(app.provider_picker().is_some());
    assert!(app.model_picker().is_none(), "the two lists are not one");
    let routes: Vec<&str> = app
        .picker_providers()
        .iter()
        .map(|route| route.route.as_str())
        .collect();
    assert_eq!(routes, vec!["test-provider", "xai"]);
    assert_eq!(app.picker_selected(), 0);
    assert!(app.transcript.is_empty());
}

/// A name typed in full is an instruction, not a question: nothing opens.
#[tokio::test]
async fn a_named_provider_switches_without_opening_anything() {
    let (mut app, _scripted, _updates, _local) = app_with_providers();

    type_text(&mut app, "/provider xai");
    app.handle_key(key(KeyCode::Enter));

    assert!(!app.picker_open());
    assert_eq!(app.provider(), Some("xai"));
}

/// A model id belongs to the provider that serves it, so one carried across is
/// a pair no run ever used.
#[tokio::test]
async fn switching_provider_unsets_the_model_the_old_one_served() {
    let (mut app, _scripted, _updates, _local) = app_with_providers();

    type_text(&mut app, "/provider xai");
    app.handle_key(key(KeyCode::Enter));

    assert_eq!(app.model(), "");
    // And the old route's list goes with it: keeping it would have `/model`
    // refuse names the new route does serve.
    assert!(app.models().is_empty());
}

/// Invariant 8: a route nothing is registered under is refused by name, here,
/// where the person can still see what they typed.
#[tokio::test]
async fn a_provider_no_route_is_registered_for_is_refused() {
    let (mut app, _scripted, _updates, _local) = app_with_providers();

    type_text(&mut app, "/provider xaii");
    app.handle_key(key(KeyCode::Enter));

    assert_eq!(
        app.provider(),
        Some("test-provider"),
        "a refusal must not move the provider"
    );
    assert_eq!(app.model(), "gpt-5.6-sol", "nor unset the model");
    assert!(matches!(app.transcript.last(), Some(Cell::Error(text)) if text.contains("xaii")));
}

/// The choice outlives the process, through the one persistence path `/model`
/// and `/effort` already use.
#[tokio::test]
async fn the_chosen_provider_is_written_to_the_user_config() {
    let home = tempfile::tempdir().expect("a temporary directory");
    let home = keke_paths::AbsPath::new(home.path()).expect("an absolute home");
    let (app, _scripted, _updates, _local) = app_with_providers();
    let mut app = app.with_config_home(home.clone());

    type_text(&mut app, "/provider xai");
    app.handle_key(key(KeyCode::Enter));

    let written = std::fs::read_to_string(home.as_path().join("config.toml"))
        .expect("the switch was written");
    assert!(written.contains("provider = \"xai\""), "{written}");
    // The model is not written back under a route that may not serve it.
    assert!(!written.contains("model = "), "{written}");
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

/// After `/provider` moves ahead of the running conversation, `/model` must
/// still land in config.toml paired with the new route — but must not hand
/// the old, still-live conversation a model id that belongs to a provider it
/// was never built for.
#[tokio::test]
async fn model_after_a_pending_provider_switch_is_written_but_not_sent_live() {
    let home = tempfile::tempdir().expect("a temporary directory");
    let home = keke_paths::AbsPath::new(home.path()).expect("an absolute home");
    let (app, scripted, _updates, _local) = app_with_providers();
    let mut app = app.with_config_home(home.clone());

    type_text(&mut app, "/provider xai");
    app.handle_key(key(KeyCode::Enter));

    type_text(&mut app, "/model grok-4.6");
    app.handle_key(key(KeyCode::Enter));

    let written = std::fs::read_to_string(home.as_path().join("config.toml"))
        .expect("the switch was written");
    assert!(written.contains("provider = \"xai\""), "{written}");
    assert!(written.contains("model = \"grok-4.6\""), "{written}");
    assert_eq!(app.model(), "grok-4.6");
    assert!(
        scripted.models().is_empty(),
        "the still-live conversation is on the old provider and must not be told about a model that belongs to the new one"
    );
}
