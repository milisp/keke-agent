//! Each test names a rule from `AGENTS.md` or the crate's own prose and would
//! fail if the rule broke, however the code was rearranged.
//!
//! An integration test is not `#[cfg(test)]`, so the clippy allowance for
//! panicking in tests does not reach it. Same waiver the other suites take.
#![allow(clippy::expect_used, clippy::unwrap_used)]
#![cfg(unix)]

use std::path::Path;

use keke_config_types::PluginTimeouts;
use keke_plugin::PluginScope;
use keke_plugin::PluginSet;
use keke_plugin::load;
use keke_plugin_api::ExtensionContext;
use keke_plugin_api::ExtensionRegistry;
use keke_plugin_api::ExtensionRegistryBuilder;
use keke_protocol::SessionId;
use keke_protocol::ThreadId;
use keke_protocol::ToolCall;
use keke_protocol::ToolCallId;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("has a parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

fn script(root: &Path, name: &str, body: &str) {
    let path = root.join(name);
    write(&path, body);
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// A plugin declaring one hook, in the ecosystem's own file layout.
fn plugin(root: &Path, name: &str, event: &str, matcher: &str, command: &str, timeout: &str) {
    let dir = root.join(name);
    write(
        &dir.join("plugin.json"),
        &format!(r#"{{"name": "{name}", "version": "1.0.0"}}"#),
    );
    write(
        &dir.join("hooks/hooks.json"),
        &format!(
            r#"{{"hooks": {{"{event}": [{{"matcher": "{matcher}", "hooks": [{{"type": "command", "command": "{command}"{timeout}}}]}}]}}}}"#
        ),
    );
}

fn plugin_set(root: &Path, names: &[&str]) -> PluginSet {
    let plugins = names
        .iter()
        .map(|name| load(&root.join(name), PluginScope::User).expect("load"))
        .collect();
    PluginSet::compose(plugins).expect("compose")
}

fn call(tool: &str, id: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::new(id),
        name: tool.to_string(),
        arguments: serde_json::json!({"command": "rm -rf /"}),
    }
}

fn context() -> ExtensionContext {
    ExtensionContext::new(SessionId::new(), ThreadId::new())
}

/// Install the runner and run the tool-start point the engine runs before any
/// guard is consulted.
async fn registry_after_starting(plugins: &PluginSet, calls: &[ToolCall]) -> ExtensionRegistry {
    let mut builder = ExtensionRegistryBuilder::new();
    keke_hooks::install(&mut builder, plugins);
    let registry = builder.build();
    let ctx = context();
    for contributor in registry.tool_lifecycle_contributors() {
        for call in calls {
            contributor.on_tool_start(&ctx, call).await;
        }
    }
    registry
}

#[tokio::test]
async fn a_pre_tool_use_hook_exiting_non_zero_denies_the_call() {
    let dir = tempfile::tempdir().expect("tempdir");
    plugin(
        dir.path(),
        "auditor",
        "PreToolUse",
        "Bash",
        "echo no shells here; exit 1",
        "",
    );
    let plugins = plugin_set(dir.path(), &["auditor"]);
    let registry = registry_after_starting(&plugins, &[call("Bash", "c1")]).await;

    assert_eq!(
        registry.first_denial(&call("Bash", "c1")).as_deref(),
        Some("no shells here")
    );
}

#[tokio::test]
async fn a_permissive_hook_cannot_undo_a_denying_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Named so that one sorts before the denier and one after: no ordering of
    // hooks may turn the denial back into permission.
    plugin(dir.path(), "aaa-allow", "PreToolUse", "Bash", "exit 0", "");
    plugin(
        dir.path(),
        "mmm-deny",
        "PreToolUse",
        "Bash",
        "echo denied; exit 1",
        "",
    );
    plugin(dir.path(), "zzz-allow", "PreToolUse", "Bash", "exit 0", "");
    let plugins = plugin_set(dir.path(), &["aaa-allow", "mmm-deny", "zzz-allow"]);
    let registry = registry_after_starting(&plugins, &[call("Bash", "c1")]).await;

    assert_eq!(
        registry.first_denial(&call("Bash", "c1")).as_deref(),
        Some("denied")
    );
}

#[tokio::test]
async fn a_hook_exiting_zero_does_not_allow_what_another_guard_denies() {
    let dir = tempfile::tempdir().expect("tempdir");
    plugin(dir.path(), "permissive", "PreToolUse", "Bash", "exit 0", "");
    let plugins = plugin_set(dir.path(), &["permissive"]);

    let mut builder = ExtensionRegistryBuilder::new();
    keke_hooks::install(&mut builder, &plugins);
    builder.tool_guard(Box::new(|call| {
        (call.name == "Bash").then(|| "shell is disabled".to_string())
    }));
    let registry = builder.build();
    let ctx = context();
    for contributor in registry.tool_lifecycle_contributors() {
        contributor.on_tool_start(&ctx, &call("Bash", "c1")).await;
    }

    assert_eq!(
        registry.first_denial(&call("Bash", "c1")).as_deref(),
        Some("shell is disabled")
    );
}

#[tokio::test]
async fn a_denial_with_nothing_to_say_still_denies() {
    let dir = tempfile::tempdir().expect("tempdir");
    plugin(dir.path(), "silent", "PreToolUse", "Bash", "exit 3", "");
    let plugins = plugin_set(dir.path(), &["silent"]);
    let registry = registry_after_starting(&plugins, &[call("Bash", "c1")]).await;

    let denial = registry.first_denial(&call("Bash", "c1"));
    assert!(denial.is_some_and(|reason| !reason.trim().is_empty()));
}

#[tokio::test]
async fn a_hook_that_runs_past_its_timeout_denies() {
    let dir = tempfile::tempdir().expect("tempdir");
    plugin(
        dir.path(),
        "slow",
        "PreToolUse",
        "Bash",
        "sleep 30",
        r#", "timeout": 1"#,
    );
    let plugins = plugin_set(dir.path(), &["slow"]);
    let registry = registry_after_starting(&plugins, &[call("Bash", "c1")]).await;

    assert!(registry.first_denial(&call("Bash", "c1")).is_some());
}

#[tokio::test]
async fn a_tool_call_whose_hooks_never_ran_is_denied() {
    let dir = tempfile::tempdir().expect("tempdir");
    plugin(dir.path(), "auditor", "PreToolUse", "Bash", "exit 0", "");
    let plugins = plugin_set(dir.path(), &["auditor"]);
    // Only `c1` is decided; `c2` reaches the guard undecided.
    let registry = registry_after_starting(&plugins, &[call("Bash", "c1")]).await;

    assert!(registry.first_denial(&call("Bash", "c1")).is_none());
    assert!(registry.first_denial(&call("Bash", "c2")).is_some());
}

#[tokio::test]
async fn one_tool_call_cannot_read_another_calls_verdict() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Denies only the call whose arguments the hook was shown.
    script(
        dir.path(),
        "check.sh",
        "#!/bin/sh\ngrep -q '\"tool_call_id\":\"denied\"' && { echo caught; exit 1; }\nexit 0\n",
    );
    let checker = dir.path().join("check.sh").display().to_string();
    plugin(dir.path(), "auditor", "PreToolUse", "Bash", &checker, "");
    let plugins = plugin_set(dir.path(), &["auditor"]);
    let registry =
        registry_after_starting(&plugins, &[call("Bash", "denied"), call("Bash", "clean")]).await;

    assert_eq!(
        registry.first_denial(&call("Bash", "denied")).as_deref(),
        Some("caught")
    );
    assert!(registry.first_denial(&call("Bash", "clean")).is_none());
}

