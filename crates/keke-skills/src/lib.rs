//! Plugin-contributed skills, as model-visible context.
//!
//! A skill is a prompt fragment a plugin ships. The whole design problem is
//! that skills are cheap to install and expensive to carry: a person with
//! twenty installed plugins cannot afford twenty bodies in every request. So
//! only the one-line descriptions go up front, and a body is read when the
//! model decides the skill is relevant. That is why `description` is required
//! in the manifest — a skill whose relevance cannot be judged without loading
//! it defeats the arrangement entirely.
//!
//! Nothing here knows what a plugin is beyond a resolved [`PluginSet`]. The
//! engine sees an ordinary `ContextContributor`.

//! Turns plugin-contributed skills into model-visible context.
//!
//! A skill (`skills/<name>/SKILL.md` in a data-plugin) is a prompt fragment the
//! model may want, but its body is not injected up front — only its
//! `plugin:name — description` line is. That is the entire reason the resolved
//! manifest requires a description: relevance has to be judgeable without
//! spending the context window on every skill's body, every turn. The model
//! reads a skill's body on demand, by asking for its qualified name.
//!
//! This crate is a thin translation from `keke_plugin::PluginSet` (data,
//! already resolved and inert) to a [`keke_plugin_api::ContextContributor`]
//! (behavior, wired into the session by the composition root). It contains no
//! plugin-format parsing of its own.

use std::sync::Arc;

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
                "- {}:{} — {} ({})\n",
                skill.plugin, skill.name, skill.description, skill.path
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
pub fn install(registry: &mut ExtensionRegistryBuilder, plugins: &PluginSet) {
    let skills: Vec<ResolvedSkill> = plugins.skills().cloned().collect();
    registry.context_contributor(Arc::new(SkillsContributor { skills }));
}

/// Load a skill's body by its qualified `plugin:name`, with the YAML
/// frontmatter stripped — the body is what the model asked to read, not the
/// metadata that was already summarized in the index fragment.
///
/// `qualified` must name a skill present in `plugins`; anything else is
/// refused without touching the filesystem, since a qualified name reaching
/// here may have been chosen by the model.
pub async fn read_skill_body(plugins: &PluginSet, qualified: &str) -> Result<String, SkillError> {
    let path = plugins
        .skills()
        .find(|skill| format!("{}:{}", skill.plugin, skill.name) == qualified)
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

    Ok(strip_frontmatter(&text).to_string())
}

/// Strip a leading `---\n ... \n---` YAML block, returning the body that
/// follows it. Text with no frontmatter is returned unchanged.
fn strip_frontmatter(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("---") else {
        return text;
    };
    let Some(end) = rest.find("\n---") else {
        return text;
    };
    // Skip the closing `---` line itself, plus its trailing newline if present.
    let after = &rest[end + 4..];
    let after = after.strip_prefix('\n').unwrap_or(after);
    after.trim_start_matches('\n')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_is_stripped_leaving_only_the_body() {
        let text = "---\nname: review\ndescription: how we review\n---\n\nBody text.\n";
        assert_eq!(strip_frontmatter(text), "Body text.\n");
    }

    #[test]
    fn text_without_frontmatter_passes_through() {
        let text = "Body text.\n";
        assert_eq!(strip_frontmatter(text), "Body text.\n");
    }
}
