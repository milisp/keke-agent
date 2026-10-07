//! Skills as model-visible context, with bodies read only on demand.
//!
//! Native directory skills use bare names and plugin packages keep their
//! namespace. Selection, precedence, and real-path deduplication share one
//! filter so the model index, slash commands, and body reader agree.
//! Discovery remains inert in `keke-plugin`; the composition root decides
//! which directories are scanned.

use std::sync::Arc;

use keke_config_types::SkillSelection;
use keke_plugin::PluginSet;
use keke_plugin::ResolvedSkill;
use keke_plugin_api::ContextContributor;
use keke_plugin_api::ContextFragment;
use keke_plugin_api::ExtFuture;
use keke_plugin_api::ExtensionContext;
use keke_plugin_api::ExtensionRegistryBuilder;

/// Order for the skills index fragment: tool guidance, not identity or persona.
/// See the convention documented on `ContextFragment::order`.
const SKILLS_INDEX_ORDER: i32 = 100;

/// Errors from [`read_skill_body`].
#[derive(Debug)]
pub enum SkillError {
    /// The qualified name does not name a skill in the set. Refused before
    /// touching the filesystem, so a model-supplied name can never be used to
    /// read an arbitrary path.
    Unknown { qualified: String },
    Read {
        path: String,
        source: std::io::Error,
    },
}

impl std::fmt::Display for SkillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown { qualified } => write!(f, "no skill named {qualified:?}"),
            Self::Read { path, source } => write!(f, "reading {path}: {source}"),
        }
    }
}

impl std::error::Error for SkillError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unknown { .. } => None,
            Self::Read { source, .. } => Some(source),
        }
    }
}

/// Contributes one index line per skill; bodies stay on disk until asked for.
struct SkillsContributor {
    skills: Vec<ResolvedSkill>,
}

impl SkillsContributor {
    /// One line per skill: `plugin:name — description`, plus how to load it.
    fn index_text(&self) -> String {
        // Naming a tool here would be a promise this crate cannot keep — it
        // contributes no tools. The path is enough: the built-in file tools
        // can read it, and pointing at a tool that may not be installed is
        // how a model ends up reporting a failure that is really our error.
        let mut text = String::from(
            "Skills available this session. Each line is a summary only. Read a \
             skill's file at the listed path before following it, and only when \
             it looks relevant to the current task.\n\n",
        );
        for skill in &self.skills {
            text.push_str(&format!(
                "- {} — {} ({})\n",
                skill.qualified_name(),
                skill.description,
                skill.path
            ));
        }
        text
    }
}

impl ContextContributor for SkillsContributor {
    fn contribute_turn_context<'a>(
        &'a self,
        _ctx: &'a ExtensionContext,
    ) -> ExtFuture<'a, Vec<ContextFragment>> {
        Box::pin(async move {
            // No fragment at all when there are no skills: an empty section is
            // wasted context, not a neutral one.
            if self.skills.is_empty() {
                return Vec::new();
            }
            vec![ContextFragment::new(
                "skills-index",
                SKILLS_INDEX_ORDER,
                self.index_text(),
            )]
        })
    }
}

/// Register plugin-contributed skills as model-visible context.
///
/// Every skill a person did not turn off; see [`install_with`] when there is a
/// configured selection to honor.
pub fn install(registry: &mut ExtensionRegistryBuilder, plugins: &PluginSet) {
    install_with(registry, plugins, &SkillSelection::default());
}

/// Register the enabled skills as model-visible context.
///
/// A disabled skill is filtered out here rather than marked, because the index
/// fragment is the whole of what the model can see: a skill that is still
/// listed is one the model will still ask to read.
pub fn install_with(
    registry: &mut ExtensionRegistryBuilder,
    plugins: &PluginSet,
    selection: &SkillSelection,
) {
    let skills: Vec<ResolvedSkill> = enabled(plugins, selection).cloned().collect();
    registry.context_contributor(Arc::new(SkillsContributor { skills }));
}

/// The skills this deployment kept, resolved by scope and source precedence.
///
/// The one place the selection is applied, so what the model is told about,
/// what a surface offers, and what [`read_skill_body_with`] will open cannot
/// drift apart.
pub fn enabled<'a>(
    plugins: &'a PluginSet,
    selection: &'a SkillSelection,
) -> impl Iterator<Item = &'a ResolvedSkill> + 'a {
    let mut candidates: Vec<_> = plugins
        .plugins()
        .flat_map(|plugin| {
            plugin
                .skills
                .iter()
                .filter(|skill| !selection.is_disabled(&skill.plugin, &skill.name))
                .map(move |skill| (plugin.scope, skill))
        })
        .collect();
    // A project can specialize a person's skill, and native directories should
    // remain authoritative when a compatibility directory contains the same name.
    candidates.sort_by_key(|(scope, skill)| {
        (
            std::cmp::Reverse(*scope),
            !skill.native,
            match skill.plugin.as_str() {
                "workspace" | "local" => 0,
                "agents-workspace" | "agents" => 1,
                _ => 2,
            },
        )
    });
    let mut paths = std::collections::HashSet::new();
    let mut names = std::collections::HashSet::new();
    candidates.into_iter().filter_map(move |(_, skill)| {
        let path = std::fs::canonicalize(skill.path.as_path())
            .unwrap_or_else(|_| skill.path.as_path().to_path_buf());
        let name = skill.qualified_name();
        if paths.contains(&path) || names.contains(&name) {
            return None;
        }
        paths.insert(path);
        names.insert(name);
        Some(skill)
    })
}

