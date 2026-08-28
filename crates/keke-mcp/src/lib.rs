//! Plugin-contributed MCP servers, exposed as ordinary keke tools.
//!
//! `keke-plugin` resolves what servers a plugin declares and stops there —
//! resolution runs nothing. This crate is the part that runs them: it spawns
//! each server on first use, asks it what tools it has, and wraps each one in
//! [`keke_tool::Tool`] so the engine never learns that MCP exists.
//!
//! The transport is a small newline-delimited JSON-RPC client written here
//! rather than a general MCP library. keke needs `initialize`, `tools/list` and
//! `tools/call`; the rest of the protocol has nowhere to go in this engine, and
//! a dependency for it would be a dependency to keep in step forever.

mod auth;
mod backend;
mod client;
mod http;
mod server;
mod tool;

pub use auth::AuthHome;
pub use auth::ServerAuth;
pub use tool::McpArgs;
pub use tool::McpToolOutput;

use std::sync::Arc;
use std::sync::OnceLock;

use keke_plugin::PluginSet;
use keke_plugin_api::ExtensionContext;
use keke_plugin_api::ExtensionRegistryBuilder;
use keke_plugin_api::ToolContributor;
use keke_tool::ArcTool;

use keke_config_types::PluginTimeouts;

use backend::backend;
use server::McpServer;
use tool::McpTool;

/// Budgets for talking to a server.
///
/// Named and passed in rather than hidden as `DEFAULT_*` constants
/// (`AGENTS.md` invariant 9): both numbers are things a deployment can
/// reasonably want to change — a server that shells out to a package manager
/// needs a longer call budget than one that reads a file.
#[derive(Clone, Debug)]
pub struct McpOptions {
    /// Where a remote server's OAuth tokens are kept. `None` disables signing
    /// in — a host with no state directory has nowhere to put a token, and
    /// holding one in memory for one process would ask the person to sign in
    /// again every run.
    pub auth: Option<AuthHome>,
    /// How long a server has to answer `initialize` and `tools/list`. A server
    /// that overruns this contributes no tools rather than stalling the session.
    pub startup_timeout_millis: u64,
    /// The ceiling for a single `tools/call`, also advertised to the engine as
    /// the tool's timeout so there is one budget rather than two.
    pub call_timeout_millis: u64,
}

/// The default budgets are the configured ones, never zero: a derived
/// `Default` here would hand every server a 0ms deadline, which reads as every
/// server being broken.
impl Default for McpOptions {
    fn default() -> Self {
        Self::from(PluginTimeouts::default())
    }
}

/// The configured budgets, narrowed to the two this crate spends.
impl From<PluginTimeouts> for McpOptions {
    fn from(timeouts: PluginTimeouts) -> Self {
        Self {
            startup_timeout_millis: timeouts.mcp_startup_millis,
            call_timeout_millis: timeouts.mcp_call_millis,
            auth: None,
        }
    }
}

/// Every MCP server in a plugin set, as a tool contributor.
struct McpTools {
    servers: Vec<Arc<McpServer>>,
    options: McpOptions,
    /// Discovery happens once and its result is reused for the process, because
    /// it is also what opens the connections the calls then share.
    listed: OnceLock<Vec<ArcTool>>,
}

impl McpTools {
    /// Start every server and collect what each one advertises.
    ///
    /// A server that will not start, will not initialize, or will not answer in
    /// time contributes nothing and is logged. It cannot fail the session, and
    /// it cannot cost another server its tools — which is why the failures are
    /// collected per server here instead of with `?`.
    fn discover(&self) -> Vec<ArcTool> {
        let Some(backend) = backend() else {
            tracing::error!("no runtime for MCP servers; none will be available");
            return Vec::new();
        };

        let servers = self.servers.clone();
        let call_timeout = self.options.call_timeout_millis;
        let (tx, rx) = std::sync::mpsc::channel();

        backend.spawn(async move {
            let listings = futures::future::join_all(
                servers
                    .iter()
                    .map(|server| async move { (server, server.list_tools().await) }),
            )
            .await;

            let mut tools: Vec<ArcTool> = Vec::new();
            for (server, listing) in listings {
                match listing {
                    Ok(remote) => {
                        for entry in remote {
                            tools.push(Arc::new(McpTool::new(server, entry, call_timeout)));
                        }
                    }
                    Err(reason) => tracing::warn!(
                        plugin = server.plugin(),
                        server = server.name(),
                        %reason,
                        "MCP server unavailable; its tools are not listed"
                    ),
                }
            }
            let _ = tx.send(tools);
        });

        // Blocks the caller, not the backend: `tools` is synchronous and the
        // work is on a runtime of our own, so this waits without re-entering or
        // stalling the session's runtime.
        rx.recv().unwrap_or_default()
    }
}

impl ToolContributor for McpTools {
    fn tools(&self, _ctx: &ExtensionContext) -> Vec<ArcTool> {
        self.listed.get_or_init(|| self.discover()).clone()
    }
}

/// Register every MCP server the plugin set declares, with default budgets.
pub fn install(registry: &mut ExtensionRegistryBuilder, plugins: &PluginSet) {
    install_with(registry, plugins, McpOptions::default());
}

/// Register every MCP server the plugin set declares.
///
/// Registration is inert: nothing is spawned here. The servers start the first
/// time a session asks what tools exist, and each one starts at most once for
/// the life of the process however many calls share it.
pub fn install_with(
    registry: &mut ExtensionRegistryBuilder,
    plugins: &PluginSet,
    options: McpOptions,
) {
    let servers: Vec<Arc<McpServer>> = plugins
        .mcp_servers()
        .map(|resolved| Arc::new(McpServer::new(resolved, options.clone())))
        .collect();

    if servers.is_empty() {
        return;
    }

    registry.tool_contributor(Arc::new(McpTools {
        servers,
        options,
        listed: OnceLock::new(),
    }));
}
