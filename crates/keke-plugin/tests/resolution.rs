//! Each test names a rule from `docs/architecture.md` or `AGENTS.md` and would
//! fail if the rule broke, however the code was rearranged.
//!
//! An integration test is not `#[cfg(test)]`, so the clippy allowance for
//! panicking in tests does not reach it. Same waiver the other suites take.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;

use keke_plugin::HookEvent;
use keke_plugin::PluginError;
use keke_plugin::PluginScope;
use keke_plugin::PluginSet;
use keke_plugin::Trust;
use keke_plugin::TrustStore;
use keke_plugin::discover;
use keke_plugin::load;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("has a parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

/// A plugin in the shape published plugins actually take: no keke-specific
/// file, contributions found by convention.
fn claude_plugin(root: &Path, name: &str) -> std::path::PathBuf {
    let dir = root.join(name);
    write(
        &dir.join("plugin.json"),
        &format!(r#"{{"name": "{name}", "version": "1.0.0", "homepage": "https://example"}}"#),
    );
    write(
        &dir.join("skills/review/SKILL.md"),
        "---\nname: review\ndescription: how this team reviews\n---\n\nbody\n",
    );
    write(
        &dir.join("commands/ship.md"),
        "---\ndescription: cut a release\n---\n\nship it\n",
    );
    write(
        &dir.join(".mcp.json"),
        r#"{"mcpServers": {"api": {"command": "node", "args": ["server.js"]}}}"#,
    );
    write(
        &dir.join("hooks/hooks.json"),
        r#"{"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "./audit.sh"}]}]}}"#,
    );
    dir
}

#[test]
fn a_plugin_written_for_claude_code_loads_unchanged() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = claude_plugin(tmp.path(), "acme");

    let plugin = load(&dir, PluginScope::User).expect("resolves");

    assert_eq!(plugin.name, "acme");
    assert_eq!(plugin.skills.len(), 1);
    assert_eq!(plugin.skills[0].description, "how this team reviews");
    assert_eq!(plugin.commands.len(), 1);
    assert_eq!(plugin.commands[0].name, "ship");
    assert_eq!(plugin.mcp_servers.len(), 1);
    assert_eq!(plugin.mcp_servers[0].command, "node");
    assert_eq!(plugin.hooks.len(), 1);
    assert_eq!(plugin.hooks[0].event, HookEvent::PreToolUse);
}

#[test]
fn resolution_does_not_run_anything() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = claude_plugin(tmp.path(), "acme");
    write(
        &dir.join("hooks/hooks.json"),
        &format!(
            r#"{{"hooks": {{"SessionStart": [{{"hooks": [{{"type": "command", "command": "touch {}/RAN"}}]}}]}}}}"#,
            dir.display()
        ),
    );

    let plugin = load(&dir, PluginScope::User).expect("resolves");

    assert_eq!(plugin.hooks.len(), 1);
    // Located, not launched. A surface can list an untrusted plugin safely.
    assert!(!dir.join("RAN").exists());
}

#[test]
fn a_package_with_no_manifest_still_loads_from_convention() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("barebones");
    write(
        &dir.join("skills/review/SKILL.md"),
        "---\ndescription: no manifest anywhere\n---\n\nbody\n",
    );

    let plugin = load(&dir, PluginScope::User).expect("resolves");

    // The name comes from the directory, which is how most published plugins
    // are actually shaped.
    assert_eq!(plugin.name, "barebones");
    assert_eq!(plugin.skills.len(), 1);
}

#[test]
fn the_claude_manifest_location_is_accepted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("compat");
    write(
        &dir.join(".claude-plugin/plugin.json"),
        r#"{"name": "compat", "description": "manifest in the claude location"}"#,
    );

    let plugin = load(&dir, PluginScope::User).expect("resolves");

    assert_eq!(plugin.name, "compat");
    assert_eq!(
        plugin.description.as_deref(),
        Some("manifest in the claude location")
    );
}

#[test]
fn unknown_metadata_is_ignored_but_an_unknown_contribution_is_reported() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("newer");
    write(
        &dir.join("plugin.json"),
        r#"{"name": "newer", "homepage": "https://x", "keywords": ["a"], "lspServers": {"rust": {}}}"#,
    );

    let plugin = load(&dir, PluginScope::User).expect("loads despite the unknown key");

    // Metadata drift must not block a load, or no manifest written for a newer
    // host would ever install.
    assert_eq!(plugin.name, "newer");
    // A capability keke does not implement is a different matter: dropping it
    // silently lets an author believe it is active.
    assert_eq!(plugin.unsupported, ["lspServers"]);
}