/// Load a skill's body by its public name, with the YAML
/// frontmatter stripped — the body is what the model asked to read, not the
/// metadata that was already summarized in the index fragment.
///
/// `qualified` must name a skill present in `plugins`; anything else is
/// refused without touching the filesystem, since a qualified name reaching
/// here may have been chosen by the model.
pub async fn read_skill_body(plugins: &PluginSet, qualified: &str) -> Result<String, SkillError> {
    read_skill_body_with(plugins, &SkillSelection::default(), qualified).await
}

/// Load an enabled skill's body by its bare native or qualified plugin name.
///
/// A disabled skill is `Unknown` rather than a distinct refusal: to everything
/// downstream it is simply not a skill this session has, which is what keeps a
/// turned-off skill from being reachable by naming it exactly.
pub async fn read_skill_body_with(
    plugins: &PluginSet,
    selection: &SkillSelection,
    qualified: &str,
) -> Result<String, SkillError> {
    let path = enabled(plugins, selection)
        .find(|skill| skill.qualified_name() == qualified)
        .map(|skill| skill.path.clone())
        .ok_or_else(|| SkillError::Unknown {
            qualified: qualified.to_string(),
        })?;

    let text = tokio::fs::read_to_string(path.as_path())
        .await
        .map_err(|source| SkillError::Read {
            path: path.to_string(),
            source,
        })?;

    Ok(keke_plugin::markdown_body(&text).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The metadata was summarized into the index line already; sending it
    /// again is context spent on something the reader has acted on.
    #[test]
    fn frontmatter_is_stripped_leaving_only_the_body() {
        let text = "---\nname: review\ndescription: how we review\n---\n\nBody text.\n";
        assert_eq!(keke_plugin::markdown_body(text), "Body text.\n");
    }

    #[test]
    fn a_disabled_skill_is_not_offered_to_the_model() {
        let selection = SkillSelection::new(vec!["acme:review".to_string()]).expect("valid");
        assert!(selection.is_disabled("acme", "review"));
        assert!(!selection.is_disabled("other", "review"));
    }
    fn fixture(
        root: &std::path::Path,
        source: &str,
        scope: keke_plugin::PluginScope,
        body: &str,
    ) -> keke_plugin::ResolvedPlugin {
        let skill = root.join("skills/review");
        std::fs::create_dir_all(&skill).expect("skill directory");
        std::fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: review\ndescription: Review code\n---\n{body}"),
        )
        .expect("skill");
        keke_plugin::load_named(root, scope, true, Some(source)).expect("native directory")
    }

    #[test]
    fn native_names_are_bare_and_project_skills_override_user_skills() {
        let temp = tempfile::tempdir().expect("directory");
        let user = fixture(
            &temp.path().join("user"),
            "agents",
            keke_plugin::PluginScope::User,
            "user",
        );
        let project = fixture(
            &temp.path().join("project"),
            "workspace",
            keke_plugin::PluginScope::Project,
            "project",
        );
        let plugins = PluginSet::compose(vec![user, project]).expect("set");
        let selection = SkillSelection::default();
        let skills: Vec<_> = enabled(&plugins, &selection).collect();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].plugin, "workspace");
        assert_eq!(skills[0].qualified_name(), "review");
        assert_eq!(plugins.skills().count(), 2);
    }

    #[test]
    fn disabling_the_winning_source_reveals_the_other_native_skill() {
        let temp = tempfile::tempdir().expect("directory");
        let user = fixture(
            &temp.path().join("user"),
            "agents",
            keke_plugin::PluginScope::User,
            "user",
        );
        let project = fixture(
            &temp.path().join("project"),
            "workspace",
            keke_plugin::PluginScope::Project,
            "project",
        );
        let plugins = PluginSet::compose(vec![user, project]).expect("set");
        let selection = SkillSelection::new(vec!["workspace:review".into()]).expect("selection");
        let skills: Vec<_> = enabled(&plugins, &selection).collect();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].plugin, "agents");
    }

    #[test]
    fn the_same_file_is_offered_only_once_even_with_distinct_plugin_names() {
        let temp = tempfile::tempdir().expect("directory");
        let mut first = fixture(
            &temp.path().join("first"),
            "agents",
            keke_plugin::PluginScope::User,
            "body",
        );
        let mut second = fixture(
            &temp.path().join("second"),
            "local",
            keke_plugin::PluginScope::User,
            "body",
        );
        first.skills[0].native = false;
        second.skills[0].native = false;
        second.skills[0].path = first.skills[0].path.clone();
        let plugins = PluginSet::compose(vec![first, second]).expect("set");
        assert_eq!(enabled(&plugins, &SkillSelection::default()).count(), 1);
    }
    #[tokio::test]
    async fn native_skill_body_is_read_by_bare_name() {
        let temp = tempfile::tempdir().expect("directory");
        let plugin = fixture(
            temp.path(),
            "agents",
            keke_plugin::PluginScope::User,
            "Native body",
        );
        let plugins = PluginSet::compose(vec![plugin]).expect("set");
        assert_eq!(
            read_skill_body(&plugins, "review").await.expect("body"),
            "Native body"
        );
        assert!(matches!(
            read_skill_body(&plugins, "agents:review").await,
            Err(SkillError::Unknown { .. })
        ));
    }
}
