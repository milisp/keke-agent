//! Spawning one hook program and reading its answer.
//!
//! A hook is a shell command line the person installed, run with the platform
//! shell. This module is *not* a sandbox: it does not restrict the filesystem,
//! the network, or anything else the shell can reach. Installing a plugin is
//! trusting its hooks with everything the agent process itself has, and nothing
//! here weakens that or improves on it.

use std::process::Stdio;

use keke_plugin::ResolvedHook;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// What a hook said, when it managed to say anything.
pub(crate) struct Completed {
    pub(crate) success: bool,
    /// Trimmed stdout. The ecosystem's convention is that a denying hook
    /// explains itself here.
    pub(crate) stdout: String,
}

/// Run `hook` with `payload` on stdin, under `default_millis` when the hook
/// declares no budget of its own.
///
/// `Err` means the hook did not answer — it could not be spawned, or it ran
/// past its timeout. Callers decide what that means; for `PreToolUse` it means
/// denial, because a hook that cannot answer is not a hook that says yes.
pub(crate) async fn run(
    hook: &ResolvedHook,
    payload: &Value,
    default_millis: u64,
) -> Result<Completed, String> {
    let command = expand_plugin_root(&hook.command, &hook.plugin_root.to_string());
    let (program, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };

    let mut child = Command::new(program)
        .arg(flag)
        .arg(&command)
        .current_dir(hook.plugin_root.as_path())
        // Both spellings again as environment, so a hook can find its files
        // without the host having rewritten its command line.
        .env("CLAUDE_PLUGIN_ROOT", hook.plugin_root.as_path())
        .env("KEKE_PLUGIN_ROOT", hook.plugin_root.as_path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The timeout path drops this future; dropping it must not leave a hook
        // running behind the turn that gave up on it.
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("hook from plugin {} could not start: {error}", hook.plugin))?;

    if let Some(mut stdin) = child.stdin.take() {
        // A hook that ignores stdin closes it early; that is a broken pipe, not
        // a failure of the hook to answer.
        let encoded = payload.to_string();
        let _ = stdin.write_all(encoded.as_bytes()).await;
        let _ = stdin.shutdown().await;
    }

    // Every hook runs under a budget. The manifest's `timeout` is the hook
    // author's control; `default_millis` is the deployment's, a validated
    // `keke-config-types` field rather than a constant invented here
    // (`AGENTS.md` invariant 9). What there is no setting for is "no budget":
    // a hook runs before the tool it guards, so one that never returns does not
    // slow the turn down, it stops it.
    let budget = hook
        .timeout_seconds
        .map_or(default_millis, |seconds| seconds.saturating_mul(1_000));
    let wait = child.wait_with_output();
    let output = match tokio::time::timeout(std::time::Duration::from_millis(budget), wait).await {
        Ok(finished) => finished,
        Err(_) => {
            return Err(format!(
                "hook from plugin {} timed out after {budget}ms",
                hook.plugin
            ));
        }
    };

    let output = output.map_err(|error| {
        format!(
            "hook from plugin {} could not be waited on: {error}",
            hook.plugin
        )
    })?;

    Ok(Completed {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
    })
}

/// Substitute the plugin root for either spelling of its placeholder.
///
/// `CLAUDE_PLUGIN_ROOT` is the spelling every published plugin was written
/// against, so it is supported on equal terms with keke's own — a plugin should
/// not have to be edited to run here.
fn expand_plugin_root(command: &str, root: &str) -> String {
    command
        .replace("${CLAUDE_PLUGIN_ROOT}", root)
        .replace("${KEKE_PLUGIN_ROOT}", root)
}

#[cfg(test)]
mod tests {
    use super::expand_plugin_root;

    #[test]
    fn both_spellings_of_the_plugin_root_expand() {
        assert_eq!(
            expand_plugin_root("${CLAUDE_PLUGIN_ROOT}/a ${KEKE_PLUGIN_ROOT}/b", "/p"),
            "/p/a /p/b"
        );
    }
}
