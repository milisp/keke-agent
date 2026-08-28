//! Signing in to a remote MCP server, against a server that behaves like one.
//!
//! The mock refuses everything without a bearer token and answers the whole
//! discovery chain the MCP spec defines. That is the point: the assertion is
//! not that keke sends particular requests, it is that a person who runs the
//! login can afterwards use the server's tools — through the ordinary tool
//! registry, with nothing about authentication visible at that layer.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::Mutex;

use keke_auth_api::LoginUi;
use keke_mcp::AuthHome;
use keke_mcp::ServerAuth;
use keke_paths::AbsPath;
use serde_json::Value;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

/// A browser that is not there: it records the URL, so the test can follow the
/// redirect itself the way a person's browser would.
#[derive(Clone, Default)]
struct FakeBrowser {
    urls: Arc<Mutex<Vec<String>>>,
}

impl LoginUi for FakeBrowser {
    fn open_browser(&self, url: &str) {
        self.urls.lock().unwrap().push(url.to_string());
    }
    fn show_device_code(&self, _code: &str, _uri: &str) {}
}

/// The MCP endpoint: a 401 with a challenge until a bearer token shows up.
///
/// The challenge names where the metadata is, which is the authoritative path
/// the spec defines — a client that only knew the well-known locations would
/// still work here, and would not be testing that.
struct Endpoint {
    metadata_url: String,
}

impl Respond for Endpoint {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let authorized = request
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value == "Bearer access-1");
        if !authorized {
            return ResponseTemplate::new(401).insert_header(
                "www-authenticate",
                format!(r#"Bearer resource_metadata="{}""#, self.metadata_url).as_str(),
            );
        }

        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let id = body.get("id").cloned().unwrap_or(Value::Null);
        let result = match body.get("method").and_then(Value::as_str) {
            // Not a modern-era server: it wants the legacy handshake.
            Some("server/discover") => {
                return ResponseTemplate::new(200).set_body_json(json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": "method not found"},
                }));
            }
            Some("initialize") => json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "serverInfo": {"name": "remote", "version": "1"},
            }),
            Some("tools/list") => json!({"tools": [{
                "name": "deploy",
                "description": "Ship it.",
                "inputSchema": {"type": "object"},
            }]}),
            _ => json!({}),
        };
        ResponseTemplate::new(200)
            .set_body_json(json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }
}

async fn mount(server: &MockServer) {
    let base = server.uri();

    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(Endpoint {
            metadata_url: format!("{base}/.well-known/oauth-protected-resource"),
        })
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "resource": base,
            "authorization_servers": [base],
            "scopes_supported": ["mcp:tools"],
        })))
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": base,
            "authorization_endpoint": format!("{base}/authorize"),
            "token_endpoint": format!("{base}/token"),
            "registration_endpoint": format!("{base}/register"),
        })))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/register"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "client_id": "client-1",
        })))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "access-1",
            "refresh_token": "refresh-1",
            "token_type": "Bearer",
            "expires_in": 3600,
        })))
        .mount(server)
        .await;
}

