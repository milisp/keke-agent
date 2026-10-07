//! Skill management keeps saved choices separate from the current model input.
use super::App;
use crate::skills::SkillRow;

impl App {
    #[must_use]
    pub fn with_skills(mut self, skills: crate::Skills) -> Self {
        let other = skills
            .entries
            .iter()
            .filter(|skill| !skill.source_enabled)
            .count();
        if other > 0 {
            self.transcript.push(crate::Cell::Notice(format!(
                "Discovered {other} skills in disabled sources — /skills lets you enable them."
            )));
        }
        self.skills = skills;
        self
    }

    pub fn open_skills_picker(&mut self) {
        if let Some(manage) = &self.skills.manage {
            match manage.refresh() {
                Ok(entries) => self.skills.entries = entries,
                Err(error) => self.skills_message = Some(error),
            }
        }
        self.skills_detail = None;
        self.skills_expanded.clear();
        self.picker = Some(crate::picker::Picker::new(
            crate::picker::PickerKind::Skills,
        ));
    }

    #[must_use]
    pub fn skills_picker(&self) -> Option<&crate::picker::Picker> {
        self.picker
            .as_ref()
            .filter(|picker| picker.kind() == crate::picker::PickerKind::Skills)
    }

    #[must_use]
    pub fn picker_skills(&self) -> Vec<SkillRow> {
        let Some(picker) = self.skills_picker() else {
            return Vec::new();
        };
        let query = picker.query().trim().to_lowercase();
        let mut sources = Vec::new();
        for entry in &self.skills.entries {
            if !sources.contains(&entry.source) {
                sources.push(entry.source.clone());
            }
        }
        let mut rows = Vec::new();
        for source in sources {
            let entries: Vec<_> = self
                .skills
                .entries
                .iter()
                .filter(|entry| entry.source == source)
                .collect();
            let matching: Vec<_> = entries
                .iter()
                .filter(|entry| {
                    format!(
                        "{} {} {} {} {}",
                        entry.name,
                        entry.source,
                        entry.note,
                        entry.description,
                        entry.path.display()
                    )
                    .to_lowercase()
                    .contains(&query)
                })
                .collect();
            if matching.is_empty() {
                continue;
            }
            let expanded = !query.is_empty() || self.skills_expanded.contains(&source);
            rows.push(SkillRow::Source {
                source,
                enabled: entries[0].source_enabled,
                count: entries.len(),
                expanded,
            });
            if expanded {
                rows.extend(
                    matching
                        .into_iter()
                        .map(|entry| SkillRow::Skill((*entry).clone())),
                );
            }
        }
        rows
    }

    pub(crate) fn toggle_selected_skill(&mut self) {
        let Some(row) = self.picker_skills().get(self.picker_selected()).cloned() else {
            return;
        };
        let (source, name, disabled) = match row {
            SkillRow::Source {
                source, enabled, ..
            } => (source, None, enabled),
            SkillRow::Skill(entry) => {
                if entry.locked {
                    self.skills_message = Some("Disabled by configuration.".to_string());
                    return;
                }
                (entry.source, Some(entry.name), entry.enabled)
            }
        };
        let Some(manage) = &self.skills.manage else {
            self.skills_message =
                Some("Skill settings cannot be saved by this interface.".to_string());
            return;
        };
        match manage.set_disabled(&source, name.as_deref(), disabled) {
            Ok(()) => {
                for entry in &mut self.skills.entries {
                    if entry.source == source {
                        if let Some(name) = &name {
                            if entry.name == *name {
                                entry.enabled = !disabled;
                            }
                        } else {
                            entry.source_enabled = !disabled;
                        }
                    }
                }
                if let Ok(entries) = manage.refresh() {
                    self.skills.entries = entries;
                }
                self.skills_message = Some("Saved — changes apply on the next launch.".to_string());
            }
            Err(error) => self.skills_message = Some(error),
        }
    }

    pub(crate) fn details_selected_skill(&mut self) {
        self.skills_message = None;
        if let Some(SkillRow::Source { source, .. }) =
            self.picker_skills().get(self.picker_selected())
        {
            let source = source.clone();
            if !self.skills_expanded.remove(&source) {
                self.skills_expanded.insert(source);
            }
            self.skills_detail = None;
            return;
        }
        self.skills_detail = self.picker_skills().get(self.picker_selected()).map(|row| match row {
            SkillRow::Source { source, count, .. } => format!("{source}: {count} discovered skills. Space toggles the source; individual choices are preserved."),
            SkillRow::Skill(entry) => format!("{} · {} · {} · {}{}", entry.path.display(), entry.description, entry.source, entry.note, if entry.locked { " · disabled by configuration" } else { "" }),
        });
    }

    #[must_use]
    pub fn skills_footer(&self) -> String {
        self.skills_message.clone().or_else(|| self.skills_detail.clone()).unwrap_or_else(|| {
            if self.skills.entries.is_empty() {
                "Add ~/.agents/skills/<name>/SKILL.md; third-party skills can be installed there too.".to_string()
            } else {
                "Enter expands a source or shows details · Space toggles · type to search · changes apply on next launch".to_string()
            }
        })
    }
}
