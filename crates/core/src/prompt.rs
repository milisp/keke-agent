//! Turn context assembly.
//!
//! Everything assembled here reaches the model, so everything assembled here is
//! logged — each fragment as a `SessionEvent::ContextFragment`, because the
//! system prompt they are joined into is not what `SessionEvent::ModelRequest`
//! carries. Nothing may be added to a request outside this module without also
//! appearing in the log.
//!
//! The recording happens here rather than in each contributor on purpose. A
//! contributor that forgot would put text in front of the model that the log
//! cannot account for, and invariant 6 would then hold only as far as every
//! extension author remembered it — which is the failure mode the invariant
//! exists to prevent.

use keke_plugin_api::ContextFragment;
use keke_plugin_api::ExtensionContext;
use keke_plugin_api::ExtensionRegistry;
use keke_protocol::SessionEvent;
use keke_workspace::Workspace;

/// Order slots, so a fragment's position in the prompt is a property of the
/// fragment rather than an accident of registration order.
pub const ORDER_IDENTITY: i32 = -100;
/// Between the two on purpose: a caller's persona refines what keke is, and
/// the project it is pointed at still gets the last word over the persona.
pub const ORDER_PERSONA: i32 = -50;
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
    instructions: Option<&str>,
    registry: &ExtensionRegistry,
    ext_ctx: &ExtensionContext,
) -> String {
    let mut fragments = vec![ContextFragment::new("identity", ORDER_IDENTITY, IDENTITY)];

    // Blank is the same as absent: a caller that passes an empty persona meant
    // to say nothing, and an empty fragment would still cost a blank line in
    // front of the model and a fragment in the log accounting for it.
    if let Some(persona) = instructions.map(str::trim).filter(|text| !text.is_empty()) {
        fragments.push(ContextFragment::new("persona", ORDER_PERSONA, persona));
    }

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

    // In assembled order, so the log reads the way the prompt does.
    for fragment in &fragments {
        ext_ctx.record(SessionEvent::ContextFragment {
            turn: ext_ctx.turn(),
            name: fragment.name.clone(),
            text: fragment.text.clone(),
        });
    }

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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use keke_plugin_api::ContextContributor;
    use keke_plugin_api::ExtFuture;
    use keke_plugin_api::ExtensionRegistryBuilder;
    use keke_protocol::SessionId;
    use keke_protocol::ThreadId;

    use super::*;

    struct Says(&'static str);

    impl ContextContributor for Says {
        fn contribute_turn_context<'a>(
            &'a self,
            _ctx: &'a ExtensionContext,
        ) -> ExtFuture<'a, Vec<ContextFragment>> {
            let text = self.0;
            Box::pin(async move { vec![ContextFragment::new("extra", 1000, text)] })
        }
    }

    async fn assemble(registry: &ExtensionRegistry, ext_ctx: &ExtensionContext) -> String {
        assemble_with(None, registry, ext_ctx).await
    }

    /// The same assembly, with a project instruction file on disk, so a test
    /// can say where a persona lands relative to what the repository asked for.
    async fn assemble_with(
        instructions: Option<&str>,
        registry: &ExtensionRegistry,
        ext_ctx: &ExtensionContext,
    ) -> String {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("AGENTS.md"), "the project speaks").expect("write");
        let root = keke_paths::AbsPath::new(dir.path()).expect("absolute");
        let workspace = Workspace::new(root);
        assemble_system_prompt(&workspace, dir.path(), instructions, registry, ext_ctx).await
    }

    /// A caller running several named agents from one installation gives each
    /// its own character, and the repository still gets the last word: the
    /// persona is read before the instructions the project wrote.
    #[tokio::test]
    async fn a_persona_leads_the_project_it_is_pointed_at() {
        let registry = ExtensionRegistryBuilder::new().build();
        let ext_ctx = ExtensionContext::new(SessionId::new(), ThreadId::new());

        let prompt = assemble_with(Some("You are Ada, and you review."), &registry, &ext_ctx).await;

        let persona = prompt
            .find("You are Ada")
            .expect("the persona reached the prompt");
        let project = prompt
            .find("the project speaks")
            .expect("project instructions");
        let identity = prompt.find("You are keke").expect("keke's own identity");
        assert!(
            identity < persona && persona < project,
            "a persona refines keke's identity and is refined by the project"
        );
        assert!(
            ext_ctx.drain_events().into_iter().any(|event| matches!(
                event,
                keke_protocol::SessionEvent::ContextFragment { ref name, .. } if name == "persona"
            )),
            "invariant 6: a persona is model-visible, so it is logged"
        );
    }

    /// A caller with nothing to say passes an empty string as readily as it
    /// passes nothing, and the two must not produce different prompts.
    #[tokio::test]
    async fn a_blank_persona_is_the_same_as_none() {
        let registry = ExtensionRegistryBuilder::new().build();
        let blank = ExtensionContext::new(SessionId::new(), ThreadId::new());
        let absent = ExtensionContext::new(SessionId::new(), ThreadId::new());
        // One directory for both, because the environment fragment names it and
        // two tempdirs would differ for a reason this test is not about.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = keke_paths::AbsPath::new(dir.path()).expect("absolute");
        let workspace = Workspace::new(root);

        assert_eq!(
            assemble_system_prompt(&workspace, dir.path(), Some("   \n  "), &registry, &blank)
                .await,
            assemble_system_prompt(&workspace, dir.path(), None, &registry, &absent).await
        );
    }

    /// Invariant 6: what an extension puts in front of the model reaches the
    /// request inside the *system* prompt, which `SessionEvent::ModelRequest`
    /// does not carry. Recorded here rather than by each contributor, so a
    /// contributor cannot leave the log unable to account for the prompt.
    #[tokio::test]
    async fn every_assembled_fragment_is_logged() {
        let mut builder = ExtensionRegistryBuilder::new();
        builder.context_contributor(Arc::new(Says("remember the milk")));
        let registry = builder.build();
        let ext_ctx = ExtensionContext::new(SessionId::new(), ThreadId::new());

        let prompt = assemble(&registry, &ext_ctx).await;

        let recorded: Vec<(String, String)> = ext_ctx
            .drain_events()
            .into_iter()
            .filter_map(|event| match event {
                keke_protocol::SessionEvent::ContextFragment { name, text, .. } => {
                    Some((name, text))
                }
                _ => None,
            })
            .collect();
        for (_, text) in &recorded {
            assert!(
                prompt.contains(text.as_str()),
                "a fragment was logged that never reached the prompt"
            );
        }
        assert!(
            recorded
                .iter()
                .any(|(name, text)| name == "extra" && text == "remember the milk"),
            "a contributor's fragment must be in the log, not only in the prompt"
        );
        assert!(
            recorded.iter().any(|(name, _)| name == "identity"),
            "the engine's own fragments are model-visible too"
        );
    }

    /// The log must read the way the prompt does, or replaying it reconstructs
    /// a prompt the model never saw.
    #[tokio::test]
    async fn fragments_are_logged_in_the_order_they_were_assembled() {
        let registry = ExtensionRegistryBuilder::new().build();
        let ext_ctx = ExtensionContext::new(SessionId::new(), ThreadId::new());

        let prompt = assemble(&registry, &ext_ctx).await;

        let texts: Vec<String> = ext_ctx
            .drain_events()
            .into_iter()
            .filter_map(|event| match event {
                keke_protocol::SessionEvent::ContextFragment { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(texts.join("\n\n"), prompt);
    }
}
