//! Catalogs: one source that offers several plugins, and installs another
//! harness recorded.
//!
//! Both formats belong to the Claude Code ecosystem, and keke reads them as
//! published rather than defining its own. A marketplace is how a single
//! repository offers more than one plugin; `installed_plugins.json` is how the
//! other harness records what it installed, and reading it means a person who
//! already installed something there does not install it twice.
//!
//! Nothing here fetches. A marketplace is parsed from a directory that is
//! already on disk, which keeps resolution's "activates nothing" property
//! intact and keeps the network in the one crate allowed to reach it.

//! `marketplace.json`, and the install record the Claude Code CLI keeps.
//!
//! Neither format is keke's. A catalog is worth what it lists on the day it
//! ships, so keke reads the catalogs that already exist rather than asking
//! publishers to write a second file — the same bargain `manifest.rs` makes.

use std::collections::BTreeMap;
use std::path::Component;
use std::path::Path;

use keke_paths::AbsPath;
use serde::Deserialize;

use crate::resolve::PluginError;

/// Where a catalog entry's files come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntrySource {
    /// A directory inside the marketplace repository itself.
    Local { path: String },
    /// A git repository of its own.
    Git { url: String, reference: GitRef },
}

/// How firmly a git source is tied down.
///
/// The distinction is not cosmetic. Updating a [`GitRef::Pinned`] source cannot
/// change what runs without the pin changing too, and the change is visible in
/// the catalog. A [`GitRef::Moving`] source can become something else entirely
/// between one update and the next with nothing in the catalog to show for it,
/// which is precisely when a person needs to be asked again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitRef {
    /// A commit id: content-addressed.
    Pinned(String),
    /// A branch or tag: what it points at can change.
    Moving(String),
    /// Unstated — the remote's default branch, and therefore moving.
    Default,
}

impl GitRef {
    /// Whether what this points at can change without the catalog changing.
    #[must_use]
    pub fn can_move(&self) -> bool {
        !matches!(self, Self::Pinned(_))
    }
}

/// One plugin offered by a marketplace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketplaceEntry {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub source: EntrySource,
    pub homepage: Option<String>,
}

/// A parsed `marketplace.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Marketplace {
    pub name: String,
    pub description: Option<String>,
    pub owner: Option<String>,
    pub entries: Vec<MarketplaceEntry>,
    /// Names of entries dropped because nothing in them said where the files
    /// come from.
    ///
    /// One unusable entry does not make the catalog unusable, so it is not an
    /// error — but a plugin that silently is not in a list the person is
    /// reading is a plugin they will look for and not find, so the omission has
    /// to be something a surface can report.
    pub skipped: Vec<String>,
}

/// Manifest locations for a catalog, in the order they are tried.
pub const MARKETPLACE_PATHS: &[&str] = &[
    ".keke-plugin/marketplace.json",
    ".claude-plugin/marketplace.json",
    "marketplace.json",
];

impl Marketplace {
    /// Read the catalog in `root`, if there is one.
    ///
    /// `Ok(None)` means this directory is not a marketplace, which is the
    /// ordinary case for a repository holding a single plugin. A catalog that
    /// is present but unreadable is an error: the person named this source, and
    /// telling them it simply offers nothing would send them to look in the
    /// wrong place.
    pub fn load(root: &AbsPath) -> Result<Option<Self>, PluginError> {
        let Some(path) = MARKETPLACE_PATHS
            .iter()
            .map(|rel| root.as_path().join(rel))
            .find(|path| path.is_file())
        else {
            return Ok(None);
        };
        let text = std::fs::read_to_string(&path).map_err(|source| PluginError::Read {
            path: path.display().to_string(),
            source,
        })?;
        let raw: RawMarketplace =
            serde_json::from_str(&text).map_err(|source| PluginError::Json {
                path: path.display().to_string(),
                source,
            })?;

        let mut entries = Vec::new();
        let mut skipped = Vec::new();
        for entry in raw.plugins {
            match entry.source.as_ref().and_then(parse_source) {
                Some(source) => entries.push(MarketplaceEntry {
                    name: entry.name,
                    version: entry.version,
                    description: entry.description,
                    source,
                    homepage: entry.homepage,
                }),
                None => skipped.push(entry.name),
            }
        }

        Ok(Some(Self {
            name: raw.name,
            description: raw.description,
            owner: raw.owner.and_then(|owner| owner.name),
            entries,
            skipped,
        }))
    }

    /// The entry named `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&MarketplaceEntry> {
        self.entries.iter().find(|entry| entry.name == name)
    }
}

/// An install another harness recorded, as read from `installed_plugins.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignInstall {
    pub name: String,
    /// The marketplace half of the record's `name@marketplace` key.
    pub marketplace: Option<String>,
    pub install_path: AbsPath,
}

