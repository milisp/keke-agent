//! Finding the runtime plugins installed on this machine.
//!
//! Discovery lives in the composition root rather than in `keke-plugin`
//! because *where* plugins are installed is a deployment decision, while
//! parsing one is not. It is also the only layer that knows both the harness
//! home and the workspace root.

use anyhow::Context as _;
use anyhow::Result;
use keke_config_types::HomeLayout;
use keke_paths::AbsPath;
use keke_plugin::PluginScope;
use keke_plugin::PluginSet;
use keke_plugin::TrustStore;
use keke_plugin::Withheld;

/// Where plugins are looked for, in ascending precedence.
///
/// Both keke's own directories and Claude Code's are searched. A person who
/// already has plugins installed for that ecosystem should not have to move or
/// reinstall anything, and a plugin author should not have to publish twice.
fn roots(home: &HomeLayout) -> Vec<(AbsPath, PluginScope)> {
    let mut roots = Vec::new();
    let mut push = |path: std::path::PathBuf, scope: PluginScope| {
        if let Ok(abs) = AbsPath::new(path) {
            roots.push((abs, scope));
        }
    };

    push(home.home.as_path().join("plugins"), PluginScope::User);
    if let Some(claude) = dirs::home_dir() {
        push(claude.join(".claude/plugins"), PluginScope::User);
    }
    push(
        home.workspace_root.as_path().join(".keke/plugins"),
        PluginScope::Project,
    );
    push(
        home.workspace_root.as_path().join(".claude/plugins"),
        PluginScope::Project,
    );
    roots
}

/// Resolve every installed plugin.
///
/// A broken plugin fails startup rather than being skipped with a warning. That
/// is the harsher choice and the deliberate one: a plugin that silently does
/// not load looks exactly like a plugin whose author made a mistake in its
/// contents, and the person is left debugging behavior that was never running.
pub(crate) fn discover(home: &HomeLayout) -> Result<PluginSet> {
    let mut plugins = Vec::new();
    for (root, scope) in roots(home) {
        plugins.extend(
            keke_plugin::discover(&root, scope)
                .with_context(|| format!("reading plugins under {root}"))?,
        );
    }
    PluginSet::compose(plugins).context("composing the installed plugins")
}

/// Where approvals are kept.
///
/// In the person's own directory, never in the workspace: a file the repository
/// can write is not a record of what the person agreed to.
fn trust_path(home: &HomeLayout) -> std::path::PathBuf {
    home.home.as_path().join("plugin-trust.json")
}

/// Read the approvals given so far.
///
/// A missing file is an empty store — nobody has approved anything yet, which
/// is where everyone starts. A *corrupt* file is an error: the safe reading of
/// unreadable approvals would be to treat them as absent, but silently doing so
/// would turn a bug in this file into "your plugins stopped working" with
/// nothing said, and the person would reach for `trust` again without ever
/// learning why.
pub(crate) fn trust_store(home: &HomeLayout) -> Result<TrustStore> {
    let path = trust_path(home);
    match std::fs::read_to_string(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(TrustStore::default()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        Ok(text) => {
            serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))
        }
    }
}

/// Write the approvals back.
pub(crate) fn save_trust_store(home: &HomeLayout, store: &TrustStore) -> Result<()> {
    let path = trust_path(home);
    let text = serde_json::to_string_pretty(store).context("encoding the plugin trust store")?;
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
}

/// Resolve every plugin, then hold back the programs of the ones not vouched for.
pub(crate) fn discover_trusted(home: &HomeLayout) -> Result<(PluginSet, Vec<Withheld>)> {
    let store = trust_store(home)?;
    Ok(discover(home)?.withhold_untrusted(&store))
}

/// What to tell a person whose session is starting with programs held back.
///
/// Said once, on stderr, naming the command that resolves it. A plugin that was
/// installed and does nothing is otherwise indistinguishable from a plugin that
/// is broken, and the person debugs the wrong thing.
pub(crate) fn report_withheld(withheld: &[Withheld]) {
    for plugin in withheld {
        eprintln!(
            "keke: plugin `{}` is {} — its {} program(s) will not run",
            plugin.name,
            plugin.trust,
            plugin.executables.len()
        );
    }
    if !withheld.is_empty() {
        eprintln!(
            "keke: review with `keke plugin show <name>`, allow with `keke plugin trust <name>`"
        );
    }
}
