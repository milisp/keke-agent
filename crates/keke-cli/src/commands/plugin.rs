//! Inspecting and installing runtime plugins.

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use keke_config::Config;
use keke_paths::AbsPath;

use crate::cli::PluginAction;

/// Inspect installed runtime plugins.
///
/// Listing must never activate anything: resolution locates files and reads
/// manifests, and that is all. A person needs to be able to look at a plugin
/// they do not yet trust.
pub(super) fn plugin(action: PluginAction, config: Config) -> Result<()> {
    let plugins = crate::plugins::discover(&config.home)?;
    let mut store = crate::plugins::trust_store(&config.home)?;

    match action {
        PluginAction::List => {
            if plugins.is_empty() {
                println!("no plugins installed");
                println!("\nkeke reads plugins from:");
                println!("  {}/plugins", config.home.home);
                println!("  ~/.claude/plugins");
                println!("  {}/.keke/plugins", config.home.workspace_root);
                println!("  {}/.claude/plugins", config.home.workspace_root);
                return Ok(());
            }

            for plugin in plugins.plugins() {
                let version = plugin.version.as_deref().unwrap_or("no version");
                let trust = store.evaluate(plugin);
                println!("{} ({version}) [{}, {trust}]", plugin.name, plugin.scope);
                if let Some(description) = &plugin.description {
                    println!("  {description}");
                }
                println!(
                    "  {} skills, {} commands, {} hooks, {} mcp servers",
                    plugin.skills.len(),
                    plugin.commands.len(),
                    plugin.hooks.len(),
                    plugin.mcp_servers.len(),
                );

                // Anything keke cannot honor is said out loud here rather than
                // left for the person to discover as silence.
                for kind in &plugin.unsupported {
                    println!("  ! `{kind}` is not implemented by keke and does nothing");
                }
                let inert = plugin.inert_hooks().count();
                if inert > 0 {
                    println!("  ! {inert} hook(s) bound to events keke does not run");
                }
                if !trust.permits_running() {
                    println!(
                        "  ! its programs will not run — `keke plugin trust {}` to allow them",
                        plugin.name
                    );
                }
            }
        }
        PluginAction::Show { name } => {
            let plugin = plugins
                .get(&name)
                .with_context(|| format!("no plugin named `{name}` is installed"))?;

            println!("{} [{}]", plugin.name, plugin.scope);
            println!("root: {}", plugin.root);
            println!("trust: {}", store.evaluate(plugin));
            if let Some(version) = &plugin.version {
                println!("version: {version}");
            }
            if let Some(description) = &plugin.description {
                println!("description: {description}");
            }

            if !plugin.skills.is_empty() {
                println!("\nskills:");
                for skill in &plugin.skills {
                    println!("  {}:{} — {}", skill.plugin, skill.name, skill.description);
                }
            }
            if !plugin.commands.is_empty() {
                println!("\ncommands:");
                for command in &plugin.commands {
                    println!(
                        "  {}:{} — {}",
                        command.plugin, command.name, command.description
                    );
                }
            }
            if !plugin.mcp_servers.is_empty() {
                println!("\nmcp servers:");
                for server in &plugin.mcp_servers {
                    println!(
                        "  {}: {} {}",
                        server.name,
                        server.command,
                        server.args.join(" ")
                    );
                    // Names only. A value here could be a secret, and this
                    // output is the kind of thing people paste into an issue.
                    for (key, _) in &server.env {
                        println!("    env {key}");
                    }
                }
            }
            if !plugin.hooks.is_empty() {
                println!("\nhooks:");
                for hook in &plugin.hooks {
                    let matcher = if hook.matcher.is_empty() {
                        "*"
                    } else {
                        &hook.matcher
                    };
                    let inert = if hook.event.is_supported() {
                        ""
                    } else {
                        "  (keke does not run this event)"
                    };
                    println!("  {} [{matcher}] {}{inert}", hook.event, hook.command);
                }
            }
            for kind in &plugin.unsupported {
                println!("\n! `{kind}` is not implemented by keke and does nothing");
            }
        }
        PluginAction::Trust { name } => {
            let plugin = plugins
                .get(&name)
                .with_context(|| format!("no plugin named `{name}` is installed"))?;
            let executables = plugin.executables();

            if executables.is_empty() {
                println!("`{name}` runs no programs; there is nothing to trust");
                return Ok(());
            }

            // Printed before it takes effect, not after. What is being approved
            // is these lines, and a person cannot approve what they were not
            // shown.
            println!("trusting `{name}` allows it to run:");
            for line in &executables {
                println!("  {line}");
            }

            store.approve(plugin);
            crate::plugins::save_trust_store(&config.home, &store)?;
            println!("\n`{name}` is trusted. Adding to what it runs revokes this.");
        }
        PluginAction::Add {
            source,
            git_ref,
            plugin: wanted,
            yes,
        } => {
            add_plugin(
                &config,
                &mut store,
                &source,
                git_ref.as_deref(),
                wanted.as_deref(),
                yes,
            )?;
        }
        PluginAction::Update { name } => {
            update_plugins(&config, &mut store, &plugins, name.as_deref())?;
        }
        PluginAction::Remove { name } => {
            let plugin = plugins
                .get(&name)
                .with_context(|| format!("no plugin named `{name}` is installed"))?;

            // Only what keke installed is keke's to delete. A plugin the person
            // placed by hand, or one the repository ships, is removed the way it
            // arrived.
            let managed = crate::plugins::install_dir(&config.home);
            if !plugin.root.as_path().starts_with(&managed) {
                bail!(
                    "`{name}` was not installed by keke (it is at {}); remove it the way it got there",
                    plugin.root
                );
            }

            std::fs::remove_dir_all(plugin.root.as_path())
                .with_context(|| format!("removing {}", plugin.root))?;
            store.forget(&plugin.root);
            crate::plugins::save_trust_store(&config.home, &store)?;
            println!("removed `{name}` and forgot what was decided about it");
        }
        PluginAction::Untrust { name } => {
            let plugin = plugins
                .get(&name)
                .with_context(|| format!("no plugin named `{name}` is installed"))?;

            if store.revoke(plugin) {
                crate::plugins::save_trust_store(&config.home, &store)?;
                println!("`{name}` is no longer trusted; its programs will not run");
            } else {
                println!("`{name}` was not trusted; nothing changed");
            }
        }
    }
    Ok(())
}