#[tokio::test]
async fn a_hook_sees_the_call_it_is_deciding_on() {
    let dir = tempfile::tempdir().expect("tempdir");
    script(
        dir.path(),
        "inspect.sh",
        r#"#!/bin/sh
payload=$(cat)
case "$payload" in *'"tool_name":"Bash"'*) ;; *) exit 0 ;; esac
case "$payload" in *'rm -rf'*) echo saw the arguments; exit 1 ;; esac
exit 0
"#,
    );
    // Read back through the placeholder every published plugin was written for.
    plugin(
        dir.path(),
        "auditor",
        "PreToolUse",
        "Bash",
        "${CLAUDE_PLUGIN_ROOT}/../inspect.sh",
        "",
    );
    let plugins = plugin_set(dir.path(), &["auditor"]);
    let registry = registry_after_starting(&plugins, &[call("Bash", "c1")]).await;

    assert_eq!(
        registry.first_denial(&call("Bash", "c1")).as_deref(),
        Some("saw the arguments")
    );
}

#[tokio::test]
async fn a_hook_for_another_tool_has_no_say() {
    let dir = tempfile::tempdir().expect("tempdir");
    plugin(dir.path(), "auditor", "PreToolUse", "Bash", "exit 1", "");
    let plugins = plugin_set(dir.path(), &["auditor"]);
    let registry = registry_after_starting(&plugins, &[]).await;

    assert!(registry.first_denial(&call("read_file", "c1")).is_none());
}

#[tokio::test]
async fn a_hook_for_an_event_keke_does_not_implement_never_runs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let witness = dir.path().join("ran");
    plugin(
        dir.path(),
        "future",
        "PreCompact",
        "",
        &format!("touch {}", witness.display()),
        "",
    );
    let plugins = plugin_set(dir.path(), &["future"]);
    let registry = registry_after_starting(&plugins, &[call("Bash", "c1")]).await;
    let ctx = context();
    for contributor in registry.turn_lifecycle_contributors() {
        contributor
            .on_turn_start(&ctx, keke_protocol::TurnId::new())
            .await;
    }

    assert!(!witness.exists());
    assert!(registry.first_denial(&call("Bash", "c1")).is_none());
}

