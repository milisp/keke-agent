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

/// Whose directory a root is, which decides what a failure in it may cost.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// A directory keke owns and put things in.
    Keke,
    /// A directory belonging to another harness, read for compatibility.
    Foreign,
}

/// Where plugins are looked for, in ascending precedence.
///
/// Both keke's own directories and Claude Code's are searched. A person who
/// already has plugins installed for that ecosystem should not have to move or
/// reinstall anything, and a plugin author should not have to publish twice.
fn roots(home: &HomeLayout) -> Vec<(AbsPath, PluginScope, Origin)> {
    let mut roots = Vec::new();
    let mut push = |path: std::path::PathBuf, scope: PluginScope, origin: Origin| {
        if let Ok(abs) = AbsPath::new(path) {
            roots.push((abs, scope, origin));
        }
    };

    push(
        home.home.as_path().join("plugins"),
        PluginScope::User,
        Origin::Keke,
    );
    if let Some(claude) = dirs::home_dir() {
        push(
            claude.join(".claude/plugins"),
            PluginScope::User,
            Origin::Foreign,
        );
    }
    push(
        home.workspace_root.as_path().join(".keke/plugins"),
        PluginScope::Project,
        Origin::Keke,
    );
    push(
        home.workspace_root.as_path().join(".claude/plugins"),
        PluginScope::Project,
        Origin::Foreign,
    );
    roots
}

/// Directories that are a person's own rather than a plugin package, and the
/// name each contributes under.
///
/// A person who wants one slash command or one MCP server should not have to
/// author a plugin to get it: dropping `commands/review.md` into `~/.keke`, or
/// running `keke mcp add`, is the whole gesture. What those directories hold is
/// exactly a plugin's convention content, so they are read as packages — which
/// also means the workspace one passes through the same trust gate as anything
/// else the repository ships (`AGENTS.md` invariant 13).
///
/// The names are ones no published plugin is likely to claim, because a
/// collision here would be reported as the same plugin installed twice.
pub(crate) fn local_roots(
    home: &HomeLayout,
) -> Vec<(std::path::PathBuf, &'static str, PluginScope, bool)> {
    let mut roots = vec![(
        home.home.as_path().to_path_buf(),
        "local",
        PluginScope::User,
        true,
    )];
    if let Some(claude) = dirs::home_dir() {
        roots.push((claude.join(".claude"), "claude", PluginScope::User, false));
    }
    roots.push((
        home.workspace_root.as_path().join(".keke"),
        "workspace",
        PluginScope::Project,
        true,
    ));
    roots.push((
        home.workspace_root.as_path().join(".claude"),
        "claude-workspace",
        PluginScope::Project,
        false,
    ));
    roots
}

/// Whether a person's directory holds anything keke would read from it.
///
/// Checked before loading so an empty `~/.keke` does not become a plugin that
/// contributes nothing and shows up in `keke plugin list` regardless.
fn has_local_content(root: &std::path::Path) -> bool {
    root.join(keke_plugin::COMMANDS_DIR).is_dir()
        || root.join(keke_plugin::SKILLS_DIR).is_dir()
        || root.join(keke_plugin::MCP_FILE).is_file()
}

