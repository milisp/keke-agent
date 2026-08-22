//! `plugin.json`, in the layout the Claude Code plugin ecosystem already uses.
//!
//! keke did not design this format. Adopting it is the whole point: a plugin
//! ecosystem's value is the plugins that exist on day one, and inventing a
//! nicer manifest would have bought a cleaner schema in exchange for an empty
//! catalog. xAI's grok-build reached the same conclusion and reads the same
//! files.
//!
//! **Forward-compatibility is deliberate and asymmetric.** Unknown *metadata*
//! is ignored, because a manifest written for a newer host must still load.
//! Unknown *contribution* kinds are not ignored — see
//! [`ManifestContributions::unsupported`]. The difference matters: ignoring a
//! stray `homepage` costs nothing, while ignoring a `lspServers` block means an
//! author believes a capability is active when this host has never heard of it.

use serde::Deserialize;
use serde::Serialize;

/// Manifest locations, in priority order. The first that exists wins.
///
/// A plugin authored for Claude Code needs no keke-specific file; a plugin that
/// wants to differ between hosts can add `.keke-plugin/plugin.json` without
/// disturbing its Claude manifest.
pub const MANIFEST_PATHS: &[&str] = &[
    "plugin.json",
    ".keke-plugin/plugin.json",
    ".claude-plugin/plugin.json",
];

/// Contribution kinds keke understands. Anything else found in a manifest is
/// recorded as unsupported rather than dropped.
pub const KNOWN_CONTRIBUTIONS: &[&str] = &["skills", "commands", "hooks", "mcpServers"];

/// Convention paths used when a manifest omits a contribution, and when a
/// package has no manifest at all.
pub const SKILLS_DIR: &str = "skills";
pub const COMMANDS_DIR: &str = "commands";
pub const MCP_FILE: &str = ".mcp.json";
pub const HOOKS_FILE: &str = "hooks/hooks.json";

/// A parsed `plugin.json`.
///
/// Note the absence of `deny_unknown_fields`: that is what "forward-compatible"
/// means here, and the unsupported-contribution check below is what keeps it
/// from becoming silent.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginManifest {
    /// Plugin namespace. Optional in the file — a package with no manifest, or
    /// a manifest without a name, takes its name from the directory.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub author: Option<Author>,

    #[serde(default)]
    pub skills: Option<PathOrPaths>,
    #[serde(default)]
    pub commands: Option<PathOrPaths>,
    #[serde(default)]
    pub hooks: Option<PathOrInline>,
    #[serde(default)]
    pub mcp_servers: Option<PathOrInline>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Author {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// One path or several. The ecosystem's manifests use both spellings.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum PathOrPaths {
    Single(String),
    Multiple(Vec<String>),
}

impl PathOrPaths {
    #[must_use]
    pub fn as_slice(&self) -> &[String] {
        match self {
            Self::Single(one) => std::slice::from_ref(one),
            Self::Multiple(many) => many,
        }
    }
}

/// A file path, or the configuration inlined into the manifest.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum PathOrInline {
    Path(String),
    Inline(serde_json::Value),
}

/// Contribution keys present in a manifest that this host does not implement.
///
/// Surfaced rather than dropped so `keke plugin list` can tell the person that
/// part of a plugin is inert here. Silently ignoring these is how an author
/// ends up believing a hook or a sandbox restriction is in force when nothing
/// is running it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ManifestContributions {
    pub unsupported: Vec<String>,
}

impl ManifestContributions {
    /// Scan the raw manifest object for contribution-shaped keys keke does not
    /// implement. Metadata keys are not contributions and are not reported.
    #[must_use]
    pub fn scan(raw: &serde_json::Value) -> Self {
        const METADATA: &[&str] = &[
            "name",
            "version",
            "description",
            "author",
            "homepage",
            "repository",
            "license",
            "keywords",
        ];

        let Some(object) = raw.as_object() else {
            return Self::default();
        };
        let mut unsupported: Vec<String> = object
            .keys()
            .filter(|key| {
                !METADATA.contains(&key.as_str()) && !KNOWN_CONTRIBUTIONS.contains(&key.as_str())
            })
            .cloned()
            .collect();
        unsupported.sort();
        Self { unsupported }
    }
}

/// Plugin names reach slash commands, tool namespaces, and log lines. Holding
/// them to the ecosystem's own kebab-case rule means no consumer downstream has
/// to re-decide what quoting an author's chosen name requires.
#[must_use]
pub fn is_valid_plugin_name(name: &str) -> bool {
    const MAX: usize = 64;
    !name.is_empty()
        && name.len() <= MAX
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}
