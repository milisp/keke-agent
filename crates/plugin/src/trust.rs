//! Which plugins are allowed to run programs, and on whose say-so.
//!
//! A plugin under the workspace is content the repository controls. Cloning a
//! repository is not consent to run what it ships: without a gate here, a
//! `.claude/plugins/*/hooks/hooks.json` in someone else's project executes on
//! the first turn, with everything the agent process itself has.
//!
//! Three decisions shape this module:
//!
//! - **Only execution is gated.** Withholding trust removes hooks and MCP
//!   servers and leaves skills and commands in place. Those are text, and the
//!   harness already reads the repository's own `AGENTS.md` into the prompt
//!   without asking — gating repository text *here* would be a policy the rest
//!   of keke does not have, applied at the one place a person would not think
//!   to look for it.
//! - **Approval is of contents, not of a path.** A plugin that is trusted and
//!   then gains a hook is untrusted again. Otherwise saying yes once is a blank
//!   cheque on every future commit to that repository, which is the property an
//!   attacker would need.
//! - **What was approved is written down in words.** The store keeps the
//!   command lines themselves rather than a digest of them, so a person can
//!   read the file and see what they agreed to run.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;

use crate::resolve::PluginScope;
use crate::resolve::PluginSet;
use crate::resolve::ResolvedPlugin;

/// Where a plugin stands with respect to running its programs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trust {
    /// The plugin runs nothing, so there is nothing to decide.
    NothingToRun,
    /// Installed by the person into their own directory.
    OwnedByThePerson,
    /// Approved, and still contributing exactly what was approved.
    Approved,
    /// Never approved.
    NeverApproved,
    /// Approved earlier, but what it would run has changed since.
    ChangedSinceApproval,
}

impl Trust {
    /// Whether the plugin's programs may run.
    #[must_use]
    pub fn permits_running(self) -> bool {
        matches!(
            self,
            Self::NothingToRun | Self::OwnedByThePerson | Self::Approved
        )
    }
}

impl std::fmt::Display for Trust {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NothingToRun => "runs nothing",
            Self::OwnedByThePerson => "installed by you",
            Self::Approved => "trusted",
            Self::NeverApproved => "not trusted",
            Self::ChangedSinceApproval => "changed since you trusted it",
        })
    }
}

impl ResolvedPlugin {
    /// Every program this plugin would run, one readable line each.
    ///
    /// This is both what a person is asked to approve and what the approval is
    /// compared against later, so it has to name everything that determines
    /// what executes — a hook's event and matcher decide *when* it runs, and an
    /// MCP server's arguments are part of *what* it is.
    ///
    /// Environment forwarding is listed by variable name. The names are part of
    /// what the program is handed; the values are secrets and are neither read
    /// here nor written to the store.
    #[must_use]
    pub fn executables(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for hook in &self.hooks {
            let when = if hook.matcher.trim().is_empty() {
                "*".to_string()
            } else {
                hook.matcher.clone()
            };
            lines.push(format!("hook {} [{when}]: {}", hook.event, hook.command));
        }
        for server in &self.mcp_servers {
            lines.push(format!(
                "mcp {}: {}",
                server.name,
                server.transport.describe()
            ));
        }
        lines.sort();
        lines
    }
}

/// How a plugin came to be on this machine.
///
/// Recorded because it changes the trust verdict. A directory the person placed
/// themselves is something they at least looked at; a directory `keke plugin
/// add` fetched from a URL is something they named but never read. The second
/// does not get the first's benefit of the doubt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "kebab-case")]
pub enum InstallSource {
    /// Fetched from a git remote.
    Git {
        url: String,
        /// The ref as the person named it, for `update` to fetch again.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reference: Option<String>,
        /// Whether that ref can point somewhere else tomorrow. A pin cannot,
        /// which is the difference between an update that can surprise the
        /// person and one that cannot.
        moving: bool,
    },
    /// Copied from a directory on this machine.
    Path { path: String },
    /// An entry in a catalog, which is a git source plus where it was listed.
    Marketplace {
        url: String,
        catalog: String,
        entry: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reference: Option<String>,
        moving: bool,
    },
}

impl InstallSource {
    /// Whether fetching again can produce different contents.
    #[must_use]
    pub fn can_change_under_you(&self) -> bool {
        match self {
            Self::Git { moving, .. } | Self::Marketplace { moving, .. } => *moving,
            // A local directory is the person's own; they change it when they
            // change it.
            Self::Path { .. } => false,
        }
    }

    /// The remote to fetch from, when there is one.
    #[must_use]
    pub fn git_url(&self) -> Option<&str> {
        match self {
            Self::Git { url, .. } | Self::Marketplace { url, .. } => Some(url),
            Self::Path { .. } => None,
        }
    }

    /// The ref to fetch, when one was named.
    #[must_use]
    pub fn git_ref(&self) -> Option<&str> {
        match self {
            Self::Git { reference, .. } | Self::Marketplace { reference, .. } => {
                reference.as_deref()
            }
            Self::Path { .. } => None,
        }
    }
}

