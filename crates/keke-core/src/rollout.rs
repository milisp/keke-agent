//! The append-only session log.
//!
//! One JSONL file per session, one [`SessionEventEnvelope`] per line. The
//! invariant it serves is *model-visible implies logged*: if a session cannot be
//! replayed from this file, something reached the model that was never
//! recorded, and the replay will diverge from the live run.
//!
//! Reading is line-by-line and best-effort: a line that fails to parse is
//! skipped with a warning rather than failing the whole load, so a log written
//! by a newer keke with an unknown event variant still resumes.

use std::path::Path;
use std::path::PathBuf;

use keke_paths::AbsPath;
use keke_protocol::SessionEvent;
use keke_protocol::SessionEventEnvelope;
use keke_protocol::SessionId;
use tokio::io::AsyncWriteExt;

/// Why the log could not be written or read.
#[derive(Debug, thiserror::Error)]
pub enum RolloutError {
    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("could not serialize a session event: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// Appends events to a session's log.
pub struct RolloutRecorder {
    path: PathBuf,
    file: tokio::fs::File,
}

impl RolloutRecorder {
    /// Create the log for `session` under `home`.
    pub async fn create(home: &AbsPath, session: SessionId) -> Result<Self, RolloutError> {
        let dir = home.as_path().join("sessions");
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|source| RolloutError::Io {
                path: dir.display().to_string(),
                source,
            })?;

        let path = dir.join(format!("{session}.jsonl"));
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|source| RolloutError::Io {
                path: path.display().to_string(),
                source,
            })?;

        Ok(Self { path, file })
    }

    /// Where this log lives.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one event, flushing before returning.
    ///
    /// Flushing per event costs a syscall and buys the property that matters
    /// more: a session killed mid-turn still has everything up to the kill.
    pub async fn append(&mut self, event: SessionEvent) -> Result<(), RolloutError> {
        let envelope = SessionEventEnvelope {
            at: chrono::Utc::now().to_rfc3339(),
            event,
        };
        let mut line = serde_json::to_string(&envelope)?;
        line.push('\n');

        self.file
            .write_all(line.as_bytes())
            .await
            .map_err(|source| RolloutError::Io {
                path: self.path.display().to_string(),
                source,
            })?;
        self.file.flush().await.map_err(|source| RolloutError::Io {
            path: self.path.display().to_string(),
            source,
        })
    }
}

/// Read a log back, skipping lines that do not parse.
///
/// A malformed or unknown line is skipped with a warning rather than aborting
/// the load: refusing to open a session because one line is from a newer
/// version would be a worse failure than resuming without it.
pub fn read_log(path: &Path) -> Result<Vec<SessionEventEnvelope>, RolloutError> {
    let text = std::fs::read_to_string(path).map_err(|source| RolloutError::Io {
        path: path.display().to_string(),
        source,
    })?;

    Ok(text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| match serde_json::from_str(line) {
            Ok(envelope) => Some(envelope),
            Err(error) => {
                tracing::warn!(%error, "skipping unreadable session log line");
                None
            }
        })
        .collect())
}
