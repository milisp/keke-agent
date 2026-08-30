//! The four things a data-plugin can contribute, in the ecosystem's own file
//! formats.
//!
//! Each parser here is total and inert: it reads a file and produces a value.
//! Nothing spawns, nothing reaches a model. That is what lets a surface list an
//! untrusted plugin's contents safely.

use keke_paths::AbsPath;
use serde::Deserialize;
use serde::Serialize;

/// A prompt fragment, from `skills/<name>/SKILL.md`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedSkill {
    /// Owning plugin, kept so a name collision names the culprit.
    pub plugin: String,
    pub name: String,
    /// Relevance summary from the frontmatter. This is the only part loaded
    /// into the context window up front — the body is read when the skill is
    /// actually used, which is the entire reason the description is required.
    pub description: String,
    pub path: AbsPath,
}

/// A slash command, from `commands/<name>.md`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedCommand {
    pub plugin: String,
    pub name: String,
    pub description: String,
    pub path: AbsPath,
}

/// Lifecycle points keke runs hooks at, named as the ecosystem names them.
///
/// There is no event that can *allow* a tool call. `PreToolUse` may deny and
/// nothing more, which is what keeps denial monotonic (`AGENTS.md` invariant 7)
/// even when a person installs a plugin specifically to loosen a restriction.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    /// Runs before the tool body. A non-zero exit denies the call.
    PreToolUse,
    PostToolUse,
    Stop,
    /// An event this host does not implement.
    ///
    /// Retained rather than filtered out: a plugin declaring a hook keke never
    /// runs is something the person should be told about, not something that
    /// should quietly do nothing.
    Unsupported(String),
}

impl HookEvent {
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw {
            "SessionStart" => Self::SessionStart,
            "UserPromptSubmit" => Self::UserPromptSubmit,
            "PreToolUse" => Self::PreToolUse,
            "PostToolUse" => Self::PostToolUse,
            "Stop" => Self::Stop,
            other => Self::Unsupported(other.to_string()),
        }
    }

    #[must_use]
    pub fn is_supported(&self) -> bool {
        !matches!(self, Self::Unsupported(_))
    }
}

/// Prints the event under the name a plugin author wrote, which is the name
/// they will search their own manifest for.
impl std::fmt::Display for HookEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::Stop => "Stop",
            Self::Unsupported(name) => name,
        })
    }
}

/// A hook program bound to a lifecycle point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedHook {
    pub plugin: String,
    pub event: HookEvent,
    /// The command line as written. Unlike skills and commands this is not
    /// resolved to a path: the ecosystem's hooks are shell commands, often
    /// referring to the plugin root through `${CLAUDE_PLUGIN_ROOT}`.
    pub command: String,
    /// Tool-name pattern. Empty means every tool, so a hook whose author forgot
    /// to filter observes everything — the safe direction for an audit hook.
    pub matcher: String,
    /// Substituted for `${CLAUDE_PLUGIN_ROOT}` / `${KEKE_PLUGIN_ROOT}` when the
    /// hook runs, so a hook can find its own files without hardcoding a path.
    pub plugin_root: AbsPath,
    pub timeout_seconds: Option<u64>,
}

impl ResolvedHook {
    /// Whether this hook applies to `tool`.
    ///
    /// The matcher is the ecosystem's simple form: empty or `*` means all, and
    /// `|` separates alternatives. Deliberately not a regex — a plugin should
    /// not be able to hang the turn loop with a pathological pattern.
    #[must_use]
    pub fn matches(&self, tool: &str) -> bool {
        let matcher = self.matcher.trim();
        matcher.is_empty() || matcher == "*" || matcher.split('|').any(|part| part.trim() == tool)
    }
}

/// How a server is reached.
///
/// One enum rather than a struct with optional fields, because the two shapes
/// share nothing: a stdio server has no URL to talk to and a remote one has no
/// child to spawn. A struct carrying both would let a caller act on the pair
/// that never occurs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpTransport {
    /// A child process, spoken to over its stdin and stdout.
    Stdio {
        command: String,
        args: Vec<String>,
        /// Environment for the child. Values come from the manifest as written;
        /// `${VAR}` references are expanded from the host environment at spawn
        /// time rather than here, so a resolved set never holds a secret.
        env: Vec<(String, String)>,
    },
    /// A remote endpoint, spoken to with the streamable HTTP transport.
    Http {
        url: String,
        /// Headers sent with every request. `${VAR}` references are expanded at
        /// request time for the same reason as `Stdio::env`, so an
        /// `Authorization` written as `Bearer ${TOKEN}` never resolves here.
        headers: Vec<(String, String)>,
    },
    /// A remote endpoint using the older HTTP+SSE transport: a long-lived `GET`
    /// stream that names the URL replies are posted to.
    Sse {
        url: String,
        headers: Vec<(String, String)>,
    },
}

