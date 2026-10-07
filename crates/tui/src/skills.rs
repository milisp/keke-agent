//! Skill inventory and persistence capabilities supplied by the host.
use std::path::PathBuf;
use std::sync::Arc;

/// A discovered skill, including choices saved for the next launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillStatus {
    pub name: String,
    pub source: String,
    pub description: String,
    /// Discovery or precedence information supplied by the host.
    pub note: String,
    pub path: PathBuf,
    pub enabled: bool,
    pub source_enabled: bool,
    /// The host cannot change this contribution independently.
    pub locked: bool,
}

/// Persists source and skill choices without changing the running session.
/// Implementers own discovery and storage; the interface only names a choice.
pub trait SkillsManage: Send + Sync + 'static {
    fn set_disabled(&self, source: &str, name: Option<&str>, disabled: bool) -> Result<(), String>;
    fn refresh(&self) -> Result<Vec<SkillStatus>, String>;
}

/// The inventory and optional capability for managing it.
#[derive(Default)]
pub struct Skills {
    pub entries: Vec<SkillStatus>,
    pub manage: Option<Arc<dyn SkillsManage>>,
}

/// A selectable source group or one skill inside it.
#[derive(Clone, Debug)]
pub enum SkillRow {
    Source {
        source: String,
        enabled: bool,
        count: usize,
        expanded: bool,
    },
    Skill(SkillStatus),
}
