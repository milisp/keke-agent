//! Write a private file so a crash cannot be observed as a half file.
//!
//! Staging and committing are separate values rather than one function because
//! the property that matters — an interrupted write leaves the previous
//! contents intact — is only testable if a test can stage and then *not*
//! commit.

use std::fs;
use std::io;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;

/// Bytes written to a temporary file in the destination's directory, not yet
/// visible under the destination name.
///
/// Dropping without [`Staged::commit`] removes the temporary file, which is
/// what makes a failed write a no-op rather than a stray file next to the real
/// one.
pub(crate) struct Staged {
    temp: PathBuf,
    committed: bool,
}

impl Staged {
    /// Write `body` beside `dest`, `0600`, without touching `dest`.
    pub(crate) fn stage(dest: &Path, body: &[u8]) -> io::Result<Self> {
        let dir = dest.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "destination has no directory")
        })?;
        fs::create_dir_all(dir)?;
        create_private_dir(dir)?;

        let name = dest
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file");
        // The pid keeps two processes staging the same destination from
        // clobbering each other's temporary file before either renames.
        let temp = dir.join(format!(".{name}.{}.tmp", std::process::id()));

        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(body)?;
        file.sync_all()?;

        Ok(Self {
            temp,
            committed: false,
        })
    }

    /// Move the staged bytes onto `dest` in one step.
    pub(crate) fn commit(mut self, dest: &Path) -> io::Result<()> {
        fs::rename(&self.temp, dest)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.temp);
        }
    }
}

/// Stage and commit in one call.
pub(crate) fn write_private_atomic(dest: &Path, body: &[u8]) -> io::Result<()> {
    Staged::stage(dest, body)?.commit(dest)
}

/// Tighten the containing directory to `0700`.
///
/// A `0600` file under a `0777` directory is still removable and replaceable by
/// anyone with an account on the box, so the file mode alone does not carry the
/// guarantee the caller thinks it bought.
fn create_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(dir)?.permissions().mode();
        if mode & 0o077 != 0 {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_uncommitted_write_leaves_the_destination_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("auth.codex.json");
        write_private_atomic(&dest, b"first").expect("write");

        drop(Staged::stage(&dest, b"second").expect("stage"));

        assert_eq!(fs::read(&dest).expect("read"), b"first");
        let strays: Vec<_> = fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| name != "auth.codex.json")
            .collect();
        assert!(strays.is_empty(), "left behind {strays:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_staged_file_is_private_before_it_is_committed() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("auth.codex.json");
        let staged = Staged::stage(&dest, b"secret").expect("stage");
        let mode = fs::metadata(&staged.temp)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "mode was {mode:o}");
    }
}
