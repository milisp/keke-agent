//! Typed path wrappers.
//!
//! Most path bugs in an agent harness are category errors: a relative path used
//! where the caller assumed a workspace-absolute one, or a non-UTF-8 path
//! reaching a JSON tool argument. Encoding absoluteness in the type removes the
//! first class of bug; requiring UTF-8 at construction removes the second.

use std::fmt;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

/// Rejection reasons shared by both wrappers.
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    #[error("path is not valid UTF-8: {0:?}")]
    NotUtf8(PathBuf),
    #[error("expected an absolute path, got {0}")]
    NotAbsolute(String),
    #[error("expected a relative path, got {0}")]
    NotRelative(String),
    #[error("{child} is not contained under {root}")]
    NotContained { root: String, child: String },
}

/// An absolute, UTF-8 filesystem path.
///
/// Absoluteness is checked once at construction, so downstream code may join
/// and compare without re-validating.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AbsPath(String);

impl AbsPath {
    /// Wrap `path`, rejecting relative or non-UTF-8 input.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, PathError> {
        let path = path.as_ref();
        let text = path
            .to_str()
            .ok_or_else(|| PathError::NotUtf8(path.to_path_buf()))?;
        if !path.is_absolute() {
            return Err(PathError::NotAbsolute(text.to_string()));
        }
        Ok(Self(text.to_string()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    /// Append a relative path, keeping the result absolute by construction.
    pub fn join(&self, rel: &RelPath) -> Result<Self, PathError> {
        Self::new(self.as_path().join(rel.as_path()))
    }

    /// Return `self` expressed relative to `root`, or an error when `self` lies
    /// outside it.
    ///
    /// Callers use this to enforce containment (a plugin resource must stay
    /// under its package root, a tool must stay under the workspace root), so
    /// escaping the root is an error rather than a silently clamped path.
    pub fn strip_root(&self, root: &AbsPath) -> Result<RelPath, PathError> {
        self.as_path()
            .strip_prefix(root.as_path())
            .map_err(|_| PathError::NotContained {
                root: root.0.clone(),
                child: self.0.clone(),
            })
            .and_then(RelPath::new)
    }

    /// Whether `self` is `root` or lies beneath it.
    #[must_use]
    pub fn is_contained_in(&self, root: &AbsPath) -> bool {
        self.as_path().starts_with(root.as_path())
    }
}

impl fmt::Display for AbsPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for AbsPath {
    type Error = PathError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<AbsPath> for String {
    fn from(value: AbsPath) -> Self {
        value.0
    }
}

/// A relative, UTF-8 filesystem path.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RelPath(String);

impl RelPath {
    /// Wrap `path`, rejecting absolute or non-UTF-8 input.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, PathError> {
        let path = path.as_ref();
        let text = path
            .to_str()
            .ok_or_else(|| PathError::NotUtf8(path.to_path_buf()))?;
        if path.is_absolute() {
            return Err(PathError::NotRelative(text.to_string()));
        }
        Ok(Self(text.to_string()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for RelPath {
    type Error = PathError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<RelPath> for String {
    fn from(value: RelPath) -> Self {
        value.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    const ROOT: &str = "/tmp/keke";
    #[cfg(windows)]
    const ROOT: &str = r"C:\tmp\keke";

    #[test]
    fn abs_path_rejects_relative() {
        assert!(matches!(
            AbsPath::new("src/lib.rs"),
            Err(PathError::NotAbsolute(_))
        ));
    }

    #[test]
    fn rel_path_rejects_absolute() {
        assert!(matches!(RelPath::new(ROOT), Err(PathError::NotRelative(_))));
    }

    #[test]
    fn join_then_strip_round_trips() {
        let root = AbsPath::new(ROOT).expect("absolute");
        let rel = RelPath::new("a/b.txt").expect("relative");
        let joined = root.join(&rel).expect("join");
        assert!(joined.is_contained_in(&root));
        assert_eq!(joined.strip_root(&root).expect("strip"), rel);
    }

    #[test]
    fn strip_root_rejects_escape() {
        let root = AbsPath::new(ROOT).expect("absolute");
        let outside = AbsPath::new(format!("{ROOT}-other")).expect("absolute");
        assert!(matches!(
            outside.strip_root(&root),
            Err(PathError::NotContained { .. })
        ));
    }
}
