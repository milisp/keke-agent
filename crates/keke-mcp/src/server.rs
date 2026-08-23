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

/// The MCP revisions this client speaks, best first. Not a deployment choice:
/// each entry is a claim about the code below, so this is a constant rather
/// than configuration.
///
/// The list spans both of the spec's *eras*. A **modern** revision (`2026-07-28`
/// and later) carries its version in every request's `_meta` and has no
/// handshake; a **legacy** one opens a session with `initialize`. The spec's own
/// compatibility matrix says a client of one era fails against a server of the
/// other, and that a legacy client has no way to fall forward — so keke speaks
/// both and decides per server which it is talking to.
const MODERN_VERSIONS: &[&str] = &["2026-07-28"];

/// The legacy revision keke still speaks, for servers that never moved.
const LEGACY_VERSION: &str = "2025-06-18";

/// The `_meta` key namespace the modern era reserves for protocol fields.
const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";

/// Which era a particular server turned out to speak.
///
/// Decided once per server process and then reused. The spec is explicit that
/// the era is a property of the server rather than of a request, and re-probing
/// per call would both cost a round trip and risk two calls disagreeing.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Era {
    /// Per-request `_meta`, no handshake, at the agreed version.
    Modern(String),
    /// `initialize` first, then plain requests.
    Legacy,
}

/// Whether `code` identifies a peer that speaks the modern era.
///
/// Keyed on the range the spec reserves for itself (`-32020` to `-32099`),
/// never on one code. The spec requires exactly that: legacy servers answer an
/// unknown pre-`initialize` method with implementation-defined errors —
/// commonly `-32601` or `-32602`, both outside this range — so a reserved code
/// coming back is proof the peer understood a modern request enough to reject
/// it on modern grounds.
fn is_modern_error(code: i64) -> bool {
    (-32_099..=-32_020).contains(&code)
}

/// The best era we share with a server that told us what it speaks.
///
/// Modern is preferred wherever it is on offer. Falling to `LEGACY_VERSION`
/// when a server advertises it is not the "fall back to `initialize`" the spec
/// forbids after a modern error: that prohibition is about *guessing* an era
/// from an unrecognized failure, and this is the server naming the revision
/// itself.
fn shared_era(offered: &[String]) -> Option<Era> {
    if let Some(version) = MODERN_VERSIONS
        .iter()
        .find(|version| offered.iter().any(|offer| offer == *version))
    {
        return Some(Era::Modern((*version).to_string()));
    }
    offered
        .iter()
        .any(|offer| offer == LEGACY_VERSION)
        .then_some(Era::Legacy)
}

/// Version strings out of a `supportedVersions` / `supported` array.
fn versions_in(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

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
    session: OnceCell<Result<Arc<Session>, String>>,
}

/// A connection and the era it was found to speak.
///
/// The two are one value because they are decided together and neither is
/// meaningful without the other: sending modern `_meta` down a legacy
/// connection, or omitting it on a modern one, are both malformed.
struct Session {
    connection: Arc<Connection>,
    era: Era,
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

/// What asking `server/discover` established.
enum Probe {
    /// The server is modern, at this agreed version.
    Modern(String),
    /// The server wants the legacy handshake. `connection_usable` is false when
    /// the probe killed the pipe, which means the handshake needs a fresh child.
    Legacy { connection_usable: bool },
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
            session: OnceCell::new(),
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
        let session = self.connect().await?;
        let result = self
            .request(
                &session,
                "tools/list",
                json!({}),
                self.options.startup_timeout_millis,
            )
            .await?;

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
        let session = self.connect().await?;
        let result = self
            .request(
                &session,
                "tools/call",
                json!({"name": tool, "arguments": arguments}),
                timeout_millis,
            )
            .await?;

