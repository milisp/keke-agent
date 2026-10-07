//! Skill switches persist choices without changing the running inventory.
use crate::tests::helpers::*;
use crossterm::event::KeyCode;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Manager(Mutex<Vec<(String, Option<String>, bool)>>);
impl crate::SkillsManage for Manager {
    fn set_disabled(&self, source: &str, name: Option<&str>, disabled: bool) -> Result<(), String> {
        self.0
            .lock()
            .unwrap()
            .push((source.into(), name.map(str::to_owned), disabled));
        Ok(())
    }
    fn refresh(&self) -> Result<Vec<crate::SkillStatus>, String> {
        let mut entries = entries();
        for (source, name, disabled) in self.0.lock().unwrap().iter() {
            for entry in &mut entries {
                if entry.source == *source {
                    match name {
                        Some(name) if entry.name == *name => entry.enabled = !disabled,
                        None => entry.source_enabled = !disabled,
                        _ => {}
                    }
                }
            }
        }
        Ok(entries)
    }
}
fn entries() -> Vec<crate::SkillStatus> {
    vec![crate::SkillStatus {
        name: "review".into(),
        source: "agents".into(),
        description: "Review code".into(),
        note: String::new(),
        path: "/skills/review/SKILL.md".into(),
        enabled: true,
        source_enabled: true,
        locked: false,
    }]
}
#[tokio::test]
async fn source_toggle_preserves_individual_choice_and_persists() {
    let (app, ..) = app_with_commands(Vec::new(), Vec::new());
    let manager = Arc::new(Manager::default());
    let mut app = app.with_skills(crate::Skills {
        entries: entries(),
        manage: Some(manager.clone()),
    });
    type_text(&mut app, "/skills");
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.picker_skills().len(), 1);
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.picker_skills().len(), 2);
    app.handle_key(key(KeyCode::Char(' ')));
    let crate::skills::SkillRow::Skill(entry) = &app.picker_skills()[1] else {
        panic!()
    };
    assert!(entry.enabled);
    assert!(!entry.source_enabled);
    assert_eq!(*manager.0.lock().unwrap(), [("agents".into(), None, true)]);
    assert!(app.skills_footer().contains("next launch"));
    app.handle_key(key(KeyCode::Char(' ')));
    let crate::skills::SkillRow::Skill(entry) = &app.picker_skills()[1] else {
        panic!()
    };
    assert!(entry.enabled && entry.source_enabled);
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));
    assert!(app.skills_footer().contains("/skills/review/SKILL.md"));
}
#[tokio::test]
async fn search_details_and_escape_work_without_consuming_search_letters() {
    let (app, ..) = app_with_commands(Vec::new(), Vec::new());
    let mut app = app.with_skills(crate::Skills {
        entries: entries(),
        manage: None,
    });
    app.open_skills_picker();
    type_text(&mut app, "review");
    assert_eq!(app.picker_skills().len(), 2);
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));
    assert!(app.skills_footer().contains("/skills/review/SKILL.md"));
    app.handle_key(key(KeyCode::Esc));
    assert!(!app.picker_open());
}
#[tokio::test]
async fn empty_inventory_explains_where_to_install_skills() {
    let (mut app, ..) = app_with_commands(Vec::new(), Vec::new());
    app.open_skills_picker();
    assert!(app.skills_picker().is_some());
    assert!(app.skills_footer().contains("~/.agents/skills"));
}

#[tokio::test]
async fn a_disabled_source_has_no_enabled_marks_or_duplicate_star() {
    let (app, ..) = app_with_commands(Vec::new(), Vec::new());
    let mut inventory = entries();
    inventory[0].source_enabled = false;
    let mut app = app.with_skills(crate::Skills {
        entries: inventory,
        manage: None,
    });
    app.open_skills_picker();
    app.handle_key(key(KeyCode::Enter));
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20)).unwrap();
    terminal
        .draw(|frame| crate::draw::picker::draw(frame, frame.area(), &app))
        .unwrap();
    let screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(screen.contains("[ ] agents"));
    assert!(screen.contains("[ ] review"));
    assert!(!screen.contains("[x]"));
    assert!(!screen.contains('*'));
}
