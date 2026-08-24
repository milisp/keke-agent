//! What a person typed, per project, so the arrow keys can bring it back.
//!
//! This is not the rollout log. A rollout is one session's model-visible
//! history; this is the typing history of a *directory*, which is what somebody
//! reaching for the up arrow is actually asking for — the prompt they wrote
//! yesterday, in this repository, in a session they have since closed.
//!
//! One JSONL file per project directory, appended to and never rewritten, so
//! two keke processes in the same directory interleave lines instead of
//! clobbering each other's file.

use std::path::Path;
use std::path::PathBuf;

use keke_paths::AbsPath;
use keke_protocol::SessionId;
use serde::Deserialize;
use serde::Serialize;

use crate::RolloutError;
use crate::resume::project_dir;

/// How many past prompts a load hands back, newest last.
///
/// The file keeps growing; what the interface holds does not. A person scrolls
/// back through tens of prompts, not thousands, and the cap is what keeps
/// startup from reading a year of typing into memory.
const KEPT: usize = 1000;

/// One line of the file.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PromptHistoryEntry {
    /// RFC 3339, in UTC.
    pub timestamp: String,
    /// The session it was typed in, when the surface knows its id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub prompt: String,
}

/// Where a project's typing history lives.
///
/// Next to that project's session logs, in the directory named after the
/// project path, so everything belonging to one directory is in one place.
#[must_use]
pub fn prompt_history_path(home: &AbsPath, cwd: &Path) -> PathBuf {
    project_dir(home, cwd).join("prompt_history.jsonl")
}

/// The prompt history of one project directory.
#[derive(Clone, Debug)]
pub struct PromptHistory {
    path: PathBuf,
    session: Option<SessionId>,
}

impl PromptHistory {
    /// The history for `cwd` under `home`. Nothing is read or written yet.
    #[must_use]
    pub fn new(home: &AbsPath, cwd: &Path) -> Self {
        Self {
            path: prompt_history_path(home, cwd),
            session: None,
        }
    }

    /// Tag what gets recorded from here on with the session it was typed in.
    #[must_use]
    pub fn in_session(mut self, session: SessionId) -> Self {
        self.session = Some(session);
        self
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The past prompts, oldest first, at most [`KEPT`] of them.
    ///
    /// A missing file is an empty history rather than an error — the first run
    /// in a directory is the ordinary case. A line that fails to parse is
    /// skipped for the same reason the rollout reader skips one: one bad line
    /// must not cost a person the rest of their history.
    pub fn load(&self) -> Result<Vec<String>, RolloutError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(RolloutError::Io {
                    path: self.path.display().to_string(),
                    source,
                });
            }
        };

        let mut prompts: Vec<String> = Vec::new();
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let Ok(entry) = serde_json::from_str::<PromptHistoryEntry>(line) else {
                tracing::warn!(path = %self.path.display(), "skipping an unreadable history line");
                continue;
            };
            if entry.prompt.trim().is_empty() {
                continue;
            }
            // Repeating the same prompt is a person retrying, not two things
            // worth arrowing through separately.
            if prompts.last() == Some(&entry.prompt) {
                continue;
            }
            prompts.push(entry.prompt);
        }
        if prompts.len() > KEPT {
            prompts.drain(..prompts.len() - KEPT);
        }
        Ok(prompts)
    }

    /// Append one prompt. Blank input is not history.
    pub fn record(&self, prompt: &str) -> Result<(), RolloutError> {
        if prompt.trim().is_empty() {
            return Ok(());
        }
        let entry = PromptHistoryEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            session_id: self.session,
            prompt: prompt.to_string(),
        };
        let mut line = serde_json::to_string(&entry)?;
        line.push('\n');

        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| RolloutError::Io {
                path: dir.display().to_string(),
                source,
            })?;
        }
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|source| RolloutError::Io {
                path: self.path.display().to_string(),
                source,
            })?;
        file.write_all(line.as_bytes())
            .map_err(|source| RolloutError::Io {
                path: self.path.display().to_string(),
                source,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> (tempfile::TempDir, AbsPath) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonicalize");
        let home = AbsPath::new(root).expect("absolute");
        (dir, home)
    }

    /// The name says which project it is, and holds no separators of its own.
    #[test]
    fn the_file_is_named_after_the_project_directory() {
        let (_dir, home) = home();
        let path = prompt_history_path(&home, Path::new("/Users/x/projects/keke"));
        assert!(path.ends_with("%2FUsers%2Fx%2Fprojects%2Fkeke/prompt_history.jsonl"));
        assert!(path.starts_with(crate::sessions_dir(&home)));
    }

    /// The first run in a directory has no history, which is not a failure.
    #[test]
    fn a_missing_file_loads_as_an_empty_history() {
        let (_dir, home) = home();
        let history = PromptHistory::new(&home, Path::new("/nowhere"));
        assert!(history.load().expect("loads").is_empty());
    }

    #[test]
    fn what_was_recorded_comes_back_oldest_first() {
        let (_dir, home) = home();
        let history = PromptHistory::new(&home, Path::new("/p")).in_session(SessionId::new());
        history.record("first").expect("records");
        history.record("second").expect("records");
        assert_eq!(history.load().expect("loads"), vec!["first", "second"]);
    }

    /// Retrying the same prompt twice is one thing to arrow back through.
    #[test]
    fn a_repeated_prompt_is_not_two_entries() {
        let (_dir, home) = home();
        let history = PromptHistory::new(&home, Path::new("/p"));
        history.record("again").expect("records");
        history.record("again").expect("records");
        assert_eq!(history.load().expect("loads"), vec!["again"]);
    }

    #[test]
    fn blank_input_is_not_history() {
        let (_dir, home) = home();
        let history = PromptHistory::new(&home, Path::new("/p"));
        history.record("   \n ").expect("records");
        assert!(history.load().expect("loads").is_empty());
    }

    /// One unreadable line must not cost a person the rest of the file.
    #[test]
    fn an_unreadable_line_is_skipped() {
        let (_dir, home) = home();
        let history = PromptHistory::new(&home, Path::new("/p"));
        history.record("kept").expect("records");
        let mut text = std::fs::read_to_string(history.path()).expect("reads");
        text.push_str("{not json\n");
        std::fs::write(history.path(), text).expect("writes");
        assert_eq!(history.load().expect("loads"), vec!["kept"]);
    }
}
