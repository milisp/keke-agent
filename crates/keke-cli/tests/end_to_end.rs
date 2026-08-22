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

use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

/// Frame chunks as server-sent events the way a chat-completions stream does.
fn sse(chunks: &[serde_json::Value]) -> String {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str(&format!("data: {chunk}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    body
}

struct Fixture {
    home: tempfile::TempDir,
    server: MockServer,
}

impl Fixture {
    async fn new() -> Self {
        Self {
            home: tempfile::tempdir().expect("tempdir"),
            server: MockServer::start().await,
        }
    }

    fn keke(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_keke"));
        command
            .env("KEKE_HOME", self.home.path())
            .env("XAI_BASE_URL", format!("{}/v1", self.server.uri()))
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
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&[
                serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[
                    {"index":0,"id":"call-1","type":"function",
                     "function":{"name":"list_dir","arguments":""}}]}}]}),
                serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[
                    {"index":0,"function":{"arguments":"{\"path\":\".\"}"}}]}}]}),
                serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
            ]),
            "text/event-stream",
        ))
        .up_to_n_times(1)
        .mount(&fixture.server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&[
                serde_json::json!({"choices":[{"index":0,"delta":{"content":"listed it"}}]}),
                serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
                                   "usage":{"prompt_tokens":100,"completion_tokens":20}}),
            ]),
            "text/event-stream",
        ))
        .mount(&fixture.server)
        .await;

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
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_string("{\"error\":\"bad model\"}"))
        .mount(&fixture.server)
        .await;

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
        .env_remove("XAI_API_KEY")
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
