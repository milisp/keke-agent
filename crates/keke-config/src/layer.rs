//! Configuration layers and their discovery.

use std::path::Path;
use std::path::PathBuf;

use keke_paths::AbsPath;

use crate::ConfigError;
use crate::ConfigFile;

/// Where a layer came from.
///
/// Retained on the merged [`Config`](crate::Config) so a surface can answer
/// "which file set this?" without re-reading disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayerSource {
    /// Deployment policy, applied first and overridable by the layers above.
    Managed(PathBuf),
    /// `$KEKE_HOME/config.toml`.
    User(PathBuf),
    /// `.keke/config.toml` in the workspace.
    Project(PathBuf),
    /// Supplied programmatically; used by tests and `--config` overrides.
    Inline(String),
}

impl LayerSource {
    /// A short label naming this layer in an error message.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Managed(path) | Self::User(path) | Self::Project(path) => {
                path.display().to_string()
            }
            Self::Inline(label) => label.clone(),
        }
    }
}

/// One parsed layer.
#[derive(Clone, Debug)]
pub struct ConfigLayer {
    pub source: LayerSource,
    pub file: ConfigFile,
}

impl ConfigLayer {
    /// Parse `text` as a layer.
    pub fn parse(source: LayerSource, text: &str) -> Result<Self, ConfigError> {
        let file = toml::from_str(text).map_err(|error| ConfigError::Parse {
            path: source.describe(),
            message: error.to_string(),
        })?;
        Ok(Self { source, file })
    }

    /// Read every layer that exists, in application order.
    ///
    /// A layer that is absent is skipped silently — that is the normal case. A
    /// layer that exists but does not parse is an error, because silently
    /// ignoring a malformed config is how a setting appears not to take effect
    /// for reasons nobody can find.
    pub fn discover(home: &AbsPath, workspace_root: &AbsPath) -> Result<Vec<Self>, ConfigError> {
        let candidates = [
            (
                home.as_path().join("managed-config.toml"),
                LayerSource::Managed as fn(PathBuf) -> LayerSource,
            ),
            (home.as_path().join("config.toml"), LayerSource::User),
            (
                workspace_root.as_path().join(".keke").join("config.toml"),
                LayerSource::Project,
            ),
        ];

        let mut layers = Vec::new();
        for (path, make_source) in candidates {
            if let Some(layer) = Self::read(&path, make_source)? {
                layers.push(layer);
            }
        }
        Ok(layers)
    }

    fn read(
        path: &Path,
        make_source: fn(PathBuf) -> LayerSource,
    ) -> Result<Option<Self>, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(make_source(path.to_path_buf()), &text).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(ConfigError::Read {
                path: path.display().to_string(),
                source,
            }),
        }
    }
}