/// Read `installed_plugins.json`, keeping only what is visible from `cwd`.
///
/// Entries scoped to a project are recorded with the project they belong to,
/// and must stay invisible outside it: surfacing another project's plugins here
/// would attach one repository's contributions to a different repository's
/// session. An entry that claims a project but names none cannot prove it is in
/// one, so it stays hidden.
///
/// A missing file yields nothing — most people have never used the other
/// harness. A malformed one also yields nothing rather than failing the
/// session: this file belongs to another program, and keke refusing to start
/// because a foreign file is corrupt would be keke's bug to the person.
pub fn foreign_installs(path: &Path, cwd: &AbsPath) -> Vec<ForeignInstall> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(record) = serde_json::from_str::<RawInstalledPlugins>(&text) else {
        return Vec::new();
    };

    let cwd = canonical(cwd.as_path());
    let mut installs = Vec::new();
    for (key, entries) in record.plugins {
        // The marketplace half is optional: a plugin installed from a path has
        // no marketplace to name.
        let (name, marketplace) = match key.split_once('@') {
            Some((name, marketplace)) => (name.to_string(), Some(marketplace.to_string())),
            None => (key.clone(), None),
        };
        for entry in entries {
            if !visible_from(&entry, &cwd) {
                continue;
            }
            let Ok(install_path) = AbsPath::new(&entry.install_path) else {
                continue;
            };
            if !install_path.as_path().exists() {
                continue;
            }
            installs.push(ForeignInstall {
                name: name.clone(),
                marketplace: marketplace.clone(),
                install_path,
            });
        }
    }
    installs
}

/// Whether an install record applies to a session running in `cwd`.
///
/// An entry is tied to a project when it says so by scope, or when it names a
/// project at all — an entry recorded with a `projectPath` is a record of that
/// project's choice whatever its scope claims. A tied entry that names no
/// project cannot show it belongs to this one, and the safe reading of an
/// unprovable claim is to withhold it.
fn visible_from(entry: &RawInstall, cwd: &Path) -> bool {
    let project = entry
        .project_path
        .as_deref()
        .filter(|project| !project.as_os_str().is_empty());
    let tied = matches!(entry.scope.as_deref(), Some("local" | "project")) || project.is_some();
    if !tied {
        return true;
    }
    let Some(project) = project else {
        return false;
    };
    // Component-wise, so that `/a/bc` is not read as living inside `/a/b`.
    cwd.starts_with(canonical(project))
}

/// The real location of `path`, falling back to a lexical cleanup when it does
/// not exist — a recorded project may be checked out on another machine, and
/// that is not a reason to compare raw strings.
fn canonical(path: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        path.components()
            .filter(|component| !matches!(component, Component::CurDir))
            .collect()
    })
}

/// Read `source` in each of the four spellings the ecosystem's catalogs use.
///
/// `sha` outranks `ref` when both appear: the commit is the more specific of
/// the two statements, and taking the branch instead would turn a source the
/// publisher pinned into one that can move under the person.
fn parse_source(source: &serde_json::Value) -> Option<EntrySource> {
    if let Some(path) = source.as_str() {
        return Some(EntrySource::Local {
            path: path.to_string(),
        });
    }
    let object = source.as_object()?;
    let text = |key: &str| {
        object
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };

    if let Some(url) = text("url") {
        let reference = match (text("sha"), text("ref")) {
            (Some(sha), _) => GitRef::Pinned(sha),
            (None, Some(reference)) => GitRef::Moving(reference),
            (None, None) => GitRef::Default,
        };
        return Some(EntrySource::Git { url, reference });
    }
    text("path").map(|path| EntrySource::Local { path })
}

/// The catalog as written. Unknown keys are ignored here for the same reason
/// they are in a manifest: a catalog authored for a newer host must still load.
#[derive(Deserialize)]
struct RawMarketplace {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    owner: Option<RawOwner>,
    #[serde(default)]
    plugins: Vec<RawEntry>,
}

#[derive(Deserialize)]
struct RawOwner {
    #[serde(default)]
    name: Option<String>,
}

/// `source` stays a [`serde_json::Value`] so that a shape this host does not
/// understand costs one entry rather than the whole catalog.
#[derive(Deserialize)]
struct RawEntry {
    name: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    source: Option<serde_json::Value>,
    #[serde(default)]
    homepage: Option<String>,
}

#[derive(Deserialize)]
struct RawInstalledPlugins {
    #[serde(default)]
    plugins: BTreeMap<String, Vec<RawInstall>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawInstall {
    install_path: std::path::PathBuf,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    project_path: Option<std::path::PathBuf>,
}
