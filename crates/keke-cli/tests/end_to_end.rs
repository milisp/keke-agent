//! Runs the real binary against a stub xAI server.
//!
//! Every other test in the workspace exercises one crate. This one exercises
//! the wiring: argument parsing, config resolution, credential lookup, provider
//! registration, the turn loop, tool dispatch, and the rollout log, in the same
//! process arrangement a person gets. It is the test that would have caught any
//! of those being connected to nothing.

// A panic is the assertion here, and an integration test is not `#[cfg(test)]`,
// so the workspace's allow-expect-in-tests setting does not reach it.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::process::Command;

use keke_test_support::Endpoint;
use keke_test_support::MockInferenceServer;
use keke_test_support::Reply;

struct Fixture {
    home: tempfile::TempDir,
    server: MockInferenceServer,
}

impl Fixture {
    async fn new() -> Self {
        Self {
            home: tempfile::tempdir().expect("tempdir"),
            server: MockInferenceServer::start().await,
        }
    }

    fn keke(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_keke"));
        command
            .env("KEKE_HOME", self.home.path())
            // The OS keyring is shared machine state; a test that read it would
            // pass or fail depending on who is logged in on this machine.
            .env("KEKE_CREDENTIAL_STORE", "file")
            // Another tool's login is shared machine state too: without this the
            // suite adopts whatever the developer signed into with the codex or
            // grok CLI.
            .env("KEKE_IMPORT", "off")
            .env("XAI_BASE_URL", self.server.base_url())
            .env("XAI_API_KEY", "test-key")
            // A stray real credential in the developer's environment must not
            // reach the stub server, and a real one must not be consulted here.
            .env_remove("KEKE_PROVIDER")
            .env_remove("KEKE_MODEL");
        command
    }

    fn sessions(&self) -> Vec<std::path::PathBuf> {
        let dir = self.home.path().join("sessions");
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .filter_map(|entry| Some(entry.ok()?.path()))
            .collect()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_runs_a_tool_and_records_a_replayable_session() {
    let fixture = Fixture::new().await;

    // First call: ask for a tool. Second: answer using its result.
    fixture.server.script(
        Endpoint::ChatCompletions,
        Reply::tool_call("list_dir", serde_json::json!({ "path": "." })),
    );
    fixture.server.script(
        Endpoint::ChatCompletions,
        Reply::text("listed it").with_usage(100, 20),
    );

    let workspace = tempfile::tempdir().expect("tempdir");
    std::fs::write(workspace.path().join("marker.txt"), "hi").expect("write");

    let output = fixture
        .keke()
        .args(["-C", &workspace.path().display().to_string()])
        .args(["exec", "list the files"])
        .output()
        .expect("runs");

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("listed it"),
        "the model's answer must reach stdout"
    );

    // The tool really ran, against the real workspace.
    let sessions = fixture.sessions();
    assert_eq!(sessions.len(), 1, "one session log");
    let log = std::fs::read_to_string(&sessions[0]).expect("reads");
    let events: Vec<serde_json::Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).expect("parses"))
        .collect();

    let kinds: Vec<&str> = events
        .iter()
        .map(|event| event["kind"].as_str().expect("kind"))
        .collect();
    assert_eq!(
        kinds,
        vec![
            "session_start",
            "turn_start",
            "model_request",
            "model_response",
            "tool_call_start",
            "tool_call_end",
            "model_request",
            "model_response",
            "turn_end",
        ]
    );

    let tool_end = events
        .iter()
        .find(|event| event["kind"] == "tool_call_end")
        .expect("a tool result");
    assert_eq!(tool_end["result"]["status"], "ok");
    assert!(
        log.contains("marker.txt"),
        "the tool listed the real workspace"
    );

    // The tools the model was offered are the tools that exist.
    let offered = events
        .iter()
        .find(|event| event["kind"] == "model_request")
        .expect("a request")["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|name| name.as_str().expect("str").to_string())
        .collect::<Vec<_>>();
    assert!(offered.contains(&"list_dir".to_string()), "{offered:?}");
    assert!(offered.contains(&"bash".to_string()), "{offered:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_failure_is_reported_and_still_logged() {
    let fixture = Fixture::new().await;
    fixture
        .server
        .script(Endpoint::ChatCompletions, Reply::status(400));

    let workspace = tempfile::tempdir().expect("tempdir");
    let output = fixture
        .keke()
        .args(["-C", &workspace.path().display().to_string()])
        .args(["exec", "hi"])
        .output()
        .expect("runs");

    assert!(!output.status.success(), "a 400 must fail the command");

    let sessions = fixture.sessions();
    let log = std::fs::read_to_string(&sessions[0]).expect("reads");
    assert!(
        log.contains("\"kind\":\"error\""),
        "the failure belongs in the log: {log}"
    );
}

#[test]
fn an_unknown_provider_names_the_ones_that_exist() {
    let home = tempfile::tempdir().expect("tempdir");
    let output = Command::new(env!("CARGO_BIN_EXE_keke"))
        .env("KEKE_HOME", home.path())
        .env("KEKE_CREDENTIAL_STORE", "file")
        .env("KEKE_IMPORT", "off")
        .args(["--provider", "nope", "exec", "hi"])
        .output()
        .expect("runs");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("nope"), "{stderr}");
    assert!(
        stderr.contains("grok"),
        "the hint must list what exists: {stderr}"
    );
}

#[test]
fn doctor_reports_what_was_resolved() {
    let home = tempfile::tempdir().expect("tempdir");
    let output = Command::new(env!("CARGO_BIN_EXE_keke"))
        .env("KEKE_HOME", home.path())
        .env("KEKE_CREDENTIAL_STORE", "file")
        .env("KEKE_IMPORT", "off")
        .env_remove("XAI_API_KEY")
        .env_remove("NVIDIA_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .arg("doctor")
        .output()
        .expect("runs");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("grok"), "{stdout}");
    assert!(
        stdout.contains("keke login"),
        "an unauthenticated provider must say how to fix it: {stdout}"
    );
}
