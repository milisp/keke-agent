//! `/mcp` overlay.

use crossterm::event::KeyCode;

use crate::Cell;
use crate::tests::helpers::*;

fn server(name: &str, remote: bool, signed_in: bool) -> crate::McpServerStatus {
    crate::McpServerStatus {
        name: name.to_string(),
        plugin: "local".to_string(),
        transport: if remote {
            "http https://mcp.test".to_string()
        } else {
            "node server.js".to_string()
        },
        remote,
        signed_in,
        allowed: true,
        enabled: true,
    }
}

#[tokio::test]
async fn mcp_opens_an_overlay_over_what_is_configured() {
    let (app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    let mut app = app.with_mcp(
        vec![server("files", false, false), server("vercel", true, false)],
        None,
        None,
    );

    type_text(&mut app, "/mcp");
    app.handle_key(key(KeyCode::Enter));

    assert!(app.mcp_picker().is_some());
    let names: Vec<&str> = app
        .picker_mcp()
        .into_iter()
        .map(|server| server.name.as_str())
        .collect();
    assert_eq!(names, ["files", "vercel"]);
    // The row a person came here for is the one that needs a token, not the
    // first one in the list.
    assert_eq!(app.picker_selected(), 1);
}

/// Nothing configured is a command to run, not a box with nothing in it.
#[tokio::test]
async fn mcp_with_nothing_configured_says_so_instead_of_opening() {
    let (app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    let mut app = app.with_mcp(Vec::new(), None, None);

    type_text(&mut app, "/mcp");
    app.handle_key(key(KeyCode::Enter));

    assert!(!app.picker_open());
    let Some(Cell::Notice(text)) = app.transcript.last() else {
        panic!("expected a notice, got {:?}", app.transcript.last());
    };
    assert!(text.contains("keke mcp add"), "{text}");
}

/// Enter on a row is the same login `/mcp login <name>` runs, so it reaches the
/// same refusal when the surface cannot sign in at all — but it says so on the
/// row rather than in the conversation, and leaves the overlay up.
#[tokio::test]
async fn enter_on_a_row_reports_on_the_row_and_keeps_the_overlay_open() {
    let (app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    let mut app = app.with_mcp(vec![server("vercel", true, false)], None, None);

    type_text(&mut app, "/mcp");
    app.handle_key(key(KeyCode::Enter));
    let before = app.transcript.len();
    app.handle_key(key(KeyCode::Enter));

    assert!(app.mcp_picker().is_some(), "the overlay stays open");
    assert_eq!(
        app.transcript.len(),
        before,
        "nothing reaches the transcript"
    );
    let activity = app
        .mcp_activity("vercel")
        .expect("the row says what happened");
    assert!(activity.contains("keke mcp login vercel"), "{activity}");
}

/// Progress from a login started in the overlay belongs on its row. The
/// transcript is the conversation, and a URL to click is not part of it.
#[tokio::test]
async fn login_progress_lands_on_the_row_while_the_overlay_is_open() {
    let (app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    let mut app = app.with_mcp(vec![server("vercel", true, false)], None, None);
    app.open_mcp_picker();
    let before = app.transcript.len();

    app.apply_notice(crate::login::Notice::McpProgress {
        name: "vercel".to_string(),
        message: "sign in at https://auth.example/authorize".to_string(),
    });

    assert_eq!(app.transcript.len(), before);
    let activity = app
        .mcp_activity("vercel")
        .expect("the row says what happened");
    assert!(
        activity.contains("https://auth.example/authorize"),
        "{activity}"
    );
}

/// With no overlay open there is nowhere else for it to go, so it goes where
/// everything else does rather than nowhere.
#[tokio::test]
async fn login_progress_with_no_overlay_open_still_reaches_the_transcript() {
    let (app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    let mut app = app.with_mcp(vec![server("vercel", true, false)], None, None);

    app.apply_notice(crate::login::Notice::McpProgress {
        name: "vercel".to_string(),
        message: "sign in at https://auth.example/authorize".to_string(),
    });

    let Some(Cell::Notice(text)) = app.transcript.last() else {
        panic!("expected a notice, got {:?}", app.transcript.last());
    };
    assert!(text.contains("https://auth.example/authorize"), "{text}");
}

/// Trust comes before a token: a server keke will not reach at all must not
/// send anyone off to authenticate with it.
#[tokio::test]
async fn an_untrusted_server_reports_the_trust_problem_not_the_token_one() {
    let (app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    let mut held = server("shipped", true, false);
    held.allowed = false;
    let mut app = app.with_mcp(vec![held], None, None);

    type_text(&mut app, "/mcp login shipped");
    app.handle_key(key(KeyCode::Enter));

    let Some(Cell::Error(text)) = app.transcript.last() else {
        panic!("expected an error, got {:?}", app.transcript.last());
    };
    assert!(text.contains("keke plugin trust local"), "{text}");
}

/// A list that still says "not signed in" after a successful login is a list
/// that sends the person round the loop again.
#[tokio::test]
async fn a_finished_login_stops_the_list_asking_for_one() {
    let (app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    let mut app = app.with_mcp(vec![server("vercel", true, false)], None, None);

    app.apply_notice(crate::login::Notice::McpProgress {
        name: "vercel".to_string(),
        message: "authorizing...".to_string(),
    });
    app.apply_notice(crate::login::Notice::SignedIn("vercel".to_string()));

    app.open_mcp_picker();
    assert_eq!(
        app.mcp_activity("vercel"),
        None,
        "a finished login stops being in flight"
    );
    assert!(
        app.picker_mcp()[0].signed_in,
        "the stored token must show in the list"
    );
}

/// A surface with no way to sign in says so, rather than appearing to start a
/// flow that will never finish.
#[tokio::test]
async fn signing_in_without_a_credential_store_says_where_to_do_it() {
    let (app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    let mut app = app.with_mcp(vec![server("vercel", true, false)], None, None);

    type_text(&mut app, "/mcp login vercel");
    app.handle_key(key(KeyCode::Enter));

    let Some(Cell::Error(text)) = app.transcript.last() else {
        panic!("expected an error, got {:?}", app.transcript.last());
    };
    assert!(text.contains("keke mcp login vercel"), "{text}");
}

#[tokio::test]
async fn signing_in_to_a_local_server_is_refused_where_it_was_typed() {
    let (app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    let mut app = app.with_mcp(vec![server("files", false, false)], None, None);

    type_text(&mut app, "/mcp login files");
    app.handle_key(key(KeyCode::Enter));

    let Some(Cell::Error(text)) = app.transcript.last() else {
        panic!("expected an error, got {:?}", app.transcript.last());
    };
    assert!(text.contains("nothing to sign in to"), "{text}");
}

#[tokio::test]
async fn an_unknown_server_name_is_refused_rather_than_started() {
    let (app, _scripted, _updates, _local) = app_with_commands(Vec::new(), Vec::new());
    let mut app = app.with_mcp(vec![server("vercel", true, true)], None, None);

    type_text(&mut app, "/mcp login nothing-like-that");
    app.handle_key(key(KeyCode::Enter));

    let Some(Cell::Error(text)) = app.transcript.last() else {
        panic!("expected an error, got {:?}", app.transcript.last());
    };
    assert!(text.contains("no MCP server named"), "{text}");
}
