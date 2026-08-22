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