#[test]
fn a_hook_for_an_event_keke_does_not_run_is_reported_not_dropped() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("futuristic");
    write(&dir.join("plugin.json"), r#"{"name": "futuristic"}"#);
    write(
        &dir.join("hooks/hooks.json"),
        r#"{"hooks": {"Notification": [{"hooks": [{"type": "command", "command": "./notify.sh"}]}]}}"#,
    );

    let plugin = load(&dir, PluginScope::User).expect("resolves");

    assert_eq!(plugin.inert_hooks().count(), 1);
    assert_eq!(
        plugin.hooks[0].event,
        HookEvent::Unsupported("Notification".to_string())
    );
    // And it is not offered to anything that runs hooks.
    let set = PluginSet::compose(vec![plugin]).expect("composes");
    assert_eq!(set.hooks_for(&HookEvent::PreToolUse).count(), 0);
}

#[test]
fn a_hook_of_an_unrecognized_type_is_not_run() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("exotic");
    write(&dir.join("plugin.json"), r#"{"name": "exotic"}"#);
    write(
        &dir.join("hooks/hooks.json"),
        r#"{"hooks": {"PreToolUse": [{"hooks": [{"type": "wasm", "command": "./mystery.wasm"}]}]}}"#,
    );

    // Guessing at an unknown type would mean running something under a contract
    // keke does not know.
    assert!(
        load(&dir, PluginScope::User)
            .expect("resolves")
            .hooks
            .is_empty()
    );
}

#[test]
fn a_resource_outside_the_package_root_is_refused() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("outside/secrets.json"),
        r#"{"mcpServers": {}}"#,
    );
    let dir = tmp.path().join("escaper");
    write(
        &dir.join("plugin.json"),
        r#"{"name": "escaper", "mcpServers": "../outside/secrets.json"}"#,
    );

    assert!(matches!(
        load(&dir, PluginScope::User),
        Err(PluginError::Escape { .. })
    ));
}

#[test]
#[cfg(unix)]
fn a_symlink_out_of_the_package_root_is_refused() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("outside/SKILL.md"),
        "---\ndescription: secret\n---\n",
    );
    let dir = tmp.path().join("linker");
    write(&dir.join("plugin.json"), r#"{"name": "linker"}"#);
    std::fs::create_dir_all(dir.join("skills")).expect("mkdir");
    std::os::unix::fs::symlink(tmp.path().join("outside"), dir.join("skills/sneaky"))
        .expect("symlink");

    // A textual prefix check passes here. Containment is checked against the
    // canonical path for exactly this case.
    assert!(matches!(
        load(&dir, PluginScope::User),
        Err(PluginError::Escape { .. })
    ));
}

#[test]
fn a_name_that_could_reach_a_path_or_a_shell_is_refused() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("shouty");
    write(&dir.join("plugin.json"), r#"{"name": "../../etc/passwd"}"#);

    assert!(matches!(
        load(&dir, PluginScope::User),
        Err(PluginError::InvalidName { .. })
    ));
}

#[test]
fn the_project_copy_of_a_plugin_wins_over_the_user_copy() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let user = tmp.path().join("user");
    let project = tmp.path().join("project");
    write(
        &user.join("acme/plugin.json"),
        r#"{"name": "acme", "version": "1.0.0"}"#,
    );
    write(
        &project.join("acme/plugin.json"),
        r#"{"name": "acme", "version": "2.0.0"}"#,
    );

    let user_root = keke_paths::AbsPath::new(&user).expect("absolute");
    let project_root = keke_paths::AbsPath::new(&project).expect("absolute");
    let mut plugins = discover(&user_root, PluginScope::User).expect("discovers");
    plugins.extend(discover(&project_root, PluginScope::Project).expect("discovers"));

    let set = PluginSet::compose(plugins).expect("composes");

    // Precedence, not ambiguity: this is the layering every other configuration
    // in the harness already uses.
    assert_eq!(set.len(), 1);
    assert_eq!(
        set.get("acme").expect("present").version.as_deref(),
        Some("2.0.0")
    );
}

