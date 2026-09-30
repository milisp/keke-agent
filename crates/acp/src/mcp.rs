//! The MCP servers an ACP client asks a session to use.

use std::path::PathBuf;

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
    },
    /// A legacy HTTP+SSE endpoint. v1 only.
    Sse {
        url: String,
        headers: Vec<(String, String)>,
    },
}