/// Install one plugin from a git URL or a directory.
///
/// Fetching happens into a staging directory, and nothing reaches the person's
/// plugin directory until the contents have resolved cleanly and been approved.
/// A source that turns out to be broken, or an approval that is declined, must
/// leave nothing behind.
fn add_plugin(
    config: &Config,
    store: &mut keke_plugin::TrustStore,
    source: &str,
    git_ref: Option<&str>,
    wanted: Option<&str>,
    assumed_yes: bool,
) -> Result<()> {
    let staging = tempfile::tempdir().context("making a staging directory")?;
    let fetched = staging.path().join("source");
    let local = std::path::Path::new(source);

    let from_git = if local.is_dir() {
        crate::install::copy_tree(local, &fetched)?;
        false
    } else {
        crate::install::clone(source, git_ref, &fetched)?;
        true
    };

    let fetched_abs = AbsPath::new(&fetched).context("staging path")?;
    let revision = crate::install::revision(&fetched);

    // A source may hold one plugin or a catalog of many. Which it is decides
    // what the person is being asked about, so it is settled before anything is
    // shown to them.
    let (package, entry_name) = match keke_plugin::Marketplace::load(&fetched_abs)? {
        None => (fetched.clone(), None),
        Some(catalog) => {
            let Some(wanted) = wanted else {
                println!(
                    "`{}` is a catalog of {} plugins:",
                    catalog.name,
                    catalog.entries.len()
                );
                for entry in &catalog.entries {
                    let description = entry.description.as_deref().unwrap_or("");
                    println!("  {} — {description}", entry.name);
                }
                for name in &catalog.skipped {
                    println!(
                        "  ! {name} — listed with no usable source, so keke cannot install it"
                    );
                }
                bail!("name one with --plugin <name>");
            };
            // An entry the catalog dropped for having no usable source would
            // otherwise be reported as "no such plugin", sending the person to
            // look for a typo in a name that is spelled correctly.
            if catalog.skipped.iter().any(|name| name == wanted) {
                bail!(
                    "`{}` lists `{wanted}` but does not say where to get it; that is the catalog's bug to fix",
                    catalog.name
                );
            }
            let entry = catalog
                .get(wanted)
                .with_context(|| format!("`{}` offers no plugin named `{wanted}`", catalog.name))?;
            match &entry.source {
                keke_plugin::EntrySource::Local { path } => (
                    fetched.join(path.trim_start_matches("./")),
                    Some(wanted.to_string()),
                ),
                keke_plugin::EntrySource::Git { url, reference } => {
                    let nested = staging.path().join("entry");
                    let reference = match reference {
                        keke_plugin::GitRef::Pinned(sha) => Some(sha.clone()),
                        keke_plugin::GitRef::Moving(name) => Some(name.clone()),
                        keke_plugin::GitRef::Default => None,
                    };
                    crate::install::clone(url, reference.as_deref(), &nested)?;
                    (nested, Some(wanted.to_string()))
                }
            }
        }
    };

    let package = AbsPath::new(&package).context("the plugin directory inside the source")?;
    let plugin = keke_plugin::load(package.as_path(), keke_plugin::PluginScope::User)
        .with_context(|| format!("reading the plugin at {package}"))?;

    if !crate::plugins::confirm_executables(&plugin, assumed_yes)? {
        bail!("not installed");
    }

    let target = crate::plugins::install_dir(&config.home).join(&plugin.name);
    std::fs::create_dir_all(crate::plugins::install_dir(&config.home))
        .context("creating the plugin directory")?;
    crate::install::swap_in(package.as_path(), &target)?;

    let target = AbsPath::new(&target).context("the installed path")?;
    let moving = git_ref.is_none_or(|reference| !looks_like_a_commit(reference));
    let install_source = if from_git {
        match entry_name {
            Some(entry) => keke_plugin::InstallSource::Marketplace {
                url: source.to_string(),
                catalog: source.to_string(),
                entry,
                reference: git_ref.map(str::to_string),
                moving,
            },
            None => keke_plugin::InstallSource::Git {
                url: source.to_string(),
                reference: git_ref.map(str::to_string),
                moving,
            },
        }
    } else {
        keke_plugin::InstallSource::Path {
            path: source.to_string(),
        }
    };
    let can_change = install_source.can_change_under_you();

    store.record_install(&target, &plugin.name, install_source, revision);
    // Approval is recorded against the installed path, so it is taken after the
    // move: the record has to describe where the plugin actually is.
    let installed = keke_plugin::load(target.as_path(), keke_plugin::PluginScope::User)
        .with_context(|| format!("reading the installed plugin at {target}"))?;
    store.approve(&installed);
    crate::plugins::save_trust_store(&config.home, store)?;

    println!("installed `{}` into {target}", plugin.name);
    if can_change {
        println!(
            "note: this source can point somewhere else later — `keke plugin update` will ask again if what it runs changes"
        );
    }
    Ok(())
}

