//! An MCP tool reaches a vendor under a name the vendor accepts, and still
//! runs and is logged under keke's own id.
//!
//! MCP tool ids are `plugin:server:tool`, and OpenAI and Anthropic reject any
//! function name outside `^[a-zA-Z0-9_-]{1,64}$` — failing the whole request,
//! not the one tool. A server name containing `-` is the shape people really
//! configure, so it is the one exercised here, through the real binary.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::process::Command;

use keke_test_support::Endpoint;
use keke_test_support::MockInferenceServer;
use keke_test_support::Reply;

const ECHO_SERVER: &str = r#"#!/usr/bin/env python3
import json, sys

def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

TOOLS = [{"name": "echo", "description": "Echo text back.",
          "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}]

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    message = json.loads(line)
    ident = message.get("id")
    if ident is None:
        continue
    method = message.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": ident, "result": {"protocolVersion": "2025-06-18",
              "capabilities": {}, "serverInfo": {"name": "echo", "version": "1"}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": ident, "result": {"tools": TOOLS}})
    elif method == "tools/call":
        text = message.get("params", {}).get("arguments", {}).get("text", "")
        send({"jsonrpc": "2.0", "id": ident, "result":
              {"content": [{"type": "text", "text": text}]}})
    else:
        send({"jsonrpc": "2.0", "id": ident,
              "error": {"code": -32601, "message": "method not found"}})
"#;

fn is_valid_wire_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn session_events(home: &std::path::Path) -> Vec<serde_json::Value> {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for path in entries.filter_map(|e| Some(e.ok()?.path())) {
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                out.push(path);
            }
        }
    }
    let mut logs = Vec::new();
    walk(&home.join("sessions"), &mut logs);
    assert_eq!(logs.len(), 1, "one session log");
    std::fs::read_to_string(&logs[0])
        .expect("reads")
        .lines()
        .map(|line| serde_json::from_str(line).expect("parses"))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_mcp_tool_on_a_hyphenated_server_is_offered_under_a_name_vendors_accept() {
    let root = tempfile::tempdir().expect("tempdir");
    let home = root.path().join("keke-home");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&home).expect("mkdir");
    std::fs::create_dir_all(&workspace).expect("mkdir");
    std::fs::write(home.join("config.toml"), "provider = \"grok\"\n").expect("write");
    let script = home.join("echo.py");
    std::fs::write(&script, ECHO_SERVER).expect("write");
    std::fs::write(
        home.join(".mcp.json"),
        serde_json::json!({
            "mcpServers": {
                "codexia-bots": { "command": "python3", "args": [script] }
            }
        })
        .to_string(),
    )
    .expect("write");

    let server = MockInferenceServer::start().await;
    // A server in the person's own `.mcp.json` is owned by the `local` plugin.
    let id = "local:codexia-bots:echo";
    let wire_name = id.replace(':', "__");
    server.script(
        Endpoint::ChatCompletions,
        Reply::tool_call(&wire_name, serde_json::json!({ "text": "ping-from-mcp" })),
    );
    server.script(Endpoint::ChatCompletions, Reply::text("all done"));

    let output = Command::new(env!("CARGO_BIN_EXE_keke"))
        .env("KEKE_HOME", &home)
        .env("HOME", &home)
        .env("KEKE_CREDENTIAL_STORE", "file")
        .env("KEKE_IMPORT", "off")
        .env("XAI_BASE_URL", server.base_url())
        .env("XAI_API_KEY", "test-key")
        .env_remove("KEKE_PROVIDER")
        .env_remove("KEKE_MODEL")
        .args(["-C", &workspace.display().to_string()])
        .args(["exec", "--approval", "never", "use the echo tool"])
        .output()
        .expect("runs");
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // (a) Nothing the vendor would reject was sent, and the MCP tool was sent.
    let requests = server.requests_to(Endpoint::ChatCompletions);
    assert_eq!(requests.len(), 2, "one request per model step");
    for request in &requests {
        let names: Vec<&str> = request.body["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|tool| tool["function"]["name"].as_str().expect("name"))
            .collect();
        for name in &names {
            assert!(is_valid_wire_name(name), "{name:?} would be rejected");
        }
        assert!(names.contains(&wire_name.as_str()), "{names:?}");
    }

    // (b) The call the model made under the wire name reached the server.
    let events = session_events(&home);
    let end = events
        .iter()
        .find(|event| event["kind"] == "tool_call_end")
        .expect("a tool result");
    assert_eq!(end["result"]["status"], "ok", "{end}");
    assert!(end.to_string().contains("ping-from-mcp"), "{end}");

    // (c) Keke's own record keeps its own id, not the vendor-facing rewrite.
    let start = events
        .iter()
        .find(|event| event["kind"] == "tool_call_start")
        .expect("a tool call");
    assert!(start.to_string().contains(id), "{start}");
    assert!(!start.to_string().contains(&wire_name), "{start}");
}
