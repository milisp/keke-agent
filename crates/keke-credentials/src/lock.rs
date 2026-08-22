//! A per-vendor mutation lock.
//!
//! Two keke processes can be refreshing the same credential at the same
//! moment — a TUI and an editor over ACP both hitting a 401. Without a lock
//! their read-modify-write cycles interleave and the loser's refresh token is
//! silently dropped, which surfaces much later as a login that cannot be
//! renewed. The lock is a sibling file rather than a lock on the auth file
//! itself so a stale holder can be broken without risking the credential.

use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;

use crate::error::AuthFileError;

/// How long a caller waits before deciding the holder is not coming back.
const ACQUIRE_DEADLINE: Duration = Duration::from_secs(2);
/// How often the wait re-checks.
const POLL: Duration = Duration::from_millis(10);
/// A lock file older than this belongs to a process that died holding it: a
/// mutation is a token request and a rename, never minutes of work.
const STALE_AFTER: Duration = Duration::from_secs(60);

/// Held for the duration of one read-modify-write.
///
/// Dropping releases it. A panic while holding therefore releases too, which is
/// the reason the guard exists rather than a pair of functions.
#[derive(Debug)]
pub(crate) struct MutationLock {
    path: PathBuf,
}

impl MutationLock {
    /// Take the lock at `path`, waiting for a live holder and breaking a dead
    /// one.
    pub(crate) fn acquire(path: PathBuf) -> Result<Self, AuthFileError> {
        Self::acquire_within(path, ACQUIRE_DEADLINE)
    }

    pub(crate) fn acquire_within(path: PathBuf, deadline: Duration) -> Result<Self, AuthFileError> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|err| AuthFileError::io(dir, &err))?;
        }

        let started = SystemTime::now();
        loop {
            match claim(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(AuthFileError::io(&path, &err)),
            }

            if is_stale(&path) {
                // Best effort: losing the race to remove it just means another
                // waiter broke the lock first, and the next claim will contend
                // with them rather than with the dead holder.
                let _ = fs::remove_file(&path);
                continue;
            }

            let waited = started.elapsed().unwrap_or_default();
            if waited >= deadline {
                return Err(AuthFileError::Locked {
                    path: path.display().to_string(),
                    millis: deadline.as_millis() as u64,
                });
            }
            std::thread::sleep(POLL);
        }
    }
}

impl Drop for MutationLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Create the lock file, failing if somebody already holds it.
fn claim(path: &Path) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path).map(drop)
}

fn is_stale(path: &Path) -> bool {
    let Ok(modified) = fs::metadata(path).and_then(|meta| meta.modified()) else {
        return false;
    };
    modified
        .elapsed()
        .is_ok_and(|elapsed| elapsed > STALE_AFTER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_holder_waits_and_then_reports_who_has_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.codex.lock");
        let _held = MutationLock::acquire(path.clone()).expect("first");

        let err = MutationLock::acquire_within(path.clone(), Duration::from_millis(30))
            .expect_err("second must not get in");
        assert!(
            matches!(&err, AuthFileError::Locked { path: p, .. } if p.contains("auth.codex.lock")),
            "got {err}"
        );
    }

    #[test]
    fn releasing_lets_the_next_caller_in() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.codex.lock");
        drop(MutationLock::acquire(path.clone()).expect("first"));
        MutationLock::acquire(path).expect("second");
    }

    #[test]
    fn a_lock_left_by_a_dead_process_is_broken_rather_than_waited_out() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.codex.lock");
        claim(&path).expect("claim");
        let ancient = SystemTime::now() - (STALE_AFTER + Duration::from_secs(60));
        fs::File::open(&path)
            .expect("open")
            .set_modified(ancient)
            .expect("backdate");

        MutationLock::acquire_within(path, Duration::from_millis(30))
            .expect("a stale lock must not block a login forever");
    }
}
