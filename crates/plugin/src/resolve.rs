//! Locating plugin packages and validating what they claim.
//!
//! Two properties this module exists to guarantee:
//!
//! - **Resolution activates nothing.** Paths are located and files are read;
//!   no hook runs, no MCP server spawns, no skill body reaches a model. Listing
//!   an untrusted plugin is therefore safe.
//! - **Every resource stays under its package root.** Containment is checked
//!   after canonicalization, because a `..` segment or a symlink out of the
//!   package passes a textual prefix test.

use std::collections::BTreeMap;
use std::path::Path;

use keke_paths::AbsPath;

use crate::contributions::HookEvent;
use crate::contributions::HooksFile;
use crate::contributions::McpFile;
use crate::contributions::ResolvedCommand;
use crate::contributions::ResolvedHook;
use crate::contributions::ResolvedMcpServer;
use crate::contributions::ResolvedSkill;
use crate::contributions::frontmatter;
use crate::manifest::COMMANDS_DIR;
use crate::manifest::HOOKS_FILE;
use crate::manifest::MANIFEST_PATHS;
use crate::manifest::MCP_FILE;
use crate::manifest::ManifestContributions;
use crate::manifest::PathOrInline;
use crate::manifest::PluginManifest;
use crate::manifest::SKILLS_DIR;
use crate::manifest::is_valid_plugin_name;

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("reading {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not valid JSON: {source}")]
    Json {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("plugin name {name:?} must be 1-64 lowercase letters, digits or '-'")]
    InvalidName { name: String },
    #[error("plugin {plugin}: {path} escapes the package root")]
    Escape { plugin: String, path: String },
    #[error("plugin {name:?} is installed twice in the same scope: {first} and {second}")]
    Duplicate {
        name: String,
        first: String,
        second: String,
    },
}

/// Where a plugin was found. Precedence, and eventually trust, follow from it.
///
/// A plugin inside the project directory is content the repository controls, so
/// it is not equivalent to one the person installed for themselves — the
/// distinction has to survive resolution for a trust decision to be possible
/// later.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PluginScope {
    /// `$KEKE_HOME/plugins/`, `~/.claude/plugins/`.
    User,
    /// `.keke/plugins/`, `.claude/plugins/` in the workspace.
    Project,
}

impl std::fmt::Display for PluginScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::User => "user",
            Self::Project => "project",
        })
    }
}

/// One installed plugin, fully validated and entirely inert.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedPlugin {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub scope: PluginScope,
    pub root: AbsPath,
    pub skills: Vec<ResolvedSkill>,
    pub commands: Vec<ResolvedCommand>,
    pub hooks: Vec<ResolvedHook>,
    pub mcp_servers: Vec<ResolvedMcpServer>,
    /// Contribution kinds this host does not implement, for a surface to report.
    pub unsupported: Vec<String>,
}

impl ResolvedPlugin {
    /// Hooks this plugin declared for events keke never runs.
    pub fn inert_hooks(&self) -> impl Iterator<Item = &ResolvedHook> {
        self.hooks.iter().filter(|hook| !hook.event.is_supported())
    }
}

/// Every installed plugin, composed with precedence applied.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginSet {
    pub(crate) plugins: Vec<ResolvedPlugin>,
}