/// What keke knows and what the person decided about one plugin.
///
/// Both halves live in one record because provenance is an input to the
/// decision, not a separate fact filed next to it.
///
/// Older files that carry only `name` and `approved` still load: the fields
/// added since are optional and absent means "the person put this here", which
/// is what was true of everything written before `add` existed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRecord {
    /// Kept so the file reads as a record of decisions rather than a list of
    /// paths; nothing is matched on it.
    pub name: String,
    /// Absent when keke did not put the plugin here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed: Option<InstallSource>,
    /// The commit that was installed, when the source is git. What `update`
    /// compares against, and what a person quotes in a bug report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// The lines from [`ResolvedPlugin::executables`] as they were approved.
    /// Absent means approval was never given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved: Option<Vec<String>>,
}

/// What a person has decided about each plugin, keyed by canonical root.
///
/// Keyed by path rather than by name because a name is something a repository
/// chooses: approving `acme` in one project must not approve a different `acme`
/// in the next one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TrustStore {
    entries: BTreeMap<String, PluginRecord>,
}

impl TrustStore {
    /// Where `plugin` stands.
    #[must_use]
    pub fn evaluate(&self, plugin: &ResolvedPlugin) -> Trust {
        let executables = plugin.executables();
        if executables.is_empty() {
            return Trust::NothingToRun;
        }
        let record = self.entries.get(&plugin.root.to_string());

        // The shortcut for a plugin in the person's own directory covers one
        // thing: a directory they placed there themselves, under *keke's* home
        // directory. It does not extend to what `keke plugin add` fetched into
        // that directory on their behalf — they named a URL, they did not read
        // what came back — nor to a plugin found under another harness's home
        // directory: putting something in `~/.claude` is consent for Claude
        // Code to run it, not for keke to. Without these exclusions, `add`
        // would reopen from the inside the hole the project-scope gate closes
        // from the outside, and a foreign plugin would be trusted on the
        // strength of a decision the person never made about keke at all.
        let keke_installed = record.is_some_and(|record| record.installed.is_some());
        if plugin.scope == PluginScope::User && plugin.owned && !keke_installed {
            return Trust::OwnedByThePerson;
        }

        match record.and_then(|record| record.approved.as_ref()) {
            None => Trust::NeverApproved,
            Some(approved) if *approved == executables => Trust::Approved,
            Some(_) => Trust::ChangedSinceApproval,
        }
    }

    /// The record for `plugin`, if there is one.
    #[must_use]
    pub fn record(&self, plugin: &ResolvedPlugin) -> Option<&PluginRecord> {
        self.entries.get(&plugin.root.to_string())
    }

    /// Record approval of exactly what `plugin` contributes now.
    pub fn approve(&mut self, plugin: &ResolvedPlugin) {
        let entry = self.entries.entry(plugin.root.to_string()).or_default();
        entry.name = plugin.name.clone();
        entry.approved = Some(plugin.executables());
    }

    /// Record where a plugin came from, without approving anything.
    ///
    /// Separate from [`Self::approve`] so that installing and consenting stay
    /// two statements about the world. A caller that fetches something and then
    /// fails to show it to the person leaves a record that says exactly that.
    pub fn record_install(
        &mut self,
        root: &crate::AbsPath,
        name: &str,
        source: InstallSource,
        revision: Option<String>,
    ) {
        let entry = self.entries.entry(root.to_string()).or_default();
        entry.name = name.to_string();
        entry.installed = Some(source);
        entry.revision = revision;
    }

    /// Withdraw approval, keeping what is known about where the plugin came
    /// from. Returns whether there was an approval to withdraw.
    pub fn revoke(&mut self, plugin: &ResolvedPlugin) -> bool {
        match self.entries.get_mut(&plugin.root.to_string()) {
            Some(record) => record.approved.take().is_some(),
            None => false,
        }
    }

    /// Drop everything known about the plugin at `root`.
    ///
    /// Uninstalling must forget the approval too. Otherwise reinstalling to the
    /// same path inherits a decision made about contents that are gone, which
    /// is an approval nobody gave.
    pub fn forget(&mut self, root: &crate::AbsPath) {
        self.entries.remove(&root.to_string());
    }
}

/// A plugin whose programs were held back, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Withheld {
    pub name: String,
    pub root: crate::AbsPath,
    pub trust: Trust,
    /// The lines the person would be approving.
    pub executables: Vec<String>,
}

impl PluginSet {
    /// Strip the programs of every plugin the store does not vouch for.
    ///
    /// The plugin itself stays: its skills and commands are text, and text from
    /// the repository already reaches the model through the project's own
    /// instruction files. What is removed is the ability to run something.
    ///
    /// Withholding is the default and the failure mode. There is no flag that
    /// turns it off, because a flag that turns it off is what a person reaches
    /// for once and then leaves on. A deployment that means to run a project's
    /// plugins says so per plugin, with `keke plugin trust`.
    #[must_use]
    pub fn withhold_untrusted(self, store: &TrustStore) -> (Self, Vec<Withheld>) {
        let mut withheld = Vec::new();
        let plugins = self
            .plugins
            .into_iter()
            .map(|mut plugin| {
                let trust = store.evaluate(&plugin);
                if trust.permits_running() {
                    return plugin;
                }
                withheld.push(Withheld {
                    name: plugin.name.clone(),
                    root: plugin.root.clone(),
                    trust,
                    executables: plugin.executables(),
                });
                plugin.hooks.clear();
                plugin.mcp_servers.clear();
                plugin
            })
            .collect();
        (Self { plugins }, withheld)
    }
}
