//! Persisted skill choices shared by composition and the TUI.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result};
use keke_config_types::{HomeLayout, SkillSelection};

fn path(home: &HomeLayout) -> std::path::PathBuf {
    home.home.as_path().join("state/skills.json")
}

fn choices(home: &HomeLayout) -> Result<BTreeMap<String, bool>> {
    match std::fs::read_to_string(path(home)) {
        Ok(text) => serde_json::from_str(&text).context("reading saved skill choices"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(error).context("reading saved skill choices"),
    }
}

fn default_enabled(plugin: &keke_plugin::ResolvedPlugin) -> bool {
    plugin.owned || matches!(plugin.name.as_str(), "agents" | "agents-workspace")
}

/// Configuration denials remain authoritative over interactive choices.
pub(crate) fn selection(
    home: &HomeLayout,
    configured: &SkillSelection,
    plugins: &keke_plugin::PluginSet,
) -> Result<SkillSelection> {
    let choices = choices(home)?;
    let mut patterns = configured.patterns().to_vec();
    for plugin in plugins.plugins().filter(|plugin| !default_enabled(plugin)) {
        let source = &plugin.name;
        if !choices
            .get(&format!("{source}:*"))
            .copied()
            .unwrap_or(false)
        {
            patterns.push(format!("{source}:*"));
        }
    }
    patterns.extend(
        choices
            .into_iter()
            .filter_map(|(key, enabled)| (!enabled).then_some(key)),
    );
    SkillSelection::new(patterns).map_err(anyhow::Error::msg)
}

pub(crate) struct Manage {
    pub home: HomeLayout,
    pub configured: SkillSelection,
}

impl keke_tui::skills::SkillsManage for Manage {
    fn set_disabled(&self, source: &str, name: Option<&str>, disabled: bool) -> Result<(), String> {
        let update = || -> Result<()> {
            let plugins = crate::plugins::discover(&self.home)?;
            let known = plugins
                .skills()
                .any(|skill| skill.plugin == source && name.is_none_or(|name| skill.name == name));
            anyhow::ensure!(known, "this skill source is no longer available");
            let mut saved = choices(&self.home)?;
            saved.insert(format!("{source}:{}", name.unwrap_or("*")), !disabled);
            let destination = path(&self.home);
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let text = serde_json::to_vec_pretty(&saved)?;
            let mut temporary = tempfile::NamedTempFile::new_in(
                destination.parent().context("skill state has no parent")?,
            )?;
            std::io::Write::write_all(&mut temporary, &text)?;
            temporary.persist(destination)?;
            Ok(())
        };
        update().map_err(|error| error.to_string())
    }

    fn refresh(&self) -> Result<Vec<keke_tui::skills::SkillStatus>, String> {
        let read = || -> Result<Vec<keke_tui::skills::SkillStatus>> {
            let saved = choices(&self.home)?;
            let plugins = crate::plugins::discover(&self.home)?;
            let selection = selection(&self.home, &self.configured, &plugins)?;
            let active: Vec<_> = keke_skills::enabled(&plugins, &selection).collect();
            Ok(plugins
                .skills()
                .map(|skill| {
                    let source_enabled = saved
                        .get(&format!("{}:*", skill.plugin))
                        .copied()
                        .unwrap_or_else(|| plugins.get(&skill.plugin).is_some_and(default_enabled));
                    keke_tui::skills::SkillStatus {
                        name: skill.name.clone(),
                        source: skill.plugin.clone(),
                        description: skill.description.clone(),
                        path: skill.path.as_path().to_path_buf(),
                        enabled: saved
                            .get(&format!("{}:{}", skill.plugin, skill.name))
                            .copied()
                            .unwrap_or(true),
                        source_enabled,
                        locked: self.configured.is_disabled(&skill.plugin, &skill.name),
                        note: if active.iter().any(|winner| std::ptr::eq(*winner, skill)) {
                            "Selected for the next launch".to_string()
                        } else if let Some(winner) = active.iter().find(|winner| {
                            winner.path == skill.path
                                || winner.qualified_name() == skill.qualified_name()
                        }) {
                            format!(
                                "Covered by {} from {} ({})",
                                winner.name, winner.plugin, winner.path
                            )
                        } else {
                            "Disabled for the next launch".to_string()
                        },
                    }
                })
                .collect())
        };
        read().map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_sources_require_a_choice_and_configuration_denials_remain() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = keke_paths::AbsPath::new(tmp.path()).expect("absolute");
        let home = HomeLayout {
            home: root.clone(),
            workspace_root: root,
        };
        let configured = SkillSelection::new(vec!["claude:review".into()]).expect("valid");
        let plugin_root = tmp.path().join("foreign");
        std::fs::create_dir_all(plugin_root.join("skills/review")).expect("mkdir");
        std::fs::write(plugin_root.join("plugin.json"), r#"{"name":"claude"}"#).expect("manifest");
        std::fs::write(
            plugin_root.join("skills/review/SKILL.md"),
            "---\nname: review\ndescription: Review\n---\nBody",
        )
        .expect("skill");
        let plugin =
            keke_plugin::load(&plugin_root, keke_plugin::PluginScope::User, false).expect("plugin");
        let plugins = keke_plugin::PluginSet::compose(vec![plugin]).expect("compose");
        assert!(
            selection(&home, &configured, &plugins)
                .expect("selection")
                .is_disabled("claude", "other")
        );
        std::fs::create_dir_all(path(&home).parent().expect("parent")).expect("mkdir");
        std::fs::write(path(&home), r#"{"claude:*":true}"#).expect("write");
        let selected = selection(&home, &configured, &plugins).expect("selection");
        assert!(!selected.is_disabled("claude", "other"));
        assert!(selected.is_disabled("claude", "review"));
    }

    #[test]
    fn cached_foreign_plugins_require_explicit_enablement() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("plugin.json"),
            r#"{"name":"documents-skills"}"#,
        )
        .expect("manifest");
        let plugin =
            keke_plugin::load(tmp.path(), keke_plugin::PluginScope::User, false).expect("plugin");
        assert!(!default_enabled(&plugin));
        let root = keke_paths::AbsPath::new(tmp.path()).expect("absolute");
        let home = HomeLayout {
            home: root.clone(),
            workspace_root: root,
        };
        let plugins = keke_plugin::PluginSet::compose(vec![plugin]).expect("compose");
        let config = SkillSelection::default();
        assert!(
            selection(&home, &config, &plugins)
                .expect("selection")
                .is_disabled("documents-skills", "pdf")
        );
        std::fs::create_dir_all(path(&home).parent().expect("parent")).expect("mkdir");
        std::fs::write(path(&home), r#"{"documents-skills:*":true}"#).expect("write");
        assert!(
            !selection(&home, &config, &plugins)
                .expect("selection")
                .is_disabled("documents-skills", "pdf")
        );
    }
}
