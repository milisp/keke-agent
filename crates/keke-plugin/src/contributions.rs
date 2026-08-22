//! The four things a data-plugin can contribute, in the ecosystem's own file
//! formats.
//!
//! Each parser here is total and inert: it reads a file and produces a value.
//! Nothing spawns, nothing reaches a model. That is what lets a surface list an
//! untrusted plugin's contents safely.

use keke_paths::AbsPath;
use serde::Deserialize;

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

/// An MCP server, from `.mcp.json` or an inline `mcpServers` block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedMcpServer {
    pub plugin: String,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// Environment for the child. Values come from the manifest as written;
    /// `${VAR}` references are expanded from the host environment at spawn time
    /// rather than here, so a resolved set never holds a secret.
    pub env: Vec<(String, String)>,
    pub plugin_root: AbsPath,
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
#[derive(Debug, Default, Deserialize)]
pub(crate) struct McpFile {
    #[serde(default, rename = "mcpServers")]
    pub mcp_servers: std::collections::BTreeMap<String, McpServerEntry>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct McpServerEntry {
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
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
