//! The sourcing boundary is tested before server startup, through both ACP versions.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use keke_test_support::{Endpoint, MockInferenceServer, Reply};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const SERVER: &str = r#"import json, sys
with open(sys.argv[1], 'a') as f: f.write(sys.argv[2] + '\n')
for line in sys.stdin:
    m = json.loads(line)
    if 'id' not in m: continue
    if m['method'] == 'initialize':
        r = {'protocolVersion':'2025-06-18','capabilities':{},'serverInfo':{'name':'fixture','version':'1'}}
    elif m['method'] == 'tools/list':
        r = {'tools':[{'name':'echo','description':'fixture','inputSchema':{'type':'object'}}]}
    elif m['method'] == 'tools/call': r = {'content':[{'type':'text','text':'ok'}]}
    else:
        print(json.dumps({'jsonrpc':'2.0','id':m['id'],'error':{'code':-32601,'message':'unsupported'}}), flush=True)
        continue
    print(json.dumps({'jsonrpc':'2.0','id':m['id'],'result':r}), flush=True)
"#;

struct Pipe {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    id: u64,
}
impl Pipe {
    fn start(home: &std::path::Path, cwd: &std::path::Path, url: &str, strict: bool) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_keke"));
        command.args(["agent", "stdio"]);
        if strict {
            command.args(["--mcp-policy", "client-only"]);
        }
        let mut child = command
            .current_dir(cwd)
            .env("KEKE_HOME", home)
            .env("HOME", home)
            .env("KEKE_CREDENTIAL_STORE", "file")
            .env("KEKE_IMPORT", "off")
            .env("KEKE_PROVIDER", "grok")
            .env("KEKE_MODEL", "grok-4.6")
            .env("XAI_BASE_URL", url)
            .env("XAI_API_KEY", "test-key")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("ACP starts");
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            input,
            output,
            id: 0,
        }
    }
    fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        writeln!(
            self.input,
            "{}",
            json!({"jsonrpc":"2.0","id":self.id,"method":method,"params":params})
        )
        .unwrap();
        self.input.flush().unwrap();
        loop {
            let mut line = String::new();
            assert!(
                self.output.read_line(&mut line).unwrap() > 0,
                "ACP closed during {method}"
            );
            let response: Value = serde_json::from_str(&line).unwrap();
            if response["method"] == "session/request_permission" {
                let answer = json!({"jsonrpc":"2.0","id":response["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow"}}});
                writeln!(self.input, "{answer}").unwrap();
                self.input.flush().unwrap();
                continue;
            }
            if response["method"].is_null() && response["id"] == self.id {
                return response;
            }
        }
    }
    fn initialize(&mut self, version: u8, strict: bool) {
        let params = if version == 1 {
            json!({"protocolVersion":1,"clientCapabilities":{}})
        } else {
            json!({"protocolVersion":2,"info":{"name":"fixture","version":"1"},"capabilities":{}})
        };
        let response = self.rpc("initialize", params);
        assert_eq!(
            response["result"]["_meta"]["keke.dev/mcp-policy"],
            if strict { "client-only" } else { "merge" }
        );
        let auth = self.rpc(
            if version == 1 {
                "authenticate"
            } else {
                "auth/login"
            },
            json!({"methodId":"grok","_meta":{"persist":false}}),
        );
        assert!(auth.get("error").is_none(), "{auth}");
    }
}
impl Drop for Pipe {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Fixture {
    root: tempfile::TempDir,
    home: std::path::PathBuf,
    cwd: std::path::PathBuf,
    script: std::path::PathBuf,
    marker: std::path::PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let cwd = root.path().join("workspace");
        let script = root.path().join("mcp.py");
        let marker = root.path().join("starts");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(cwd.join(".keke")).unwrap();
        std::fs::write(&script, SERVER).unwrap();
        let f = Self {
            root,
            home,
            cwd,
            script,
            marker,
        };
        std::fs::write(
            f.home.join(".mcp.json"),
            json!({"mcpServers":{"selected":f.definition("global")}}).to_string(),
        )
        .unwrap();
        let plugin = f.home.join("plugins/fixture");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(plugin.join("plugin.json"), r#"{"name":"fixture"}"#).unwrap();
        std::fs::write(
            plugin.join(".mcp.json"),
            json!({"mcpServers":{"plugin-server":f.definition("plugin")}}).to_string(),
        )
        .unwrap();
        std::fs::write(
            f.cwd.join(".keke/.mcp.json"),
            json!({"mcpServers":{"workspace-server":f.definition("workspace")}}).to_string(),
        )
        .unwrap();
        let trusted = Command::new(env!("CARGO_BIN_EXE_keke"))
            .current_dir(&f.cwd)
            .env("KEKE_HOME", &f.home)
            .env("HOME", &f.home)
            .env("KEKE_CREDENTIAL_STORE", "file")
            .env("KEKE_IMPORT", "off")
            .env("KEKE_PROVIDER", "grok")
            .env("XAI_API_KEY", "test-key")
            .args(["plugin", "trust", "workspace"])
            .output()
            .unwrap();
        assert!(
            trusted.status.success(),
            "{}",
            String::from_utf8_lossy(&trusted.stderr)
        );
        f
    }
    fn definition(&self, name: &str) -> Value {
        json!({"command":"/usr/bin/python3","args":[self.script,self.marker,name]})
    }
    fn client(&self, version: u8) -> Value {
        let mut server = self.definition("client");
        server["name"] = json!("selected");
        server["env"] = json!([]);
        if version == 2 {
            server["type"] = json!("stdio");
        }
        server
    }
    fn starts(&self) -> Vec<String> {
        std::fs::read_to_string(&self.marker)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn strict_new_load_resume_and_children_start_only_the_selected_server_once() {
    for version in [1, 2] {
        let f = Fixture::new();
        let inference = MockInferenceServer::start().await;
        inference.script(Endpoint::ChatCompletions, Reply::tool_call("spawn_agent", json!({"task":"inspect the tool list","title":"Inspect available tools","wait":true})));
        inference.script(Endpoint::ChatCompletions, Reply::text("child finished"));
        inference.script(Endpoint::ChatCompletions, Reply::text("parent finished"));
        let mut pipe = Pipe::start(&f.home, &f.cwd, &inference.base_url(), true);
        pipe.initialize(version, true);
        assert!(
            f.starts().is_empty(),
            "initialize/auth must not start configured MCP"
        );
        let response = pipe.rpc(
            "session/new",
            json!({"cwd":f.cwd,"mcpServers":[f.client(version)]}),
        );
        assert!(response.get("error").is_none(), "{response}");
        let id = response["result"]["sessionId"].clone();
        let response = pipe.rpc(
            "session/prompt",
            json!({"sessionId":id,"prompt":[{"type":"text","text":"delegate inspection"}]}),
        );
        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(
            f.starts(),
            ["client"],
            "parent and child share one MCP connection"
        );
        let requests = inference.requests_to(Endpoint::ChatCompletions);
        assert_eq!(requests.len(), 3, "the child actually ran");
        for (index, request) in requests.iter().enumerate() {
            let names: Vec<_> = request.body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["function"]["name"].as_str().unwrap())
                .collect();
            assert!(
                names.contains(&"acp__selected__echo"),
                "version {version} request {index}: {names:?}"
            );
            assert!(
                !names.iter().any(|n| n.starts_with("local__")
                    || n.starts_with("fixture__")
                    || n.starts_with("workspace__")),
                "{names:?}"
            );
        }
        assert!(
            !requests[1].body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["function"]["name"] == "spawn_agent"),
            "child cannot delegate again"
        );
        drop(pipe);
        // A resumed session's previous MCP list is not a standing grant.
        let mut pipe = Pipe::start(&f.home, &f.cwd, &inference.base_url(), true);
        pipe.initialize(version, true);
        let method = if version == 1 {
            "session/load"
        } else {
            "session/resume"
        };
        let response = pipe.rpc(method, json!({"sessionId":id,"cwd":f.cwd,"mcpServers":[]}));
        assert!(response.get("error").is_none(), "{response}");
        inference.script(Endpoint::ChatCompletions, Reply::text("resumed"));
        let response = pipe.rpc(
            "session/prompt",
            json!({"sessionId":id,"prompt":[{"type":"text","text":"continue"}]}),
        );
        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(f.starts(), ["client"]);
        let requests = inference.requests_to(Endpoint::ChatCompletions);
        let tools = requests.last().unwrap().body["tools"].as_array().unwrap();
        assert!(
            !tools
                .iter()
                .any(|t| t["function"]["name"].as_str().unwrap().contains("__echo"))
        );
        let _ = f.root.path();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn default_keeps_trusted_servers_and_strict_empty_and_duplicates_are_explicit() {
    for strict in [false, true] {
        let f = Fixture::new();
        let inference = MockInferenceServer::start().await;
        let mut pipe = Pipe::start(&f.home, &f.cwd, &inference.base_url(), strict);
        pipe.initialize(1, strict);
        let response = pipe.rpc("session/new", json!({"cwd":f.cwd,"mcpServers":[]}));
        assert!(response.get("error").is_none(), "{response}");
        inference.script(Endpoint::ChatCompletions, Reply::text("inspect tools"));
        let prompted = pipe.rpc("session/prompt", json!({"sessionId":response["result"]["sessionId"],"prompt":[{"type":"text","text":"inspect"}]}));
        assert!(prompted.get("error").is_none(), "{prompted}");
        let mut starts = f.starts();
        starts.sort();
        assert_eq!(
            starts,
            if strict {
                vec![]
            } else {
                vec!["global", "plugin", "workspace"]
            }
        );
        let response = pipe.rpc(
            "session/new",
            json!({"cwd":f.cwd,"mcpServers":[f.client(1),f.client(1)]}),
        );
        assert!(
            response["error"].to_string().contains("selected"),
            "{response}"
        );
        assert_eq!(f.starts().len(), if strict { 0 } else { 3 });
    }
}