#[tokio::test]
async fn a_failing_turn_hook_cannot_stop_the_turn() {
    let dir = tempfile::tempdir().expect("tempdir");
    plugin(dir.path(), "noisy", "UserPromptSubmit", "", "exit 9", "");
    let plugins = plugin_set(dir.path(), &["noisy"]);
    let mut builder = ExtensionRegistryBuilder::new();
    keke_hooks::install(&mut builder, &plugins);
    let registry = builder.build();
    let ctx = context();

    for contributor in registry.turn_lifecycle_contributors() {
        contributor
            .on_turn_start(&ctx, keke_protocol::TurnId::new())
            .await;
        contributor
            .on_turn_end(
                &ctx,
                keke_protocol::TurnId::new(),
                &keke_protocol::StopReason::EndTurn,
            )
            .await;
    }

    // Observation-only failures are logged and nothing else; no call is denied.
    assert!(registry.first_denial(&call("Bash", "c1")).is_none());
}

#[tokio::test]
async fn a_post_tool_use_hook_sees_how_the_call_ended() {
    let dir = tempfile::tempdir().expect("tempdir");
    let receipt = dir.path().join("receipt");
    plugin(
        dir.path(),
        "recorder",
        "PostToolUse",
        "",
        &format!("cat > {}", receipt.display()),
        "",
    );
    let plugins = plugin_set(dir.path(), &["recorder"]);

    let mut builder = ExtensionRegistryBuilder::new();
    keke_hooks::install(&mut builder, &plugins);
    let registry = builder.build();
    let ctx = context();
    let call = call("Bash", "c1");
    for contributor in registry.tool_lifecycle_contributors() {
        contributor.on_tool_start(&ctx, &call).await;
        contributor.on_tool_finish(&ctx, &call, Ok(())).await;
    }

    let payload = std::fs::read_to_string(&receipt).expect("the hook ran");
    let payload: serde_json::Value = serde_json::from_str(&payload).expect("json");
    assert_eq!(payload["hook_event_name"], "PostToolUse");
    assert_eq!(payload["tool_name"], "Bash");
    assert_eq!(payload["success"], true);
}

#[tokio::test]
async fn a_finished_call_stops_holding_its_verdict() {
    let dir = tempfile::tempdir().expect("tempdir");
    plugin(dir.path(), "denier", "PreToolUse", "Bash", "exit 1", "");
    let plugins = plugin_set(dir.path(), &["denier"]);

    let mut builder = ExtensionRegistryBuilder::new();
    keke_hooks::install(&mut builder, &plugins);
    let registry = builder.build();
    let ctx = context();
    let call = call("Bash", "c1");
    for contributor in registry.tool_lifecycle_contributors() {
        contributor.on_tool_start(&ctx, &call).await;
    }
    assert!(registry.first_denial(&call).is_some());

    for contributor in registry.tool_lifecycle_contributors() {
        contributor.on_tool_finish(&ctx, &call, Ok(())).await;
    }

    // The verdict is gone, so the map does not grow for the life of the
    // session. And what a forgotten call falls back to is denial, never
    // permission — an unasked call is not a consenting one.
    assert!(registry.first_denial(&call).is_some());
}

#[tokio::test]
async fn a_post_tool_use_hook_only_sees_the_tools_it_asked_for() {
    let dir = tempfile::tempdir().expect("tempdir");
    let receipt = dir.path().join("receipt");
    plugin(
        dir.path(),
        "watcher",
        "PostToolUse",
        "Bash",
        &format!("cat > {}", receipt.display()),
        "",
    );
    let plugins = plugin_set(dir.path(), &["watcher"]);

    let mut builder = ExtensionRegistryBuilder::new();
    keke_hooks::install(&mut builder, &plugins);
    let registry = builder.build();
    let ctx = context();
    let other = call("Read", "c1");
    for contributor in registry.tool_lifecycle_contributors() {
        contributor.on_tool_finish(&ctx, &other, Ok(())).await;
    }
    assert!(
        !receipt.exists(),
        "a hook matching Bash must not report on a Read call"
    );

    let watched = call("Bash", "c2");
    for contributor in registry.tool_lifecycle_contributors() {
        contributor.on_tool_finish(&ctx, &watched, Ok(())).await;
    }
    assert!(receipt.exists(), "the hook still sees the tool it matched");
}

#[tokio::test]
async fn a_hook_declaring_no_timeout_still_runs_under_the_configured_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    plugin(dir.path(), "silent", "PreToolUse", "Bash", "sleep 30", "");
    let plugins = plugin_set(dir.path(), &["silent"]);

    let mut builder = ExtensionRegistryBuilder::new();
    keke_hooks::install_with(
        &mut builder,
        &plugins,
        PluginTimeouts {
            hook_millis: 150,
            ..PluginTimeouts::default()
        },
    );
    let registry = builder.build();
    let ctx = context();
    let call = call("Bash", "c1");
    for contributor in registry.tool_lifecycle_contributors() {
        contributor.on_tool_start(&ctx, &call).await;
    }

    assert!(
        registry.first_denial(&call).is_some(),
        "a hook that never answers must not be able to hold the turn open"
    );
}
