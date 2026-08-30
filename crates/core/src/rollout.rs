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
    /// The derived summary, folded as events are written rather than by
    /// re-reading what was just appended.
    meta: crate::meta::SessionMeta,
}

impl RolloutRecorder {
    /// Create the log for `session` under `home`, in `cwd`'s project directory.
    ///
    /// A session owns a directory rather than a file, so the log has somewhere
    /// to keep what is derived from it.
    pub async fn create(
        home: &AbsPath,
        cwd: &Path,
        session: SessionId,
    ) -> Result<Self, RolloutError> {
        let dir = crate::project_dir(home, cwd).join(session.to_string());
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|source| RolloutError::Io {
                path: dir.display().to_string(),
                source,
            })?;

        let path = dir.join(crate::meta::LOG_FILE);
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|source| RolloutError::Io {
                path: path.display().to_string(),
                source,
            })?;

        // A resumed session appends to a log it did not write, so the summary
        // it continues from is whatever that log already says.
        let meta = {
            let path = path.clone();
            tokio::task::spawn_blocking(move || crate::meta::SessionMeta::refreshed(&path))
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_else(crate::meta::SessionMeta::new)
        };

        Ok(Self { path, file, meta })
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
        })?;

        self.meta.append(line.len() as u64, &envelope);
        // Written at the points a listing would show something different, not
        // on every event: the cache exists to be read cheaply, and a session
        // that dies between two of these is caught up by one fold of the tail
        // rather than by a rescan.
        if matches!(
            envelope.event,
            SessionEvent::SessionStart { .. } | SessionEvent::TurnEnd { .. }
        ) {
            self.meta.write(&self.path);
        }
        Ok(())
    }
}

impl Drop for RolloutRecorder {
    /// A session that ends between two turns still leaves a current cache.
    fn drop(&mut self) {
        self.meta.write(&self.path);
    }
}

/// Read a log back, skipping lines that do not parse.
///
/// A malformed or unknown line is skipped with a warning rather than aborting
/// the load: refusing to open a session because one line is from a newer
/// version would be a worse failure than resuming without it.
pub fn read_log(path: &Path) -> Result<Vec<SessionEventEnvelope>, RolloutError> {
    read_log_from(path, 0)
}

/// Read a log back from byte `from`, which must be where a line begins.
///
/// The caller that has a cached offset for the last model request uses this to
/// read the tail a resume needs instead of the whole log.
pub fn read_log_from(path: &Path, from: u64) -> Result<Vec<SessionEventEnvelope>, RolloutError> {
    let io = |source: std::io::Error| RolloutError::Io {
        path: path.display().to_string(),
        source,
    };
    let mut file = std::fs::File::open(path).map_err(io)?;
    if from > 0 {
        use std::io::Seek;
        file.seek(std::io::SeekFrom::Start(from)).map_err(io)?;
    }
    let mut text = String::new();
    {
        use std::io::Read;
        file.read_to_string(&mut text).map_err(io)?;
    }

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