impl PluginSet {
    /// Compose across scopes.
    ///
    /// A plugin name found in both scopes is *not* ambiguity: the project copy
    /// wins, which is the documented precedence people expect from every other
    /// layered configuration in the harness. Two copies within one scope is
    /// ambiguity, and fails.
    ///
    /// Contributions never collide across plugins because they are namespaced
    /// by plugin — `acme:ship`, not `ship`. That is the ecosystem's own
    /// convention, and it removes a class of error rather than reporting it.
    pub fn compose(plugins: Vec<ResolvedPlugin>) -> Result<Self, PluginError> {
        let mut by_name: BTreeMap<String, ResolvedPlugin> = BTreeMap::new();
        for plugin in plugins {
            match by_name.get(&plugin.name) {
                Some(existing) if existing.scope == plugin.scope => {
                    return Err(PluginError::Duplicate {
                        name: plugin.name,
                        first: existing.root.to_string(),
                        second: plugin.root.to_string(),
                    });
                }
                Some(existing) if existing.scope > plugin.scope => continue,
                _ => {
                    by_name.insert(plugin.name.clone(), plugin);
                }
            }
        }
        Ok(Self {
            plugins: by_name.into_values().collect(),
        })
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    pub fn plugins(&self) -> impl Iterator<Item = &ResolvedPlugin> {
        self.plugins.iter()
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ResolvedPlugin> {
        self.plugins.iter().find(|plugin| plugin.name == name)
    }

    pub fn skills(&self) -> impl Iterator<Item = &ResolvedSkill> {
        self.plugins.iter().flat_map(|p| p.skills.iter())
    }

    pub fn commands(&self) -> impl Iterator<Item = &ResolvedCommand> {
        self.plugins.iter().flat_map(|p| p.commands.iter())
    }

    pub fn mcp_servers(&self) -> impl Iterator<Item = &ResolvedMcpServer> {
        self.plugins.iter().flat_map(|p| p.mcp_servers.iter())
    }

    /// Hooks registered for `event`, in a stable order.
    pub fn hooks_for<'a>(&'a self, event: &'a HookEvent) -> impl Iterator<Item = &'a ResolvedHook> {
        self.plugins
            .iter()
            .flat_map(|p| p.hooks.iter())
            .filter(move |hook| &hook.event == event)
    }
}

/// Read every plugin package directly under `dir`.
///
/// A missing directory yields nothing rather than an error: having no plugins
/// installed is the normal case. A directory with neither a manifest nor any
/// convention content is skipped, so unrelated files under a plugin root do not
/// break startup — but a manifest that exists and is broken is always an error.
pub fn discover(dir: &AbsPath, scope: PluginScope) -> Result<Vec<ResolvedPlugin>, PluginError> {
    let entries = match std::fs::read_dir(dir.as_path()) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(PluginError::Read {
                path: dir.to_string(),
                source,
            });
        }
    };

    // Sorted, because directory order is not stable across filesystems and hook
    // order is observable behavior.
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| PluginError::Read {
            path: dir.to_string(),
            source,
        })?;
        let path = entry.path();
        if path.is_dir() && looks_like_a_plugin(&path) {
            roots.push(path);
        }
    }
    roots.sort();

    roots.iter().map(|root| load(root, scope)).collect()
}

/// Whether a directory carries a manifest or any convention content.
fn looks_like_a_plugin(root: &Path) -> bool {
    MANIFEST_PATHS.iter().any(|rel| root.join(rel).is_file())
        || root.join(SKILLS_DIR).is_dir()
        || root.join(COMMANDS_DIR).is_dir()
        || root.join(MCP_FILE).is_file()
        || root.join(HOOKS_FILE).is_file()
}

/// Read and validate a single plugin package.
///
/// A package with no manifest still loads, taking its name from the directory
/// and its contributions from the convention paths. That is how most published
/// plugins are actually shaped.
pub fn load(root: &Path, scope: PluginScope) -> Result<ResolvedPlugin, PluginError> {
    load_named(root, scope, None)
}

