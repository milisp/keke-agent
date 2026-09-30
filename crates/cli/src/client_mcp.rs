//! MCP servers an ACP client supplies with a session.
//!
//! These are not repository content. The client is the program that launched
//! keke and chose to send them, so the workspace trust gate that withholds a
//! cloned repository's plugins (`AGENTS.md` invariant 13) does not apply: a
//! repository cannot cause one to appear. That is also why there is no
//! approval step here — the only way to reach this code is a connection its
//! owner already trusts with the rest of the session.

use anyhow::Result;
use anyhow::bail;
use keke_acp::ClientMcpServer;
use keke_acp::ClientMcpTransport;
use keke_paths::AbsPath;
use keke_plugin::McpTransport;
use keke_plugin::ResolvedMcpServer;

/// The namespace client servers' tools live under (`acp:<server>:<tool>`).
///
/// Short and readable in an approval prompt. It cannot collide with another
/// server's tools even if a plugin is also called `acp`, because
/// [`refuse_collisions`] rejects the same server name on both sides.
const NAMESPACE: &str = "acp";

/// Turn what the client sent into servers keke-mcp can run.
///
/// A malformed entry fails the whole session rather than being dropped: a
/// client that listed a server expects its tools, and a session quietly
/// missing them looks like a model that chose not to use them.
pub(crate) fn resolve(
    servers: Vec<ClientMcpServer>,
    cwd: &std::path::Path,
) -> Result<Vec<ResolvedMcpServer>> {
    // Relative to the session, which is where a client that sent a relative
    // command expects it to be resolved from.
    let root = AbsPath::new(cwd)
        .map_err(|error| anyhow::anyhow!("the session directory is unusable for MCP: {error}"))?;
    let mut resolved: Vec<ResolvedMcpServer> = Vec::new();
    for server in servers {
        if server.name.trim().is_empty() {
            bail!("an MCP server sent by the client has no name");
        }
        if resolved.iter().any(|seen| seen.name == server.name) {
            bail!(
                "the client sent two MCP servers named `{}`; names must be unique",
                server.name
            );
        }
        let transport = match server.transport {
            ClientMcpTransport::Stdio { command, args, env } => {
                let command = command.to_string_lossy().into_owned();
                if command.is_empty() {
                    bail!("MCP server `{}` has an empty command", server.name);
                }
                McpTransport::Stdio { command, args, env }
            }
            ClientMcpTransport::Http { url, headers } => {
                if url.is_empty() {
                    bail!("MCP server `{}` has an empty url", server.name);
                }
                McpTransport::Http { url, headers }
            }
            ClientMcpTransport::Sse { url, headers } => {
                if url.is_empty() {
                    bail!("MCP server `{}` has an empty url", server.name);
                }
                McpTransport::Sse { url, headers }
            }
        };
        resolved.push(ResolvedMcpServer {
            plugin: NAMESPACE.to_string(),
            name: server.name,
            transport,
            plugin_root: root.clone(),
            disabled: false,
        });
    }
    Ok(resolved)
}

/// Fail if a client server shares a name with one already configured.
///
/// Two servers answering to one name is ambiguous — `keke mcp disable <name>`
/// and the `/mcp` overlay would not know which one they mean — and ambiguity
/// fails loud (invariant 8) rather than letting one silently win.
pub(crate) fn refuse_collisions<'a>(
    client: &[ResolvedMcpServer],
    configured: impl IntoIterator<Item = &'a str>,
) -> Result<()> {
    for name in configured {
        if client.iter().any(|server| server.name == name) {
            bail!(
                "the client sent an MCP server named `{name}`, but keke already has one \
                 with that name configured; rename one of them"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn stdio(name: &str, command: &str) -> ClientMcpServer {
        ClientMcpServer {
            name: name.to_string(),
            transport: ClientMcpTransport::Stdio {
                command: PathBuf::from(command),
                args: vec!["--x".to_string()],
                env: vec![("K".to_string(), "v".to_string())],
            },
        }
    }

    #[test]
    fn a_client_server_runs_in_the_session_directory_under_its_own_namespace() {
        let resolved = resolve(vec![stdio("fs", "/bin/fs")], std::path::Path::new("/work"))
            .expect("a well-formed server resolves");
        assert_eq!(
            resolved,
            vec![ResolvedMcpServer {
                plugin: "acp".to_string(),
                name: "fs".to_string(),
                transport: McpTransport::Stdio {
                    command: "/bin/fs".to_string(),
                    args: vec!["--x".to_string()],
                    env: vec![("K".to_string(), "v".to_string())],
                },
                plugin_root: AbsPath::new("/work").expect("absolute"),
                disabled: false,
            }]
        );
    }

    #[test]
    fn remote_transports_keep_their_kind_and_headers() {
        let headers = vec![("Authorization".to_string(), "Bearer x".to_string())];
        let resolved = resolve(
            vec![
                ClientMcpServer {
                    name: "h".to_string(),
                    transport: ClientMcpTransport::Http {
                        url: "https://a.test/mcp".to_string(),
                        headers: headers.clone(),
                    },
                },
                ClientMcpServer {
                    name: "s".to_string(),
                    transport: ClientMcpTransport::Sse {
                        url: "https://a.test/sse".to_string(),
                        headers: Vec::new(),
                    },
                },
            ],
            std::path::Path::new("/work"),
        )
        .expect("remote servers resolve");
        assert_eq!(
            resolved[0].transport,
            McpTransport::Http {
                url: "https://a.test/mcp".to_string(),
                headers
            }
        );
        assert_eq!(resolved[1].transport.kind(), "sse");
    }

    #[test]
    fn two_client_servers_with_one_name_are_refused_by_name() {
        let error = resolve(
            vec![stdio("dup", "/bin/a"), stdio("dup", "/bin/b")],
            std::path::Path::new("/work"),
        )
        .expect_err("ambiguity fails loud");
        assert!(error.to_string().contains("`dup`"), "{error}");
    }

    #[test]
    fn a_client_server_cannot_share_a_name_with_a_configured_one() {
        let client =
            resolve(vec![stdio("fs", "/bin/fs")], std::path::Path::new("/work")).expect("resolves");
        let error = refuse_collisions(&client, ["other", "fs"]).expect_err("collision");
        assert!(error.to_string().contains("`fs`"), "{error}");
        assert!(refuse_collisions(&client, ["other"]).is_ok());
    }

    #[test]
    fn an_empty_name_command_or_url_is_an_error() {
        let work = std::path::Path::new("/work");
        assert!(resolve(vec![stdio("", "/bin/a")], work).is_err());
        assert!(resolve(vec![stdio("a", "")], work).is_err());
        let empty_url = ClientMcpServer {
            name: "r".to_string(),
            transport: ClientMcpTransport::Http {
                url: String::new(),
                headers: Vec::new(),
            },
        };
        assert!(resolve(vec![empty_url], work).is_err());
    }
}
