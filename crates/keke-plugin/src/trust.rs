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
            let mut line = format!("mcp {}: {}", server.name, server.command);
            for arg in &server.args {
                line.push(' ');
                line.push_str(arg);
            }
            if !server.env.is_empty() {
                let names: Vec<&str> = server.env.iter().map(|(name, _)| name.as_str()).collect();
                line.push_str(" (env: ");
                line.push_str(&names.join(", "));
                line.push(')');
            }
            lines.push(line);
        }
        lines.sort();
        lines
    }
}

/// One approval, as it is written to disk.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    /// Kept so the file reads as a record of a decision rather than a list of
    /// paths; nothing is matched on it.
    pub name: String,
    /// The lines from [`ResolvedPlugin::executables`] as they were approved.
    pub approved: Vec<String>,
}

/// The approvals a person has given, keyed by canonical plugin root.
///
/// Keyed by path rather than by name because a name is something a repository
/// chooses: approving `acme` in one project must not approve a different `acme`
/// in the next one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TrustStore {
    entries: BTreeMap<String, Approval>,
}

impl TrustStore {
    /// Where `plugin` stands.
    #[must_use]
    pub fn evaluate(&self, plugin: &ResolvedPlugin) -> Trust {
        let executables = plugin.executables();
        if executables.is_empty() {
            return Trust::NothingToRun;
        }
        // A plugin in the person's own directory is there because they put it
        // there. Asking about it would train the answer to the question that
        // matters into a reflex.
        if plugin.scope == PluginScope::User {
            return Trust::OwnedByThePerson;
        }
        match self.entries.get(&plugin.root.to_string()) {
            None => Trust::NeverApproved,
            Some(approval) if approval.approved == executables => Trust::Approved,
            Some(_) => Trust::ChangedSinceApproval,
        }
    }

    /// Record approval of exactly what `plugin` contributes now.
    pub fn approve(&mut self, plugin: &ResolvedPlugin) {
        self.entries.insert(
            plugin.root.to_string(),
            Approval {
                name: plugin.name.clone(),
                approved: plugin.executables(),
            },
        );
    }

    /// Withdraw approval. Returns whether there was one to withdraw.
    pub fn revoke(&mut self, plugin: &ResolvedPlugin) -> bool {
        self.entries.remove(&plugin.root.to_string()).is_some()
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