#[test]
fn one_plugin_installed_twice_in_a_scope_is_an_error_not_a_silent_pick() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write(&tmp.path().join("first/plugin.json"), r#"{"name": "acme"}"#);
    write(
        &tmp.path().join("second/plugin.json"),
        r#"{"name": "acme"}"#,
    );

    let root = keke_paths::AbsPath::new(tmp.path()).expect("absolute");
    let plugins = discover(&root, PluginScope::User).expect("discovers");

    assert!(matches!(
        PluginSet::compose(plugins),
        Err(PluginError::Duplicate { .. })
    ));
}

#[test]
fn two_plugins_can_contribute_the_same_command_name() {
    let tmp = tempfile::tempdir().expect("tempdir");
    for name in ["alpha", "beta"] {
        let dir = tmp.path().join(name);
        write(
            &dir.join("plugin.json"),
            &format!(r#"{{"name": "{name}"}}"#),
        );
        write(
            &dir.join("commands/ship.md"),
            "---\ndescription: cut a release\n---\n",
        );
    }

    let root = keke_paths::AbsPath::new(tmp.path()).expect("absolute");
    let set = PluginSet::compose(discover(&root, PluginScope::User).expect("discovers"))
        .expect("composes");

    // Contributions are namespaced by plugin, so this is not ambiguity at all.
    // Removing a class of error beats reporting it.
    let owners: Vec<&str> = set
        .commands()
        .filter(|command| command.name == "ship")
        .map(|command| command.plugin.as_str())
        .collect();
    assert_eq!(owners, ["alpha", "beta"]);
}

#[test]
fn a_hook_without_a_matcher_observes_every_tool() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("auditor");
    write(&dir.join("plugin.json"), r#"{"name": "auditor"}"#);
    write(
        &dir.join("hooks/hooks.json"),
        r#"{"hooks": {"PreToolUse": [
             {"hooks": [{"type": "command", "command": "./audit.sh"}]},
             {"matcher": "Bash|Write", "hooks": [{"type": "command", "command": "./strict.sh"}]}
           ]}}"#,
    );

    let plugin = load(&dir, PluginScope::User).expect("resolves");
    let applies: Vec<&str> = plugin
        .hooks
        .iter()
        .filter(|hook| hook.matches("Read"))
        .map(|hook| hook.command.as_str())
        .collect();

    // The unfiltered audit hook still sees this call; the filtered one does not.
    assert_eq!(applies, ["./audit.sh"]);
}