        // Only a completed result carries the content a tool call is for.
        // Anything else must be refused rather than passed on: a result keke
        // cannot parse has no `content`, and forwarding it would reach the
        // model as an empty success, which is a lie about what occurred.
        //
        // An absent `resultType` is a completed result — earlier revisions did
        // not send the field, and the spec requires reading its absence that
        // way.
        match result.get("resultType").and_then(Value::as_str) {
            None | Some("complete") => Ok(result),
            Some("input_required") => Err(format!(
                "MCP tool `{tool}` on server `{}` asked for more input, which keke does not provide",
                self.name
            )),
            // Extensions may define further kinds, and the spec is explicit
            // that a kind the client does not recognize is invalid. keke
            // advertises no extensions, so receiving one is the server's error.
            Some(other) => Err(format!(
                "MCP tool `{tool}` on server `{}` answered with `{other}`, which keke does not implement",
                self.name
            )),
        }
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

    /// The session, opening it the first time and only the first time.
    async fn connect(&self) -> Result<Arc<Session>, String> {
        self.session
            .get_or_init(|| async { self.start().await })
            .await
            .clone()
    }

    /// Spawn the server and work out which era it speaks.
    async fn start(&self) -> Result<Arc<Session>, String> {
        let connection = self.spawn_and_attach()?;

        // The probe and the legacy handshake share the one budget the
        // deployment set for startup, half each, because in the worst case both
        // are spent on the same server. Handing the probe the whole budget
        // would leave a silent legacy server no time to answer `initialize`
        // afterwards, which is precisely the case the fallback exists for.
        let half = self.options.startup_timeout_millis / 2;

        match self.probe(&connection, half).await? {
            Probe::Modern(version) => Ok(Arc::new(Session {
                connection,
                era: Era::Modern(version),
            })),
            // The peer never answered, or answered as a legacy server does.
            // Either way `initialize` is next; the only question is whether the
            // process it goes to is still alive.
            Probe::Legacy { connection_usable } => {
                let connection = if connection_usable {
                    connection
                } else {
                    // A server that exits on an unknown method leaves nothing to
                    // send `initialize` down. Re-spawning is the only way to give
                    // such a server the legacy opening it was waiting for, and it
                    // costs a process only in this one case.
                    self.spawn_and_attach()?
                };
                self.initialize(&connection, half).await?;
                Ok(Arc::new(Session {
                    connection,
                    era: Era::Legacy,
                }))
            }
        }
    }

    fn spawn_and_attach(&self) -> Result<Arc<Connection>, String> {
        let child = self.spawn().map_err(|error| {
            format!(
                "could not start MCP server `{}` from plugin `{}`: {error}",
                self.name, self.plugin
            )
        })?;
        Connection::attach(child)
            .map(Arc::new)
            .map_err(|error| error.to_string())
    }

    /// Ask `server/discover` and read the era off the answer.
    ///
    /// `Err` is reserved for a server that identified itself as modern and
    /// still cannot be talked to — a version set with no overlap, or a reserved
    /// error we cannot act on. Those are stated rather than swallowed: a server
    /// that silently contributes no tools looks exactly like a server that has
    /// none.
    async fn probe(&self, connection: &Connection, millis: u64) -> Result<Probe, String> {
        let version = MODERN_VERSIONS.first().copied().unwrap_or(LEGACY_VERSION);
        let params = json!({"_meta": self.meta(version)});

        match self
            .call_with_timeout(connection, "server/discover", params, millis)
            .await
        {
            Ok(result) => {
                let offered = versions_in(result.get("supportedVersions"));
                match shared_era(&offered) {
                    Some(Era::Modern(version)) => Ok(Probe::Modern(version)),
                    Some(Era::Legacy) => Ok(Probe::Legacy {
                        connection_usable: true,
                    }),
                    None => Err(self.no_shared_version(&offered)),
                }
            }
            Err(RpcError::Peer {
                code,
                message,
                data,
                ..
            }) if is_modern_error(code) => {
                let offered = versions_in(data.as_ref().and_then(|data| data.get("supported")));
                if offered.is_empty() {
                    // A reserved code that is not a version mismatch — a missing
                    // client capability, say. The server is modern and has told
                    // us why it will not serve us; guessing past that would only
                    // produce a second, less clear failure.
                    return Err(format!(
                        "MCP server `{}` from plugin `{}` refused a {version} request: {message}",
                        self.name, self.plugin
                    ));
                }
                match shared_era(&offered) {
                    Some(Era::Modern(version)) => Ok(Probe::Modern(version)),
                    Some(Era::Legacy) => Ok(Probe::Legacy {
                        connection_usable: true,
                    }),
                    None => Err(self.no_shared_version(&offered)),
                }
            }
            // Anything else means the peer did not understand a modern request:
            // an implementation-defined error, a timeout, or a closed pipe. The
            // spec forbids keying this on one code, so nothing here does.
            Err(other) => Ok(Probe::Legacy {
                connection_usable: !matches!(
                    other,
                    RpcError::Closed { .. } | RpcError::Transport { .. }
                ),
            }),
        }
    }

