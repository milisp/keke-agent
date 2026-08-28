// A panic in a test *is* the assertion, so `expect`/`unwrap` are the right
// thing here even though library code is warned against them.
#![allow(clippy::expect_used, clippy::unwrap_used)]
#![cfg(unix)]

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use keke_paths::AbsPath;
use keke_plugin::PluginScope;
use keke_plugin::PluginSet;
use keke_plugin::ResolvedMcpServer;
use keke_plugin::ResolvedPlugin;
use keke_plugin_api::ExtensionContext;
use keke_plugin_api::ExtensionRegistryBuilder;
use keke_protocol::ContentBlock;
use keke_protocol::SessionId;
use keke_protocol::ThreadId;
use keke_protocol::ToolCallId;
use keke_tool::ArcTool;
use keke_tool::ToolCallContext;
use keke_tool::ToolError;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;

/// A server that answers every method this client sends, reports its own
/// environment on request, and can be told to fail.
const GOOD_SERVER: &str = r#"#!/usr/bin/env python3
import json, os, sys

def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

log = os.environ.get("KEKE_TEST_START_LOG")
if log:
    with open(log, "a") as handle:
        handle.write("started\n")

TOOLS = [
    {"name": "echo", "description": "Echo text back.",
     "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
    {"name": "environment", "description": "Report the working directory and TOKEN.",
     "inputSchema": {"type": "object"}},
    {"name": "explode", "description": "Always reports an error.",
     "inputSchema": {"type": "object"}},
]

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
              "capabilities": {}, "serverInfo": {"name": "good", "version": "1"}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": ident, "result": {"tools": TOOLS}})
    elif method == "tools/call":
        params = message.get("params", {})
        name = params.get("name")
        arguments = params.get("arguments", {})
        if name == "echo":
            send({"jsonrpc": "2.0", "id": ident, "result":
                  {"content": [{"type": "text", "text": arguments.get("text", "")}]}})
        elif name == "environment":
            report = {"cwd": os.getcwd(), "token": os.environ.get("TOKEN", "<absent>")}
            send({"jsonrpc": "2.0", "id": ident, "result":
                  {"content": [{"type": "text", "text": json.dumps(report)}]}})
        elif name == "explode":
            send({"jsonrpc": "2.0", "id": ident, "result":
                  {"content": [{"type": "text", "text": "the tool refused"}], "isError": True}})
        else:
            send({"jsonrpc": "2.0", "id": ident,
                  "error": {"code": -32602, "message": "no such tool"}})
    else:
        # What a conformant legacy server does with `server/discover`: a plain
        # method-not-found, well outside the range the modern spec reserves.
        send({"jsonrpc": "2.0", "id": ident,
              "error": {"code": -32601, "message": "method not found"}})
"#;

/// A server that holds two calls before answering them, newest first, so a
/// client that correlated by arrival order would visibly swap the answers.
const REORDERING_SERVER: &str = r#"#!/usr/bin/env python3
import json, sys, time

def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

held = []
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
              "capabilities": {}, "serverInfo": {"name": "reordering", "version": "1"}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": ident, "result": {"tools": [
            {"name": "mark", "description": "Return the marker it was given.",
             "inputSchema": {"type": "object", "properties": {"marker": {"type": "string"}}}}]}})
    elif method == "tools/call":
        held.append((ident, message.get("params", {}).get("arguments", {}).get("marker", "")))
        if len(held) == 2:
            for ident, marker in reversed(held):
                time.sleep(0.05)
                send({"jsonrpc": "2.0", "id": ident, "result":
                      {"content": [{"type": "text", "text": marker}]}})
            held = []
    else:
        send({"jsonrpc": "2.0", "id": ident,
              "error": {"code": -32601, "message": "method not found"}})
"#;

/// A server that answers the handshake and the listing, then exits — the shape
/// of a server that dies partway through a session.
const QUITTING_SERVER: &str = r#"#!/usr/bin/env python3
import json, sys

def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

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
              "capabilities": {}, "serverInfo": {"name": "quitter", "version": "1"}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": ident, "result": {"tools": [
            {"name": "gone", "description": "Never answers.",
             "inputSchema": {"type": "object"}}]}})
        sys.exit(0)
    else:
        send({"jsonrpc": "2.0", "id": ident,
              "error": {"code": -32601, "message": "method not found"}})
"#;