impl McpTransport {
    /// The name the `.mcp.json` `type` field uses for this transport.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Stdio { .. } => "stdio",
            Self::Http { .. } => "http",
            Self::Sse { .. } => "sse",
        }
    }

    /// Whether reaching this server means starting a program on this machine.
    #[must_use]
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Stdio { .. })
    }

    /// One readable line saying what talking to this server does.
    ///
    /// This is what a person is asked to approve, so it names everything that
    /// decides what happens: the whole command line for a child process, the
    /// URL for a remote one. Environment variables and headers are listed by
    /// name only — the names are part of what the server is handed, the values
    /// are secrets and belong in no file keke writes.
    #[must_use]
    pub fn describe(&self) -> String {
        let named = |pairs: &[(String, String)], label: &str| {
            if pairs.is_empty() {
                return String::new();
            }
            let names: Vec<&str> = pairs.iter().map(|(name, _)| name.as_str()).collect();
            format!(" ({label}: {})", names.join(", "))
        };
        match self {
            Self::Stdio { command, args, env } => {
                let mut line = command.clone();
                for arg in args {
                    line.push(' ');
                    line.push_str(arg);
                }
                line.push_str(&named(env, "env"));
                line
            }
            Self::Http { url, headers } => format!("http {url}{}", named(headers, "headers")),
            Self::Sse { url, headers } => format!("sse {url}{}", named(headers, "headers")),
        }
    }
}

/// An MCP server, from `.mcp.json` or an inline `mcpServers` block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedMcpServer {
    pub plugin: String,
    pub name: String,
    pub transport: McpTransport,
    pub plugin_root: AbsPath,
    /// Set by `keke mcp disable`, or the `/mcp` overlay's space key. A
    /// disabled server is still listed — that is the whole point, since a
    /// server nobody can see again cannot be re-enabled — it is just never
    /// started.
    pub disabled: bool,
}

// ---------------------------------------------------------------------------
// File formats
// ---------------------------------------------------------------------------

/// `hooks/hooks.json`: `{"hooks": {"<Event>": [{"matcher": .., "hooks": [..]}]}}`
#[derive(Debug, Default, Deserialize)]
pub(crate) struct HooksFile {
    #[serde(default)]
    pub hooks: std::collections::BTreeMap<String, Vec<HookMatcher>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct HookMatcher {
    #[serde(default)]
    pub matcher: String,
    #[serde(default)]
    pub hooks: Vec<HookCommand>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct HookCommand {
    /// The ecosystem's only type today is `command`. An unrecognized type is
    /// skipped rather than guessed at.
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub timeout: Option<u64>,
}

/// `.mcp.json`: `{"mcpServers": {"<name>": {"command": .., "args": [..]}}}`
///
/// Public because it is also the file `keke mcp add` writes. Reading and
/// writing go through the one type so a hand-edited file and a generated one
/// are the same file.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct McpFile {
    #[serde(default, rename = "mcpServers")]
    pub mcp_servers: std::collections::BTreeMap<String, McpServerEntry>,
}

/// One server as written in the file.
///
/// Every field is optional, because which ones apply depends on `type` — the
/// entry is the on-disk shape, and [`McpTransport`] is what it means. Empty
/// collections are omitted on write so a stdio entry does not grow an empty
/// `headers` and a remote one an empty `args`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct McpServerEntry {
    /// `stdio`, `http`, or `sse`. Absent means stdio, which is what every
    /// entry written before remote transports existed is.
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub headers: std::collections::BTreeMap<String, String>,
    /// Kept configured but not started. Absent (rather than `false`) is the
    /// overwhelmingly common case, so it is left out of a written entry
    /// rather than spelled out every time.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
}

impl McpServerEntry {
    /// What this entry means, or `None` when it names nothing reachable.
    ///
    /// A stdio entry with no command and a remote one with no URL are both
    /// unusable, and are dropped here rather than turned into a server that
    /// fails on first contact.
    #[must_use]
    pub fn transport(&self) -> Option<McpTransport> {
        let headers = || self.headers.clone().into_iter().collect();
        match self.kind.as_deref().unwrap_or("stdio") {
            "http" if !self.url.is_empty() => Some(McpTransport::Http {
                url: self.url.clone(),
                headers: headers(),
            }),
            "sse" if !self.url.is_empty() => Some(McpTransport::Sse {
                url: self.url.clone(),
                headers: headers(),
            }),
            "stdio" if !self.command.is_empty() => Some(McpTransport::Stdio {
                command: self.command.clone(),
                args: self.args.clone(),
                env: self.env.clone().into_iter().collect(),
            }),
            _ => None,
        }
    }
}

impl From<McpTransport> for McpServerEntry {
    fn from(transport: McpTransport) -> Self {
        match transport {
            McpTransport::Stdio { command, args, env } => Self {
                kind: Some("stdio".to_string()),
                command,
                args,
                env: env.into_iter().collect(),
                ..Self::default()
            },
            McpTransport::Http { url, headers } => Self {
                kind: Some("http".to_string()),
                url,
                headers: headers.into_iter().collect(),
                ..Self::default()
            },
            McpTransport::Sse { url, headers } => Self {
                kind: Some("sse".to_string()),
                url,
                headers: headers.into_iter().collect(),
                ..Self::default()
            },
        }
    }
}

/// Split YAML frontmatter from a markdown body, returning `(name, description)`.
///
/// A hand-rolled two-key reader rather than a YAML dependency: the frontmatter
/// keke reads is two scalar strings, and taking on a YAML parser to read them
/// would put an arbitrary-document parser in front of untrusted plugin files
/// for no gain.
pub(crate) fn frontmatter(text: &str) -> std::collections::BTreeMap<String, String> {
    let mut fields = std::collections::BTreeMap::new();
    let Some(rest) = text.strip_prefix("---") else {
        return fields;
    };
    let Some(body_start) = rest.find("\n---") else {
        return fields;
    };
    for line in rest[..body_start].lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches(['"', '\'']).trim();
        if !value.is_empty() {
            fields.insert(key.trim().to_string(), value.to_string());
        }
    }
    fields
}
