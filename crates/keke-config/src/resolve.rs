//! Locating the harness home and the workspace root.

use std::path::Path;

use keke_paths::AbsPath;

use crate::ConfigError;

/// Where the harness keeps its own state: `$KEKE_HOME`, else `~/.keke`.
///
/// This does not create the directory. Resolution and creation are separate so
/// a read-only command can ask where state lives without producing it.
pub fn keke_home() -> Result<AbsPath, ConfigError> {
    if let Some(explicit) = std::env::var_os("KEKE_HOME") {
        return Ok(AbsPath::new(explicit)?);
    }
    let home = dirs::home_dir()
        .ok_or_else(|| ConfigError::Unresolvable("the home directory".to_string()))?;
    Ok(AbsPath::new(home.join(".keke"))?)
}

/// Find the project root containing `start`.
///
/// Walks upward for a `.git` directory, falling back to `start` itself. The
/// fallback matters: keke must work in a directory that is not a repository,
/// and refusing to start there would be a worse failure than a narrower root.
pub fn resolve_workspace_root(start: &Path) -> Result<AbsPath, ConfigError> {
    let start = std::fs::canonicalize(start).map_err(|source| ConfigError::Read {
        path: start.display().to_string(),
        source,
    })?;

    let mut cursor = start.as_path();
    loop {
        if cursor.join(".git").exists() {
            return Ok(AbsPath::new(cursor)?);
        }
        match cursor.parent() {
            Some(parent) => cursor = parent,
            None => break,
        }
    }
    Ok(AbsPath::new(&start)?)
}