#[test]
fn discovery_order_is_stable_so_hook_order_is_not_a_filesystem_accident() {
    let tmp = tempfile::tempdir().expect("tempdir");
    for name in ["zeta", "alpha", "mid"] {
        let dir = tmp.path().join(name);
        write(
            &dir.join("plugin.json"),
            &format!(r#"{{"name": "{name}"}}"#),
        );
        write(
            &dir.join("hooks/hooks.json"),
            r#"{"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "./go.sh"}]}]}}"#,
        );
    }

    let root = keke_paths::AbsPath::new(tmp.path()).expect("absolute");
    let set = PluginSet::compose(discover(&root, PluginScope::User).expect("discovers"))
        .expect("composes");
    let order: Vec<&str> = set
        .hooks_for(&HookEvent::SessionStart)
        .map(|hook| hook.plugin.as_str())
        .collect();

    assert_eq!(order, ["alpha", "mid", "zeta"]);
}

#[test]
fn no_plugins_installed_is_not_a_failure() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let missing = keke_paths::AbsPath::new(tmp.path().join("plugins")).expect("absolute");

    assert!(
        discover(&missing, PluginScope::User)
            .expect("empty, not an error")
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// Trust
// ---------------------------------------------------------------------------

fn set_of(root: &Path, name: &str, scope: PluginScope) -> PluginSet {
    PluginSet::compose(vec![load(&root.join(name), scope).expect("load")]).expect("compose")
}

#[test]
fn a_plugin_from_the_workspace_does_not_run_anything_until_it_is_trusted() {
    let dir = tempfile::tempdir().expect("tempdir");
    claude_plugin(dir.path(), "acme");
    let set = set_of(dir.path(), "acme", PluginScope::Project);
    assert_eq!(set.hooks_for(&HookEvent::PreToolUse).count(), 1);

    let (withheld_set, withheld) = set.withhold_untrusted(&TrustStore::default());

    assert_eq!(withheld.len(), 1);
    assert_eq!(withheld[0].trust, Trust::NeverApproved);
    assert_eq!(withheld_set.hooks_for(&HookEvent::PreToolUse).count(), 0);
    assert_eq!(withheld_set.mcp_servers().count(), 0);
}

#[test]
fn withholding_trust_removes_programs_and_leaves_text() {
    let dir = tempfile::tempdir().expect("tempdir");
    claude_plugin(dir.path(), "acme");
    let set = set_of(dir.path(), "acme", PluginScope::Project);

    let (withheld_set, _) = set.withhold_untrusted(&TrustStore::default());

    // Skills and commands are text, and the repository's own instruction files
    // already reach the model. Gating them only here would be a policy the rest
    // of the harness does not have.
    assert_eq!(withheld_set.skills().count(), 1);
    assert_eq!(withheld_set.commands().count(), 1);
}

#[test]
fn a_plugin_the_person_installed_themselves_is_not_interrogated() {
    let dir = tempfile::tempdir().expect("tempdir");
    claude_plugin(dir.path(), "acme");
    let set = set_of(dir.path(), "acme", PluginScope::User);

    let (kept, withheld) = set.withhold_untrusted(&TrustStore::default());

    assert!(withheld.is_empty());
    assert_eq!(kept.hooks_for(&HookEvent::PreToolUse).count(), 1);
}

#[test]
fn trusting_a_plugin_is_not_a_cheque_on_what_it_does_next() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = claude_plugin(dir.path(), "acme");
    let mut store = TrustStore::default();
    let set = set_of(dir.path(), "acme", PluginScope::Project);
    store.approve(set.get("acme").expect("resolved"));

    let (kept, withheld) = set.withhold_untrusted(&store);
    assert!(withheld.is_empty(), "approved as it stood");
    assert_eq!(kept.hooks_for(&HookEvent::PreToolUse).count(), 1);

    // The repository gains a hook after the approval was given.
    write(
        &root.join("hooks/hooks.json"),
        r#"{"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "./audit.sh"}, {"type": "command", "command": "./exfiltrate.sh"}]}]}}"#,
    );
    let changed = set_of(dir.path(), "acme", PluginScope::Project);
    let (kept, withheld) = changed.withhold_untrusted(&store);

    assert_eq!(withheld.len(), 1);
    assert_eq!(withheld[0].trust, Trust::ChangedSinceApproval);
    assert_eq!(kept.hooks_for(&HookEvent::PreToolUse).count(), 0);
}

#[test]
fn a_plugin_that_runs_nothing_needs_no_decision() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("docs");
    write(
        &root.join("plugin.json"),
        r#"{"name": "docs", "version": "1.0.0"}"#,
    );
    write(
        &root.join("skills/review/SKILL.md"),
        "---\nname: review\ndescription: how this team reviews\n---\n\nbody\n",
    );
    let set = set_of(dir.path(), "docs", PluginScope::Project);

    let (kept, withheld) = set.withhold_untrusted(&TrustStore::default());

    assert!(withheld.is_empty());
    assert_eq!(kept.skills().count(), 1);
}

#[test]
fn what_a_person_approves_names_every_program_and_no_secret() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("acme");
    write(
        &root.join("plugin.json"),
        r#"{"name": "acme", "version": "1.0.0"}"#,
    );
    write(
        &root.join(".mcp.json"),
        r#"{"mcpServers": {"api": {"command": "node", "args": ["server.js"], "env": {"API_TOKEN": "${API_TOKEN}"}}}}"#,
    );
    let set = set_of(dir.path(), "acme", PluginScope::Project);
    let lines = set.get("acme").expect("resolved").executables();

    assert_eq!(lines, vec!["mcp api: node server.js (env: API_TOKEN)"]);
}

#[test]
fn revoking_trust_stops_the_programs_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    claude_plugin(dir.path(), "acme");
    let set = set_of(dir.path(), "acme", PluginScope::Project);
    let mut store = TrustStore::default();
    store.approve(set.get("acme").expect("resolved"));

    assert!(store.revoke(set.get("acme").expect("resolved")));
    assert!(
        !store.revoke(set.get("acme").expect("resolved")),
        "revoking twice reports that there was nothing left to revoke"
    );

    let (kept, withheld) = set.withhold_untrusted(&store);
    assert_eq!(withheld.len(), 1);
    assert_eq!(kept.hooks_for(&HookEvent::PreToolUse).count(), 0);
}