/// A modern-era server: it implements `server/discover`, requires the
/// per-request `_meta` fields, and refuses `initialize` outright. A client that
/// still opened with a handshake would get nothing from it.
const MODERN_SERVER: &str = r##"#!/usr/bin/env python3
import json, os, sys

def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

log = os.environ.get("KEKE_TEST_PROBE_LOG")

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
    params = message.get("params", {})
    meta = params.get("_meta", {})
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": ident,
              "error": {"code": -32601, "message": "this server is modern only"}})
        continue
    if meta.get("io.modelcontextprotocol/protocolVersion") != "2026-07-28" \
            or "io.modelcontextprotocol/clientCapabilities" not in meta:
        send({"jsonrpc": "2.0", "id": ident,
              "error": {"code": -32602, "message": "missing required _meta"}})
        continue
    if method == "server/discover":
        if log:
            with open(log, "a") as handle:
                handle.write("probed\n")
        send({"jsonrpc": "2.0", "id": ident, "result": {"resultType": "complete",
              "supportedVersions": ["2026-07-28"], "capabilities": {"tools": {}}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": ident,
              "result": {"resultType": "complete", "tools": TOOLS}})
    elif method == "tools/call":
        text = params.get("arguments", {}).get("text", "")
        send({"jsonrpc": "2.0", "id": ident, "result": {"resultType": "complete",
              "content": [{"type": "text", "text": text}], "isError": False}})
"##;

/// A modern server that rejects the version keke opens with, naming what it
/// does speak. The rejection is a negotiation step, not a failure.
const MISMATCHED_SERVER: &str = r##"#!/usr/bin/env python3
import json, sys

def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    message = json.loads(line)
    ident = message.get("id")
    if ident is None:
        continue
    method = message.get("method")
    if method == "server/discover":
        send({"jsonrpc": "2.0", "id": ident, "error": {"code": -32022,
              "message": "Unsupported protocol version",
              "data": {"supported": ["2026-07-28", "1999-01-01"],
                       "requested": "2026-07-28"}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": ident, "result": {"resultType": "complete", "tools": [
            {"name": "echo", "description": "Echo text back.",
             "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}]}})
    elif method == "tools/call":
        text = message.get("params", {}).get("arguments", {}).get("text", "")
        send({"jsonrpc": "2.0", "id": ident, "result": {"resultType": "complete",
              "content": [{"type": "text", "text": text}]}})
"##;

/// A modern server that shares no revision with keke at all.
const ALIEN_SERVER: &str = r##"#!/usr/bin/env python3
import json, sys

def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    message = json.loads(line)
    ident = message.get("id")
    if ident is None:
        continue
    send({"jsonrpc": "2.0", "id": ident, "error": {"code": -32022,
          "message": "Unsupported protocol version",
          "data": {"supported": ["1999-01-01"], "requested": "2026-07-28"}}})
"##;

/// A legacy server that answers nothing at all for a method it does not know —
/// the case the spec allows for and the only one that costs a timeout.
const SILENT_LEGACY_SERVER: &str = r##"#!/usr/bin/env python3
import json, sys

def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

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
              "capabilities": {}, "serverInfo": {"name": "silent", "version": "1"}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": ident, "result": {"tools": [
            {"name": "echo", "description": "Echo text back.",
             "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}]}})
    elif method == "tools/call":
        text = message.get("params", {}).get("arguments", {}).get("text", "")
        send({"jsonrpc": "2.0", "id": ident, "result":
              {"content": [{"type": "text", "text": text}]}})
"##;

/// A legacy server that exits when it sees a method from after its time,
/// leaving no pipe for the handshake that should follow.
const BRITTLE_LEGACY_SERVER: &str = r##"#!/usr/bin/env python3
import json, sys

def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    message = json.loads(line)
    ident = message.get("id")
    if ident is None:
        continue
    method = message.get("method")
    if method == "server/discover":
        sys.exit(1)
    elif method == "initialize":
        send({"jsonrpc": "2.0", "id": ident, "result": {"protocolVersion": "2025-06-18",
              "capabilities": {}, "serverInfo": {"name": "brittle", "version": "1"}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": ident, "result": {"tools": [
            {"name": "echo", "description": "Echo text back.",
             "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}]}})
    elif method == "tools/call":
        text = message.get("params", {}).get("arguments", {}).get("text", "")
        send({"jsonrpc": "2.0", "id": ident, "result":
              {"content": [{"type": "text", "text": text}]}})
"##;

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write the fake server");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make it executable");
    }
    path
}

fn abs(path: &Path) -> AbsPath {
    AbsPath::new(path.canonicalize().expect("canonicalize")).expect("absolute")
}

/// A plugin whose only contribution is one MCP server.
fn plugin_with_server(
    name: &str,
    root: &Path,
    server: &str,
    command: &Path,
    env: Vec<(&str, &str)>,
) -> ResolvedPlugin {
    let root = abs(root);
    ResolvedPlugin {
        name: name.to_string(),
        version: None,
        description: None,
        scope: PluginScope::User,
        root: root.clone(),
        skills: Vec::new(),
        commands: Vec::new(),
        hooks: Vec::new(),
        mcp_servers: vec![ResolvedMcpServer {
            plugin: name.to_string(),
            name: server.to_string(),
            transport: keke_plugin::McpTransport::Stdio {
                command: command.to_string_lossy().into_owned(),
                args: Vec::new(),
                env: env
                    .into_iter()
                    .map(|(key, value)| (key.to_string(), value.to_string()))
                    .collect(),
            },
            plugin_root: root,
        }],
        unsupported: Vec::new(),
    }
}

/// Install a plugin set and collect the tools it ends up contributing.
fn installed_tools(plugins: Vec<ResolvedPlugin>) -> Vec<ArcTool> {
    let set = PluginSet::compose(plugins).expect("compose");
    let mut builder = ExtensionRegistryBuilder::new();
    keke_mcp::install(&mut builder, &set);
    let registry = builder.build();
    let ctx = ExtensionContext::new(SessionId::new(), ThreadId::new());
    registry
        .tool_contributors()
        .flat_map(|contributor| contributor.tools(&ctx))
        .collect()
}

/// The same, under budgets a test chooses. Separate rather than a parameter on
/// every call site: only the tests that exercise a timeout care.
fn installed_tools_with(
    plugins: Vec<ResolvedPlugin>,
    options: keke_mcp::McpOptions,
) -> Vec<ArcTool> {
    let set = PluginSet::compose(plugins).expect("compose");
    let mut builder = ExtensionRegistryBuilder::new();
    keke_mcp::install_with(&mut builder, &set, options);
    let registry = builder.build();
    let ctx = ExtensionContext::new(SessionId::new(), ThreadId::new());
    registry
        .tool_contributors()
        .flat_map(|contributor| contributor.tools(&ctx))
        .collect()
}

fn ids(tools: &[ArcTool]) -> Vec<String> {
    let mut ids: Vec<String> = tools.iter().map(|tool| tool.id().to_string()).collect();
    ids.sort();
    ids
}

fn find(tools: &[ArcTool], id: &str) -> ArcTool {
    tools
        .iter()
        .find(|tool| tool.id().as_str() == id)
        .unwrap_or_else(|| panic!("no tool `{id}` among {:?}", ids(tools)))
        .clone()
}

fn ctx(root: &Path) -> ToolCallContext {
    ToolCallContext {
        call_id: ToolCallId::new("call-1"),
        workspace_root: abs(root),
        timeout_millis: Some(20_000),
        cancelled: Arc::new(|| false),
    }
}

fn text_of(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_working_server_contributes_its_tools_and_answers_a_call() {
    let dir = TempDir::new().expect("tempdir");
    let server = script(dir.path(), "good.py", GOOD_SERVER);
    let tools = installed_tools(vec![plugin_with_server(
        "acme",
        dir.path(),
        "files",
        &server,
        Vec::new(),
    )]);

    assert_eq!(
        ids(&tools),
        vec![
            "acme:files:echo",
            "acme:files:environment",
            "acme:files:explode"
        ]
    );

    let echo = find(&tools, "acme:files:echo");
    let out = echo
        .call(ctx(dir.path()), json!({"text": "hello"}))
        .await
        .expect("the call succeeds");
    assert_eq!(text_of(&out.model_output), "hello");
}

/// The server declares the argument shape, but `ToolDyn::input_schema` is
/// derived from the Rust `Args` type through a blanket impl no tool can
/// override — so the declared schema has to reach the model in the prose.
#[tokio::test(flavor = "multi_thread")]
async fn the_servers_declared_argument_schema_reaches_the_model() {
    let dir = TempDir::new().expect("tempdir");
    let server = script(dir.path(), "good.py", GOOD_SERVER);
    let tools = installed_tools(vec![plugin_with_server(
        "acme",
        dir.path(),
        "files",
        &server,
        Vec::new(),
    )]);

    let tool = find(&tools, "acme:files:echo");

    let description = tool
        .description(&keke_tool::ListToolsContext::default())
        .text;
    assert!(description.contains("Echo text back."), "{description}");

    // The server declares its own argument shape, and that is what the model is
    // told to send. Deriving from the open map the arguments decode into would
    // advertise "any object", which is no help at all.
    let schema = tool.input_schema();
    assert!(schema["properties"]["text"].is_object(), "{schema}");
}

#[tokio::test(flavor = "multi_thread")]
async fn two_plugins_exposing_the_same_tool_name_cannot_collide() {
    let dir = TempDir::new().expect("tempdir");
    let alpha_root = dir.path().join("alpha");
    let beta_root = dir.path().join("beta");
    std::fs::create_dir_all(&alpha_root).expect("mkdir");
    std::fs::create_dir_all(&beta_root).expect("mkdir");
    let alpha_server = script(&alpha_root, "good.py", GOOD_SERVER);
    let beta_server = script(&beta_root, "good.py", GOOD_SERVER);

    let tools = installed_tools(vec![
        plugin_with_server("alpha", &alpha_root, "files", &alpha_server, Vec::new()),
        plugin_with_server("beta", &beta_root, "files", &beta_server, Vec::new()),
    ]);

    // Both servers advertise `echo`; the namespace is what keeps them apart,
    // and neither one is dropped or renamed to make room for the other.
    assert!(ids(&tools).contains(&"alpha:files:echo".to_string()));
    assert!(ids(&tools).contains(&"beta:files:echo".to_string()));
    assert_eq!(tools.len(), 6);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_fails_to_start_does_not_take_the_others_with_it() {
    let dir = TempDir::new().expect("tempdir");
    let good_root = dir.path().join("good");
    let broken_root = dir.path().join("broken");
    std::fs::create_dir_all(&good_root).expect("mkdir");
    std::fs::create_dir_all(&broken_root).expect("mkdir");
    let good = script(&good_root, "good.py", GOOD_SERVER);

    let tools = installed_tools(vec![
        plugin_with_server("works", &good_root, "files", &good, Vec::new()),
        plugin_with_server(
            "broken",
            &broken_root,
            "files",
            Path::new("keke-no-such-mcp-binary"),
            Vec::new(),
        ),
    ]);

    assert!(ids(&tools).contains(&"works:files:echo".to_string()));
    assert!(
        !ids(&tools).iter().any(|id| id.starts_with("broken:")),
        "a server that never started has no tools to offer"
    );

    // And the surviving server still works, which is the part that matters.
    let out = find(&tools, "works:files:echo")
        .call(ctx(&good_root), json!({"text": "still here"}))
        .await
        .expect("the healthy server is unaffected");
    assert_eq!(text_of(&out.model_output), "still here");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_response_never_reaches_the_wrong_waiter() {
    let dir = TempDir::new().expect("tempdir");
    let server = script(dir.path(), "reordering.py", REORDERING_SERVER);
    let tools = installed_tools(vec![plugin_with_server(
        "acme",
        dir.path(),
        "slow",
        &server,
        Vec::new(),
    )]);

    let mark = find(&tools, "acme:slow:mark");
    let first = mark.call(ctx(dir.path()), json!({"marker": "first"}));
    let second = mark.call(ctx(dir.path()), json!({"marker": "second"}));
    let (first, second) = tokio::join!(first, second);

    // The server answered these in the opposite order to the one they were
    // sent in. Correlation is by id, so each caller still gets its own answer.
    assert_eq!(text_of(&first.expect("first").model_output), "first");
    assert_eq!(text_of(&second.expect("second").model_output), "second");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_that_reports_an_error_comes_back_as_a_tool_error() {
    let dir = TempDir::new().expect("tempdir");
    let server = script(dir.path(), "good.py", GOOD_SERVER);
    let tools = installed_tools(vec![plugin_with_server(
        "acme",
        dir.path(),
        "files",
        &server,
        Vec::new(),
    )]);

    let error = find(&tools, "acme:files:explode")
        .call(ctx(dir.path()), json!({}))
        .await
        .expect_err("an in-band MCP error is still a failure");

    assert!(
        matches!(&error, ToolError::Execution { code, message }
            if code == "mcp_tool_error" && message.contains("the tool refused")),
        "got {error:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_dies_fails_its_own_calls_rather_than_hanging_them() {
    let dir = TempDir::new().expect("tempdir");
    let server = script(dir.path(), "quitter.py", QUITTING_SERVER);
    let tools = installed_tools(vec![plugin_with_server(
        "acme",
        dir.path(),
        "files",
        &server,
        Vec::new(),
    )]);

    // It listed its tools before exiting, so the tool exists and the failure
    // has to arrive as a terminal error on the call.
    let error = find(&tools, "acme:files:gone")
        .call(ctx(dir.path()), json!({}))
        .await
        .expect_err("a request nobody will answer is a failure, not a wait");

    assert!(
        matches!(&error, ToolError::Execution { code, .. } if code == "mcp_call_failed"),
        "got {error:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_env_reference_expands_at_spawn_time_and_an_empty_one_is_absent() {
    let dir = TempDir::new().expect("tempdir");
    let configured_root = dir.path().join("configured");
    let blank_root = dir.path().join("blank");
    std::fs::create_dir_all(&configured_root).expect("mkdir");
    std::fs::create_dir_all(&blank_root).expect("mkdir");
    let configured = script(&configured_root, "good.py", GOOD_SERVER);
    let blank = script(&blank_root, "good.py", GOOD_SERVER);

    // A variable the harness already set, so the expansion is observed without
    // this test having to mutate the process environment.
    let secret = std::env::var("HOME").expect("HOME is set");
    assert!(!secret.is_empty());

    let tools = installed_tools(vec![
        plugin_with_server(
            "configured",
            &configured_root,
            "files",
            &configured,
            vec![("TOKEN", "${HOME}")],
        ),
        plugin_with_server(
            "blank",
            &blank_root,
            "files",
            &blank,
            vec![("TOKEN", "${KEKE_TEST_MCP_UNSET_TOKEN}")],
        ),
    ]);

    let report = |tool: ArcTool, root: PathBuf| async move {
        let out = tool.call(ctx(&root), json!({})).await.expect("call");
        serde_json::from_str::<Value>(&text_of(&out.model_output)).expect("json report")
    };

    let configured_report = report(
        find(&tools, "configured:files:environment"),
        configured_root.clone(),
    )
    .await;
    assert_eq!(configured_report["token"], secret);

    let blank_report = report(find(&tools, "blank:files:environment"), blank_root).await;
    // Not `""`: an empty expansion is an absent credential, never a configured
    // one, so the variable is not set on the child at all.
    assert_eq!(blank_report["token"], "<absent>");

    // And the server was started where its own files live.
    let expected = abs(&configured_root).as_str().to_string();
    assert_eq!(configured_report["cwd"], expected);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_is_started_once_however_many_calls_share_it() {
    let dir = TempDir::new().expect("tempdir");
    let server = script(dir.path(), "good.py", GOOD_SERVER);
    let log = dir.path().join("starts.log");

    let tools = installed_tools(vec![plugin_with_server(
        "acme",
        dir.path(),
        "files",
        &server,
        vec![("KEKE_TEST_START_LOG", log.to_string_lossy().as_ref())],
    )]);

    let echo = find(&tools, "acme:files:echo");
    let calls = (0..8).map(|index| {
        let echo = echo.clone();
        let root = dir.path().to_path_buf();
        async move {
            echo.call(ctx(&root), json!({"text": index.to_string()}))
                .await
                .expect("call")
        }
    });
    let results = futures::future::join_all(calls).await;

    for (index, result) in results.iter().enumerate() {
        assert_eq!(text_of(&result.model_output), index.to_string());
    }

    let starts = std::fs::read_to_string(&log).expect("the server recorded its start");
    assert_eq!(
        starts.lines().count(),
        1,
        "listing and every call share one process, not one each"
    );
}

// ---------------------------------------------------------------------------
// Protocol eras
// ---------------------------------------------------------------------------

/// Run `text` through a server's `echo` tool and return what came back.
async fn echo_through(tool: &ArcTool, root: &Path, text: &str) -> String {
    let out = tool
        .call(ctx(root), json!({"text": text}))
        .await
        .expect("the call succeeded");
    text_of(&out.model_output)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_modern_only_server_is_usable_without_a_handshake() {
    let dir = TempDir::new().expect("tempdir");
    let command = script(dir.path(), "modern.py", MODERN_SERVER);
    let tools = installed_tools(vec![plugin_with_server(
        "acme",
        dir.path(),
        "api",
        &command,
        Vec::new(),
    )]);

    // The fixture answers `initialize` with an error, so tools existing at all
    // proves keke never opened with one.
    assert_eq!(ids(&tools), vec!["acme:api:echo"]);
    let echo = find(&tools, "acme:api:echo");
    assert_eq!(echo_through(&echo, dir.path(), "modern").await, "modern");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_legacy_server_still_works_after_being_probed() {
    let dir = TempDir::new().expect("tempdir");
    let command = script(dir.path(), "good.py", GOOD_SERVER);
    let tools = installed_tools(vec![plugin_with_server(
        "acme",
        dir.path(),
        "api",
        &command,
        Vec::new(),
    )]);

    let echo = find(&tools, "acme:api:echo");
    assert_eq!(echo_through(&echo, dir.path(), "legacy").await, "legacy");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_version_names_a_shared_one_rather_than_ending_the_conversation() {
    let dir = TempDir::new().expect("tempdir");
    let command = script(dir.path(), "mismatch.py", MISMATCHED_SERVER);
    let tools = installed_tools(vec![plugin_with_server(
        "acme",
        dir.path(),
        "api",
        &command,
        Vec::new(),
    )]);

    // The probe was refused with `UnsupportedProtocolVersionError`. That names
    // a version keke speaks, so the server is usable rather than lost.
    let echo = find(&tools, "acme:api:echo");
    assert_eq!(echo_through(&echo, dir.path(), "shared").await, "shared");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_shares_no_version_contributes_no_tools() {
    let dir = TempDir::new().expect("tempdir");
    let alien = script(dir.path(), "alien.py", ALIEN_SERVER);
    let good = script(dir.path(), "good.py", GOOD_SERVER);
    let tools = installed_tools(vec![
        plugin_with_server("alien", dir.path(), "api", &alien, Vec::new()),
        plugin_with_server("acme", dir.path(), "api", &good, Vec::new()),
    ]);

    // No overlap means no tools from that server, and — because it is reported
    // rather than swallowed — no effect on the servers that do work.
    assert!(!ids(&tools).iter().any(|id| id.starts_with("alien:")));
    assert!(ids(&tools).iter().any(|id| id.starts_with("acme:")));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_silent_legacy_server_is_reached_by_falling_back() {
    let dir = TempDir::new().expect("tempdir");
    let command = script(dir.path(), "silent.py", SILENT_LEGACY_SERVER);
    // A server that ignores the probe entirely costs a timeout, so this names a
    // smaller budget than the deployment default. Not smaller still: the first
    // exec of a just-written script can take about a second, and a probe budget
    // under that would time out before the server ever read its stdin — proving
    // nothing about the fallback.
    let tools = installed_tools_with(
        vec![plugin_with_server(
            "acme",
            dir.path(),
            "api",
            &command,
            Vec::new(),
        )],
        keke_mcp::McpOptions {
            startup_timeout_millis: 6_000,
            call_timeout_millis: 5_000,
            ..keke_mcp::McpOptions::default()
        },
    );

    let echo = find(&tools, "acme:api:echo");
    assert_eq!(echo_through(&echo, dir.path(), "silent").await, "silent");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_legacy_server_that_dies_on_the_probe_still_gets_its_handshake() {
    let dir = TempDir::new().expect("tempdir");
    let command = script(dir.path(), "brittle.py", BRITTLE_LEGACY_SERVER);
    let tools = installed_tools(vec![plugin_with_server(
        "acme",
        dir.path(),
        "api",
        &command,
        Vec::new(),
    )]);

    // The probe killed the first child. Falling back has to mean a new process,
    // not `initialize` down a pipe nobody is reading.
    let echo = find(&tools, "acme:api:echo");
    assert_eq!(echo_through(&echo, dir.path(), "brittle").await, "brittle");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_modern_server_is_not_probed_twice() {
    let dir = TempDir::new().expect("tempdir");
    let command = script(dir.path(), "modern.py", MODERN_SERVER);
    let log = dir.path().join("probes");
    let tools = installed_tools(vec![plugin_with_server(
        "acme",
        dir.path(),
        "api",
        &command,
        vec![("KEKE_TEST_PROBE_LOG", log.to_str().expect("utf-8"))],
    )]);

    let echo = find(&tools, "acme:api:echo");
    echo_through(&echo, dir.path(), "one").await;
    echo_through(&echo, dir.path(), "two").await;

    // The era is a property of the server, not of a request: listing and two
    // calls share the one answer.
    let probes = std::fs::read_to_string(&log).expect("the server was probed");
    assert_eq!(probes.lines().count(), 1, "probed more than once: {probes}");
}
