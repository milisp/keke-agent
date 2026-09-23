//! The model each provider route was last used with.
//!
//! `model` in config.toml names one model for one route. Moving to another
//! route has to drop it — an id from one vendor need not exist on the next —
//! so without a record per route, coming back to a route meant starting from
//! whatever headed its catalog rather than the model the person had been
//! using there. This is that record.
//!
//! Kept beside config.toml rather than in it: it is what keke observed, not
//! what a person declared, and a file a person edits by hand should not be
//! rewritten every time they switch models. Like the model catalog cache it is
//! a convenience — a file that cannot be read is empty, not an error.

use std::collections::BTreeMap;
use std::path::PathBuf;

use keke_paths::AbsPath;

use crate::ConfigError;

fn path(home: &AbsPath) -> PathBuf {
    home.as_path().join("state").join("models.toml")
}

fn load(home: &AbsPath) -> BTreeMap<String, String> {
    std::fs::read_to_string(path(home))
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

/// The model `route` was last used with, if it ever was on this machine.
#[must_use]
pub fn remembered_model(home: &AbsPath, route: &str) -> Option<String> {
    load(home)
        .remove(route)
        .filter(|model| !model.trim().is_empty())
}

/// Record that `route` is now being used with `model`.
///
/// An empty model is not recorded: "nothing chosen" is not a model, and
/// storing it would read back as one.
pub fn remember_model(home: &AbsPath, route: &str, model: &str) -> Result<(), ConfigError> {
    if model.trim().is_empty() {
        return Ok(());
    }
    let mut models = load(home);
    if models.get(route).map(String::as_str) == Some(model) {
        return Ok(());
    }
    models.insert(route.to_string(), model.to_string());
    let path = path(home);
    let text = toml::to_string(&models).map_err(|error| ConfigError::Invalid {
        path: path.display().to_string(),
        message: format!("rendering remembered models: {error}"),
    })?;
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Written beside and renamed, so an interrupted write leaves the
        // previous record rather than a truncated one that reads as empty.
        let temporary = path.with_extension("toml.tmp");
        std::fs::write(&temporary, text)?;
        std::fs::rename(&temporary, &path)
    };
    write().map_err(|source| ConfigError::Read {
        path: path.display().to_string(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> (tempfile::TempDir, AbsPath) {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = AbsPath::new(dir.path()).expect("absolute");
        (dir, home)
    }

    #[test]
    fn each_route_keeps_its_own_model() {
        let (_dir, home) = home();
        remember_model(&home, "codex", "gpt-5.6-luna").expect("stored");
        remember_model(&home, "nvidia", "nvidia/nemotron-3-ultra").expect("stored");
        assert_eq!(
            remembered_model(&home, "codex").as_deref(),
            Some("gpt-5.6-luna")
        );
        assert_eq!(
            remembered_model(&home, "nvidia").as_deref(),
            Some("nvidia/nemotron-3-ultra")
        );
        assert_eq!(remembered_model(&home, "grok"), None);
    }

    #[test]
    fn the_latest_model_on_a_route_replaces_the_one_before() {
        let (_dir, home) = home();
        remember_model(&home, "codex", "gpt-6-astra").expect("stored");
        remember_model(&home, "codex", "gpt-5.6-luna").expect("stored");
        assert_eq!(
            remembered_model(&home, "codex").as_deref(),
            Some("gpt-5.6-luna")
        );
    }

    #[test]
    fn an_empty_model_is_never_remembered() {
        let (_dir, home) = home();
        remember_model(&home, "codex", "gpt-5.6-luna").expect("stored");
        remember_model(&home, "codex", "  ").expect("ignored");
        assert_eq!(
            remembered_model(&home, "codex").as_deref(),
            Some("gpt-5.6-luna")
        );
    }

    #[test]
    fn an_unreadable_record_is_empty_rather_than_an_error() {
        let (_dir, home) = home();
        std::fs::create_dir_all(home.as_path().join("state")).expect("dir");
        std::fs::write(path(&home), "{not toml").expect("write");
        assert_eq!(remembered_model(&home, "codex"), None);
        remember_model(&home, "codex", "gpt-5.6-luna").expect("rewritten");
        assert_eq!(
            remembered_model(&home, "codex").as_deref(),
            Some("gpt-5.6-luna")
        );
    }
}
