use std::process::Stdio;
use std::sync::Arc;

use keke_paths::AbsPath;
use keke_plugin::ResolvedMcpServer;
use serde_json::Value;
use serde_json::json;
use tokio::sync::OnceCell;

use crate::McpOptions;
use crate::client::Connection;
use crate::client::RpcError;

/// The MCP revision this client speaks. Not a deployment choice: it is a
/// property of the code below, so it is a constant rather than configuration.
///
/// This is a *legacy-era* revision in the spec's own terms: it opens a session
/// with an `initialize` handshake. Revision `2026-07-28` replaced that with
/// per-request version metadata and a mandatory `server/discover`, and its
/// compatibility matrix is explicit that a legacy client talking to a
/// modern-only server fails with no way to fall forward. Servers that serve
/// both eras answer `initialize` and work; ones that go modern-only will not.
/// Speaking the modern era is a change to this crate, not to this constant.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// One plugin-contributed server, and the connection to it once there is one.
///
/// The connection is created on first use and never twice: concurrent callers
/// all await the same initialization, and a server that failed to start keeps
/// its failure rather than being respawned on every call.
pub(crate) struct McpServer {
    plugin: String,
    name: String,
    command: String,
    args: Vec<String>,
    /// The environment as the manifest wrote it, `${VAR}` references intact.
    /// Expansion happens in [`Self::spawn`] and the result is never stored, so
    /// no value here and no `Debug` output can carry a secret.
    env: Vec<(String, String)>,
    root: AbsPath,
    options: McpOptions,
    connection: OnceCell<Result<Arc<Connection>, String>>,
}

impl std::fmt::Debug for McpServer {
    /// Prints env *names* only. A manifest may inline a value directly, and a
    /// resolved server is held for the whole session — a derived `Debug` would
    /// put that value into any log line that formatted one.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServer")
            .field("plugin", &self.plugin)
            .field("name", &self.name)
            .field("command", &self.command)
            .field("args", &self.args)
            .field(
                "env",
                &self.env.iter().map(|(key, _)| key).collect::<Vec<_>>(),
            )
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

/// A tool as the server described it.
pub(crate) struct RemoteTool {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) input_schema: Value,
}

impl McpServer {
    pub(crate) fn new(resolved: &ResolvedMcpServer, options: McpOptions) -> Self {
        Self {
            plugin: resolved.plugin.clone(),
            name: resolved.name.clone(),
            command: resolved.command.clone(),
            args: resolved.args.clone(),
            env: resolved.env.clone(),
            root: resolved.plugin_root.clone(),
            options,
            connection: OnceCell::new(),
        }
    }

    pub(crate) fn plugin(&self) -> &str {
        &self.plugin
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// The tools this server advertises.
    pub(crate) async fn list_tools(&self) -> Result<Vec<RemoteTool>, String> {
        let connection = self.connect().await?;
        let result = self
            .call_with_timeout(
                &connection,
                "tools/list",
                json!({}),
                self.options.startup_timeout_millis,
            )
            .await
            .map_err(|error| error.to_string())?;

        let listed = result
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| "`tools/list` did not return a `tools` array".to_string())?;

        Ok(listed
            .iter()
            .filter_map(|tool| {
                let name = tool.get("name").and_then(Value::as_str)?;
                Some(RemoteTool {
                    name: name.to_string(),
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    input_schema: tool
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or(Value::Object(serde_json::Map::new())),
                })
            })
            .collect())
    }

    /// Invoke one of this server's tools.
    pub(crate) async fn call_tool(
        &self,
        tool: &str,
        arguments: Value,
        timeout_millis: u64,
    ) -> Result<Value, String> {
        let connection = self.connect().await?;
        self.call_with_timeout(
            &connection,
            "tools/call",
            json!({"name": tool, "arguments": arguments}),
            timeout_millis,
        )
        .await
        .map_err(|error| error.to_string())
    }

    async fn call_with_timeout(
        &self,
        connection: &Connection,
        method: &str,
        params: Value,
        millis: u64,
    ) -> Result<Value, RpcError> {
        let deadline = std::time::Duration::from_millis(millis);
        match tokio::time::timeout(deadline, connection.request(method, params)).await {
            Ok(result) => result,
            Err(_) => Err(RpcError::Malformed {
                method: method.to_string(),
                detail: format!("no answer within {millis}ms"),
            }),
        }
    }

    /// The connection, opening it the first time and only the first time.
    async fn connect(&self) -> Result<Arc<Connection>, String> {
        self.connection
            .get_or_init(|| async { self.start().await })
            .await
            .clone()
    }

    async fn start(&self) -> Result<Arc<Connection>, String> {
        let child = self.spawn().map_err(|error| {
            format!(
                "could not start MCP server `{}` from plugin `{}`: {error}",
                self.name, self.plugin
            )
        })?;
        let connection = Arc::new(Connection::attach(child).map_err(|error| error.to_string())?);

        let handshake = self
            .call_with_timeout(
                &connection,
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "keke", "version": env!("CARGO_PKG_VERSION")},
                }),
                self.options.startup_timeout_millis,
            )
            .await;

        if let Err(error) = handshake {
            return Err(format!(
                "MCP server `{}` from plugin `{}` failed to initialize: {error}",
                self.name, self.plugin
            ));
        }

        // A server is entitled to refuse work until it has been told the
        // handshake is complete, so this is part of starting, not an extra.
        let _ = connection
            .notify("notifications/initialized", json!({}))
            .await;
        Ok(connection)
    }

    /// Spawn the child with its environment expanded and its plugin root as the
    /// working directory, so a server can find the files it ships with.
    fn spawn(&self) -> Result<tokio::process::Child, std::io::Error> {
        let mut command = tokio::process::Command::new(&self.command);
        command
            .args(&self.args)
            .current_dir(self.root.as_path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Nulled rather than captured: a server's diagnostics may repeat
            // the credentials it was handed, and anything captured here would
            // end up in an error string the model sees.
            .stderr(Stdio::null())
            .kill_on_drop(true);

        for (key, raw) in &self.env {
            let value = expand(raw);
            // An empty value is an absent one, never a configured one
            // (`AGENTS.md` invariant 8) — passing `TOKEN=""` would let a server
            // believe it was given a credential.
            if value.is_empty() {
                command.env_remove(key);
            } else {
                command.env(key, value);
            }
        }

        command.spawn()
    }
}

/// Substitute `${VAR}` from the host environment; an unset name yields nothing.
///
/// Done here rather than during plugin resolution so a resolved plugin set —
/// which is long-lived, cloned and logged — never holds an expanded secret.
fn expand(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        out.push_str(&std::env::var(&after[..end]).unwrap_or_default());
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::expand;

    #[test]
    fn an_unset_reference_expands_to_nothing() {
        // SAFETY-adjacent: the name is unique to this test so no other test
        // observes it.
        assert_eq!(expand("${KEKE_MCP_DEFINITELY_UNSET_XYZ}"), "");
        assert_eq!(expand("a-${KEKE_MCP_DEFINITELY_UNSET_XYZ}-b"), "a--b");
    }

    #[test]
    fn an_unterminated_reference_is_left_alone() {
        assert_eq!(expand("${OPEN"), "${OPEN");
        assert_eq!(expand("plain"), "plain");
    }
}