    fn no_shared_version(&self, offered: &[String]) -> String {
        let mut ours: Vec<&str> = MODERN_VERSIONS.to_vec();
        ours.push(LEGACY_VERSION);
        format!(
            "MCP server `{}` from plugin `{}` shares no protocol version with keke: it speaks {}, keke speaks {}",
            self.name,
            self.plugin,
            if offered.is_empty() {
                "nothing it would name".to_string()
            } else {
                offered.join(", ")
            },
            ours.join(", ")
        )
    }

    /// The legacy opening handshake.
    async fn initialize(&self, connection: &Connection, millis: u64) -> Result<(), String> {
        let handshake = self
            .call_with_timeout(
                connection,
                "initialize",
                json!({
                    "protocolVersion": LEGACY_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "keke", "version": env!("CARGO_PKG_VERSION")},
                }),
                millis,
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
        Ok(())
    }

    /// The `_meta` every modern request must carry.
    ///
    /// `protocolVersion` and `clientCapabilities` are required on every request
    /// — a server MUST reject a request missing either — and `clientInfo` is
    /// merely recommended, so all three go on every one. Capabilities are empty
    /// because keke offers none to a server: it does not do sampling,
    /// elicitation, or roots.
    fn meta(&self, version: &str) -> Value {
        json!({
            META_VERSION: version,
            META_CLIENT_CAPABILITIES: {},
            META_CLIENT_INFO: {"name": "keke", "version": env!("CARGO_PKG_VERSION")},
        })
    }

    /// Send `method` the way this server's era expects it.
    async fn request(
        &self,
        session: &Session,
        method: &str,
        mut params: Value,
        millis: u64,
    ) -> Result<Value, String> {
        if let Era::Modern(version) = &session.era
            && let Some(object) = params.as_object_mut()
        {
            object.insert("_meta".to_string(), self.meta(version));
        }
        self.call_with_timeout(&session.connection, method, params, millis)
            .await
            .map_err(|error| error.to_string())
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
    use super::Era;
    use super::LEGACY_VERSION;
    use super::MODERN_VERSIONS;
    use super::expand;
    use super::is_modern_error;
    use super::shared_era;

    fn offered(versions: &[&str]) -> Vec<String> {
        versions.iter().map(|v| (*v).to_string()).collect()
    }

    #[test]
    fn only_the_range_the_spec_reserved_identifies_a_modern_peer() {
        // The codes a legacy server actually answers an unknown method with.
        assert!(!is_modern_error(-32601));
        assert!(!is_modern_error(-32602));
        // The reserved sub-range, of which `UnsupportedProtocolVersion` is one.
        assert!(is_modern_error(-32022));
        assert!(is_modern_error(-32020));
        assert!(is_modern_error(-32099));
        // The legacy sub-range is not reserved and proves nothing.
        assert!(!is_modern_error(-32002));
    }

    #[test]
    fn the_newest_shared_revision_wins_over_an_older_one() {
        let both = offered(&[LEGACY_VERSION, MODERN_VERSIONS[0]]);
        assert_eq!(
            shared_era(&both),
            Some(Era::Modern(MODERN_VERSIONS[0].to_string()))
        );
    }

    #[test]
    fn a_server_offering_only_the_legacy_revision_gets_the_handshake() {
        assert_eq!(shared_era(&offered(&[LEGACY_VERSION])), Some(Era::Legacy));
    }

    #[test]
    fn no_shared_revision_is_none_rather_than_a_guess() {
        assert_eq!(shared_era(&offered(&["1999-01-01"])), None);
        assert_eq!(shared_era(&[]), None);
    }

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
