//! The MCP servers an ACP client asks a session to use.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One MCP server a client sent with `session/new`, `session/load` or
/// `session/resume`.
///
/// keke's own type rather than a protocol one: v1 and v2 declare separate wire
/// types for the same idea (v2 has no SSE, wraps paths differently), and the
/// [`SessionFactory`](crate::SessionFactory) must not learn which version
/// spoke. Each version converts once, at its edge, into this.
///
/// Nothing here is validated. An empty name or command is the factory's to
/// refuse, because it is the one that knows what else the session already has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientMcpServer {
    pub name: String,
    pub transport: ClientMcpTransport,
}

/// How a client-supplied server is reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientMcpTransport {
    /// A child process speaking over stdin and stdout.
    Stdio {
        command: PathBuf,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    /// A streamable-HTTP endpoint.
    Http {
        url: String,
        headers: Vec<(String, String)>,
        oauth: Option<ClientMcpOAuthConfig>,
    },
    /// A legacy HTTP+SSE endpoint. v1 only.
    Sse {
        url: String,
        headers: Vec<(String, String)>,
        oauth: Option<ClientMcpOAuthConfig>,
    },
}

/// A client registered out of band with an MCP authorization server.
///
/// ACP clients send this in a remote server's `_meta["keke.dev/oauth"]`.
/// Supplying it bypasses dynamic client registration. Secret values must use
/// `${VAR}` references, resolved only when requesting tokens. A fixed callback
/// must be a loopback HTTP URL keke can listen on.
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientMcpOAuthConfig {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
}

impl std::fmt::Debug for ClientMcpOAuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientMcpOAuthConfig")
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("redirect_uri", &self.redirect_uri)
            .finish()
    }
}

pub(crate) fn oauth_from_meta(
    meta: Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<Option<ClientMcpOAuthConfig>, agent_client_protocol::Error> {
    meta.and_then(|mut meta| meta.remove("keke.dev/oauth"))
        .map(|value| {
            let config: ClientMcpOAuthConfig = serde_json::from_value(value).map_err(|error| {
                agent_client_protocol::Error::invalid_params()
                    .data(format!("invalid MCP keke.dev/oauth metadata: {error}"))
            })?;
            if config.client_id.trim().is_empty() {
                return Err(agent_client_protocol::Error::invalid_params()
                    .data("MCP OAuth client_id must not be empty"));
            }
            Ok(config)
        })
        .transpose()
}