/// Resolve every installed plugin.
///
/// A broken plugin fails startup rather than being skipped with a warning. That
/// is the harsher choice and the deliberate one: a plugin that silently does
/// not load looks exactly like a plugin whose author made a mistake in its
/// contents, and the person is left debugging behavior that was never running.
pub(crate) fn discover(home: &HomeLayout) -> Result<PluginSet> {
    let mut plugins = Vec::new();
    for (root, scope, origin) in roots(home) {
        let owned = origin == Origin::Keke;
        match keke_plugin::discover(&root, scope, owned) {
            Ok(found) => plugins.extend(found),
            // A directory keke owns failing is keke's problem to state. Another
            // harness's directory failing is not: keke would be refusing to
            // start over a file it does not manage, about plugins the person
            // may not even use here. Said out loud, then stepped over.
            Err(error) if origin == Origin::Foreign => {
                eprintln!("keke: skipping plugins under {root}: {error}");
            }
            Err(error) => {
                return Err(error).with_context(|| format!("reading plugins under {root}"));
            }
        }
    }

    // Plugins the other harness installed, wherever it put them. Reading its
    // record means a person who already installed something there does not
    // install it again here to get the same thing twice. A record that points
    // at something unreadable is skipped rather than fatal: it is another
    // program's file, and keke refusing to start over it would be keke's bug to
    // the person.
    if let Some(claude) = dirs::home_dir() {
        let record = claude.join(".claude/plugins/installed_plugins.json");
        for install in keke_plugin::foreign_installs(&record, &home.workspace_root) {
            // `false`: this came out of Claude Code's own install record, not
            // out of a directory keke put the plugin in itself.
            let Ok(mut plugin) =
                keke_plugin::load(install.install_path.as_path(), PluginScope::User, false)
            else {
                continue;
            };

            // The record's key is the plugin's real name. Its directory is
            // often a content hash, and deriving the name from that would give
            // two unrelated plugins the same one.
            plugin.name = install.name;

            // A foreign record that collides with something already found is
            // dropped, not reported. Everywhere else a name claimed twice is an
            // error, because it means two things keke was asked to install are
            // ambiguous. This file is another program's bookkeeping about its
            // own installs: keke refusing to start over what it finds there
            // would make that program's mess into keke's failure.
            if plugins.iter().any(|found| found.name == plugin.name) {
                continue;
            }
            plugins.push(plugin);
        }
    }

    for (root, name, scope, owned) in local_roots(home) {
        if !has_local_content(&root) {
            continue;
        }
        match keke_plugin::load_named(&root, scope, owned, Some(name)) {
            Ok(plugin) => plugins.push(plugin),
            // Same division as above: keke's own directory failing is keke's to
            // state, another harness's is not.
            Err(error) => {
                eprintln!("keke: skipping {}: {error}", root.display());
            }
        }
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
            "{}",
            warn(&format!(
                "keke: plugin `{}` is {} — its {} program(s) will not run",
                plugin.name,
                plugin.trust,
                plugin.executables.len()
            ))
        );
    }
    if !withheld.is_empty() {
        eprintln!(
            "{}",
            warn(
                "keke: review with `keke plugin show <name>`, allow with `keke plugin trust <name>`"
            )
        );
    }
}

/// Wrap `text` in yellow, when stderr is a terminal that can show it.
///
/// A withheld plugin is a warning, not a routine status line, and it is easy to
/// miss among startup output that is otherwise plain. Piped or redirected
/// output gets the bare text back — escape codes in a log file are noise, not
/// color.
fn warn(text: &str) -> std::borrow::Cow<'_, str> {
    if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        std::borrow::Cow::Owned(format!("\u{1b}[33m{text}\u{1b}[0m"))
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

/// Where `keke plugin add` puts things.
pub(crate) fn install_dir(home: &HomeLayout) -> std::path::PathBuf {
    home.home.as_path().join("plugins")
}

/// Show what a plugin would run and get an answer.
///
/// The lines are printed before the question, never after: what is being
/// approved is these lines, and a person cannot approve what they were not
/// shown. A non-interactive session has nobody to ask, so it declines unless
/// `--yes` was passed on the command that started it.
pub(crate) fn confirm_executables(
    plugin: &keke_plugin::ResolvedPlugin,
    assumed_yes: bool,
) -> Result<bool> {
    let executables = plugin.executables();
    if executables.is_empty() {
        println!("`{}` runs no programs.", plugin.name);
        return Ok(true);
    }

    println!("`{}` would run:", plugin.name);
    for line in &executables {
        println!("  {line}");
    }

    if assumed_yes {
        println!("(--yes)");
        return Ok(true);
    }
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        println!(
            "not a terminal, so there is nobody to ask — pass --yes, or run `keke plugin trust {}` later",
            plugin.name
        );
        return Ok(false);
    }

    print!("allow these? [y/N] ");
    std::io::Write::flush(&mut std::io::stdout()).context("writing the prompt")?;
    let mut answer = String::new();
    std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut answer)
        .context("reading the answer")?;
    // Anything that is not a clear yes is a no. A prompt that accepts an empty
    // line as consent is a prompt people learn to walk past.
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}