/// Read a package under a name of the caller's choosing.
///
/// `None` takes the name from the manifest or the directory, which is
/// [`load`]. A caller passes a name when the directory has none to give: a
/// person's own `~/.keke` is not called `.keke` to them, it is called `user`.
pub fn load_named(
    root: &Path,
    scope: PluginScope,
    name_override: Option<&str>,
) -> Result<ResolvedPlugin, PluginError> {
    let canonical = std::fs::canonicalize(root).map_err(|source| PluginError::Read {
        path: root.display().to_string(),
        source,
    })?;
    let root = AbsPath::new(&canonical).map_err(|_| PluginError::Escape {
        plugin: canonical.display().to_string(),
        path: canonical.display().to_string(),
    })?;

    let (manifest, unsupported) = read_manifest(&root)?;

    let name = name_override
        .map(str::to_string)
        .or_else(|| manifest.name.clone())
        .or_else(|| {
            root.as_path()
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    if !is_valid_plugin_name(&name) {
        return Err(PluginError::InvalidName { name });
    }

    let skills = read_skills(&root, &name, &manifest)?;
    let commands = read_commands(&root, &name, &manifest)?;
    let hooks = read_hooks(&root, &name, &manifest)?;
    let mcp_servers = read_mcp_servers(&root, &name, &manifest)?;

    Ok(ResolvedPlugin {
        name,
        version: manifest.version,
        description: manifest.description,
        scope,
        root,
        skills,
        commands,
        hooks,
        mcp_servers,
        unsupported,
    })
}

fn read_manifest(root: &AbsPath) -> Result<(PluginManifest, Vec<String>), PluginError> {
    for rel in MANIFEST_PATHS {
        let path = root.as_path().join(rel);
        if !path.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(|source| PluginError::Read {
            path: path.display().to_string(),
            source,
        })?;
        let raw: serde_json::Value =
            serde_json::from_str(&text).map_err(|source| PluginError::Json {
                path: path.display().to_string(),
                source,
            })?;
        let manifest: PluginManifest =
            serde_json::from_value(raw.clone()).map_err(|source| PluginError::Json {
                path: path.display().to_string(),
                source,
            })?;
        return Ok((manifest, ManifestContributions::scan(&raw).unsupported));
    }
    Ok((PluginManifest::default(), Vec::new()))
}

/// Directories a contribution may live in: whatever the manifest names, else
/// the convention directory.
fn source_dirs(
    root: &AbsPath,
    declared: Option<&crate::manifest::PathOrPaths>,
    fallback: &str,
) -> Vec<std::path::PathBuf> {
    match declared {
        Some(paths) => paths
            .as_slice()
            .iter()
            .map(|rel| root.as_path().join(rel))
            .collect(),
        None => vec![root.as_path().join(fallback)],
    }
}

fn read_skills(
    root: &AbsPath,
    plugin: &str,
    manifest: &PluginManifest,
) -> Result<Vec<ResolvedSkill>, PluginError> {
    let mut skills = Vec::new();
    for dir in source_dirs(root, manifest.skills.as_ref(), SKILLS_DIR) {
        for entry in sorted_entries(&dir)? {
            // The ecosystem shape is `skills/<name>/SKILL.md`; a bare
            // `skills/<name>.md` is accepted too, because plenty exist.
            let file = if entry.is_dir() {
                entry.join("SKILL.md")
            } else if entry.extension().is_some_and(|ext| ext == "md") {
                entry.clone()
            } else {
                continue;
            };
            if !file.is_file() {
                continue;
            }
            let path = contained(root, &file, plugin)?;
            let text =
                std::fs::read_to_string(path.as_path()).map_err(|source| PluginError::Read {
                    path: path.to_string(),
                    source,
                })?;
            let fields = frontmatter(&text);
            let name = fields
                .get("name")
                .cloned()
                .unwrap_or_else(|| stem(&entry).to_string());
            // A skill with no description cannot have its relevance judged
            // without loading the body, which defeats keeping it out of the
            // context window. Name it plainly rather than dropping it.
            let description = fields
                .get("description")
                .cloned()
                .unwrap_or_else(|| format!("{name} (no description)"));
            skills.push(ResolvedSkill {
                plugin: plugin.to_string(),
                name,
                description,
                path,
            });
        }
    }
    Ok(skills)
}

fn read_commands(
    root: &AbsPath,
    plugin: &str,
    manifest: &PluginManifest,
) -> Result<Vec<ResolvedCommand>, PluginError> {
    let mut commands = Vec::new();
    for dir in source_dirs(root, manifest.commands.as_ref(), COMMANDS_DIR) {
        for entry in sorted_entries(&dir)? {
            if entry.extension().is_none_or(|ext| ext != "md") {
                continue;
            }
            let path = contained(root, &entry, plugin)?;
            let text =
                std::fs::read_to_string(path.as_path()).map_err(|source| PluginError::Read {
                    path: path.to_string(),
                    source,
                })?;
            let fields = frontmatter(&text);
            let name = stem(&entry).to_string();
            let description = fields.get("description").cloned().unwrap_or_default();
            commands.push(ResolvedCommand {
                plugin: plugin.to_string(),
                name,
                description,
                path,
            });
        }
    }
    Ok(commands)
}

fn read_hooks(
    root: &AbsPath,
    plugin: &str,
    manifest: &PluginManifest,
) -> Result<Vec<ResolvedHook>, PluginError> {
    let file: HooksFile = match read_component(root, manifest.hooks.as_ref(), HOOKS_FILE, plugin)? {
        Some(value) => serde_json::from_value(value).unwrap_or_default(),
        None => return Ok(Vec::new()),
    };

    let mut hooks = Vec::new();
    for (event, matchers) in file.hooks {
        let event = HookEvent::parse(&event);
        for matcher in matchers {
            for command in matcher.hooks {
                // `command` is the only type the ecosystem defines. Guessing at
                // an unrecognized type would mean running something under a
                // contract keke does not know.
                if command.kind != "command" || command.command.is_empty() {
                    continue;
                }
                hooks.push(ResolvedHook {
                    plugin: plugin.to_string(),
                    event: event.clone(),
                    command: command.command,
                    matcher: matcher.matcher.clone(),
                    plugin_root: root.clone(),
                    timeout_seconds: command.timeout,
                });
            }
        }
    }
    Ok(hooks)
}

/// The servers a parsed `.mcp.json` names, dropping entries that name nothing
/// reachable.
pub(crate) fn servers_in(file: McpFile, plugin: &str, root: &AbsPath) -> Vec<ResolvedMcpServer> {
    file.mcp_servers
        .into_iter()
        .filter_map(|(name, entry)| {
            Some(ResolvedMcpServer {
                plugin: plugin.to_string(),
                name,
                transport: entry.transport()?,
                plugin_root: root.clone(),
            })
        })
        .collect()
}

fn read_mcp_servers(
    root: &AbsPath,
    plugin: &str,
    manifest: &PluginManifest,
) -> Result<Vec<ResolvedMcpServer>, PluginError> {
    let value = match read_component(root, manifest.mcp_servers.as_ref(), MCP_FILE, plugin)? {
        Some(value) => value,
        None => return Ok(Vec::new()),
    };
    // An inline `mcpServers` block is the map itself; the file wraps it in one.
    let file: McpFile = if value.get("mcpServers").is_some() {
        serde_json::from_value(value).unwrap_or_default()
    } else {
        McpFile {
            mcp_servers: serde_json::from_value(value).unwrap_or_default(),
        }
    };

    Ok(servers_in(file, plugin, root))
}

/// Read a component that is either inlined in the manifest, named by it, or
/// found at its convention path.
fn read_component(
    root: &AbsPath,
    declared: Option<&PathOrInline>,
    fallback: &str,
    plugin: &str,
) -> Result<Option<serde_json::Value>, PluginError> {
    let path = match declared {
        Some(PathOrInline::Inline(value)) => return Ok(Some(value.clone())),
        Some(PathOrInline::Path(rel)) => root.as_path().join(rel),
        None => root.as_path().join(fallback),
    };
    if !path.is_file() {
        return Ok(None);
    }
    let path = contained(root, &path, plugin)?;
    let text = std::fs::read_to_string(path.as_path()).map_err(|source| PluginError::Read {
        path: path.to_string(),
        source,
    })?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|source| PluginError::Json {
            path: path.to_string(),
            source,
        })
}

/// Canonicalize `path` and verify it stays inside `root`.
fn contained(root: &AbsPath, path: &Path, plugin: &str) -> Result<AbsPath, PluginError> {
    let resolved = std::fs::canonicalize(path).map_err(|source| PluginError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let resolved = AbsPath::new(&resolved).map_err(|_| PluginError::Escape {
        plugin: plugin.to_string(),
        path: resolved.display().to_string(),
    })?;
    if resolved.is_contained_in(root) {
        Ok(resolved)
    } else {
        Err(PluginError::Escape {
            plugin: plugin.to_string(),
            path: path.display().to_string(),
        })
    }
}

/// Directory entries in a stable order; a missing directory yields none.
fn sorted_entries(dir: &Path) -> Result<Vec<std::path::PathBuf>, PluginError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(PluginError::Read {
                path: dir.display().to_string(),
                source,
            });
        }
    };
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    for entry in entries {
        paths.push(
            entry
                .map_err(|source| PluginError::Read {
                    path: dir.display().to_string(),
                    source,
                })?
                .path(),
        );
    }
    paths.sort();
    Ok(paths)
}

fn stem(path: &Path) -> &str {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
}