/// Whether a ref names a commit rather than a branch or tag.
///
/// A guess, and only used to describe the source in the record. Getting it
/// wrong describes a pin as moving, which asks the person one question too
/// many; the opposite would be the dangerous direction, so the test is strict.
fn looks_like_a_commit(reference: &str) -> bool {
    reference.len() >= 7
        && reference.len() <= 40
        && reference.chars().all(|c| c.is_ascii_hexdigit())
}

/// Fetch installed plugins again.
///
/// The point of this command is the check at the end, not the fetch: an update
/// that changes what a plugin runs withdraws the approval that covered the old
/// contents. Otherwise `update` would be the way to get code onto a machine
/// without anyone looking at it, which is the hole the whole gate exists for.
fn update_plugins(
    config: &Config,
    store: &mut keke_plugin::TrustStore,
    plugins: &keke_plugin::PluginSet,
    only: Option<&str>,
) -> Result<()> {
    let mut updated = 0;
    for plugin in plugins.plugins() {
        if only.is_some_and(|name| name != plugin.name) {
            continue;
        }
        let Some(record) = store.record(plugin) else {
            continue;
        };
        let Some(source) = record.installed.clone() else {
            continue;
        };
        let Some(url) = source.git_url() else {
            println!(
                "`{}` was installed from a directory; nothing to fetch",
                plugin.name
            );
            continue;
        };

        let before = record.revision.clone();
        let staging = tempfile::tempdir().context("making a staging directory")?;
        let fetched = staging.path().join("source");
        crate::install::clone(url, source.git_ref(), &fetched)?;
        let after = crate::install::revision(&fetched);

        if after.is_some() && after == before {
            println!(
                "`{}` is already at {}",
                plugin.name,
                before.unwrap_or_default()
            );
            continue;
        }

        crate::install::swap_in(&fetched, plugin.root.as_path())?;
        let refreshed = keke_plugin::load(plugin.root.as_path(), plugin.scope)
            .with_context(|| format!("reading the updated plugin at {}", plugin.root))?;
        store.record_install(&plugin.root, &refreshed.name, source, after.clone());
        updated += 1;

        println!(
            "updated `{}`{}",
            plugin.name,
            after
                .as_deref()
                .map(|r| format!(" to {r}"))
                .unwrap_or_default()
        );
        match store.evaluate(&refreshed) {
            keke_plugin::Trust::Approved | keke_plugin::Trust::NothingToRun => {}
            _ => {
                println!("  what it runs changed, so it is no longer trusted:");
                for line in refreshed.executables() {
                    println!("    {line}");
                }
                println!("  `keke plugin trust {}` to allow it again", refreshed.name);
            }
        }
    }
    crate::plugins::save_trust_store(&config.home, store)?;
    if updated == 0 && only.is_none() {
        println!("nothing to update");
    }
    Ok(())
}
