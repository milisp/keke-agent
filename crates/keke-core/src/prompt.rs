//! Turn context assembly.
//!
//! Everything assembled here reaches the model, so everything assembled here is
//! logged as part of `SessionEvent::ModelRequest`. Nothing may be added to a
//! request outside this module without also appearing in that event.

use keke_plugin_api::ContextFragment;
use keke_plugin_api::ExtensionContext;
use keke_plugin_api::ExtensionRegistry;
use keke_workspace::Workspace;

/// Order slots, so a fragment's position in the prompt is a property of the
/// fragment rather than an accident of registration order.
pub const ORDER_IDENTITY: i32 = -100;
pub const ORDER_PROJECT: i32 = 0;
pub const ORDER_ENVIRONMENT: i32 = 50;

const IDENTITY: &str = "\
You are keke, a coding agent working in a terminal. You have tools for reading, \
searching, and modifying files, and for running shell commands. Prefer using a \
tool to inspect the project over guessing. Be concise.";

/// Build the system prompt for a turn.
///
/// Fragments are sorted by `order` then by name, so assembly is deterministic:
/// the same inputs must produce the same prompt, or replay diverges.
pub async fn assemble_system_prompt(
    workspace: &Workspace,
    cwd: &std::path::Path,
    registry: &ExtensionRegistry,
    ext_ctx: &ExtensionContext,
) -> String {
    let mut fragments = vec![ContextFragment::new("identity", ORDER_IDENTITY, IDENTITY)];

    if let Ok(instructions) = workspace.instructions(cwd) {
        for (index, file) in instructions.iter().enumerate() {
            let name = workspace
                .relativize(&file.path)
                .map(|rel| rel.to_string())
                .unwrap_or_else(|_| file.path.to_string());
            fragments.push(ContextFragment::new(
                format!("project-instructions/{name}"),
                ORDER_PROJECT + index as i32,
                format!("Instructions from {name}:\n\n{}", file.text),
            ));
        }
    }

    fragments.push(ContextFragment::new(
        "environment",
        ORDER_ENVIRONMENT,
        environment_fragment(workspace, cwd),
    ));

    for contributor in registry.context_contributors() {
        fragments.extend(contributor.contribute_turn_context(ext_ctx).await);
    }

    fragments.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.name.cmp(&b.name)));
    fragments
        .into_iter()
        .map(|fragment| fragment.text)
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// What the model needs to know about where it is running.
fn environment_fragment(workspace: &Workspace, cwd: &std::path::Path) -> String {
    let mut lines = vec![
        "Environment:".to_string(),
        format!("  workspace root: {}", workspace.root()),
        format!("  cwd: {}", cwd.display()),
        format!("  platform: {}", std::env::consts::OS),
    ];

    if let Some(status) = keke_workspace::vcs_status(workspace.root()) {
        if let Some(branch) = status.branch {
            lines.push(format!("  git branch: {branch}"));
        }
        if status.changes.is_empty() {
            lines.push("  git status: clean".to_string());
        } else {
            lines.push(format!("  git status: {} change(s)", status.changes.len()));
            for change in &status.changes {
                lines.push(format!("    {change}"));
            }
            if status.truncated > 0 {
                lines.push(format!("    ... and {} more", status.truncated));
            }
        }
    }

    lines.join("\n")
}