/// Follow the authorize URL the way a browser would, then hit the loopback.
async fn follow_the_redirect(browser: &FakeBrowser) {
    let authorize = loop {
        if let Some(url) = browser.urls.lock().unwrap().first() {
            break url::Url::parse(url).expect("an authorize URL");
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    };

    let params: std::collections::BTreeMap<_, _> = authorize.query_pairs().collect();
    // The parameters that make this flow safe are asserted here rather than in
    // a separate test, because this is the only place they are observable.
    assert_eq!(params["code_challenge_method"], "S256");
    assert_eq!(params["client_id"], "client-1");
    assert!(
        !params.contains_key("code_verifier"),
        "the verifier stays here"
    );
    assert!(
        params.contains_key("resource"),
        "the token must be audience-bound"
    );
    assert_eq!(params["scope"], "mcp:tools");

    let redirect = &params["redirect_uri"];
    let callback = format!("{redirect}?code=auth-code-1&state={}", params["state"]);
    let _ = reqwest::get(&callback).await;
}

fn home(dir: &tempfile::TempDir) -> AuthHome {
    AuthHome::new(&AbsPath::new(dir.path()).expect("an absolute path"))
}

#[tokio::test]
async fn signing_in_makes_a_protected_server_usable() {
    let server = MockServer::start().await;
    mount(&server).await;
    let dir = tempfile::tempdir().expect("tempdir");

    let auth = Arc::new(ServerAuth::new(home(&dir), "vercel", &server.uri()).expect("prepared"));
    assert!(!auth.has_credential(), "nothing is stored before a login");
    assert_eq!(auth.bearer().await, None);

    let browser = FakeBrowser::default();
    let login = tokio::spawn({
        let auth = Arc::clone(&auth);
        let browser = browser.clone();
        async move { auth.login(&browser).await }
    });
    follow_the_redirect(&browser).await;
    login.await.expect("the task").expect("the login");

    assert!(auth.has_credential());
    assert_eq!(auth.bearer().await.as_deref(), Some("access-1"));

    // The credential is a file only this person can read, in their own
    // directory — the same store a provider login writes to.
    let files: Vec<String> = std::fs::read_dir(dir.path())
        .expect("readable")
        .filter_map(|entry| Some(entry.ok()?.file_name().to_string_lossy().into_owned()))
        .collect();
    assert!(
        files
            .iter()
            .any(|name| name.starts_with("auth.mcp-vercel-")),
        "{files:?}"
    );
    // The registration is kept beside it, and is not a secret.
    assert!(
        files.iter().any(|name| name == "mcp-clients.json"),
        "{files:?}"
    );
}

#[tokio::test]
async fn a_signed_in_server_contributes_its_tools() {
    let server = MockServer::start().await;
    mount(&server).await;
    let dir = tempfile::tempdir().expect("tempdir");

    let auth = ServerAuth::new(home(&dir), "vercel", &server.uri()).expect("prepared");
    let browser = FakeBrowser::default();
    let login = tokio::spawn({
        let browser = browser.clone();
        async move { auth.login(&browser).await }
    });
    follow_the_redirect(&browser).await;
    login.await.expect("the task").expect("the login");

    // Nothing above this line is visible to the tool layer: the server is
    // installed the ordinary way and the token is attached beneath it.
    let tools = installed_tools(&server.uri(), home(&dir));
    assert_eq!(tools, vec!["local:vercel:deploy"]);
}

/// Install one remote server and collect the tool names it contributes.
fn installed_tools(url: &str, auth: AuthHome) -> Vec<String> {
    use keke_plugin::McpTransport;
    use keke_plugin::PluginScope;
    use keke_plugin::PluginSet;
    use keke_plugin::ResolvedMcpServer;
    use keke_plugin::ResolvedPlugin;
    use keke_plugin_api::ExtensionContext;
    use keke_plugin_api::ExtensionRegistryBuilder;

    let root = AbsPath::new(std::env::temp_dir()).expect("a root");
    let plugin = ResolvedPlugin {
        name: "local".to_string(),
        version: None,
        description: None,
        scope: PluginScope::User,
        root: root.clone(),
        skills: Vec::new(),
        commands: Vec::new(),
        hooks: Vec::new(),
        mcp_servers: vec![ResolvedMcpServer {
            plugin: "local".to_string(),
            name: "vercel".to_string(),
            transport: McpTransport::Http {
                url: url.to_string(),
                headers: Vec::new(),
            },
            plugin_root: root,
        }],
        unsupported: Vec::new(),
    };

    let set = PluginSet::compose(vec![plugin]).expect("composes");
    let mut builder = ExtensionRegistryBuilder::new();
    keke_mcp::install_with(
        &mut builder,
        &set,
        keke_mcp::McpOptions {
            startup_timeout_millis: 10_000,
            call_timeout_millis: 10_000,
            auth: Some(auth),
        },
    );
    let registry = builder.build();
    let ctx = ExtensionContext::new(
        keke_protocol::SessionId::new(),
        keke_protocol::ThreadId::new(),
    );
    registry
        .tool_contributors()
        .flat_map(|contributor| contributor.tools(&ctx))
        .map(|tool| tool.id().to_string())
        .collect()
}
