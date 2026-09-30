//! The on-disk side: one markdown file per entry, written atomically.

use std::io::Write;
use std::path::PathBuf;

use keke_paths::AbsPath;

/// Longest entry name. Bounded so a name is always a sane file name.
const MAX_NAME_LEN: usize = 64;
/// How much of an entry's first line a listing shows.
const FIRST_LINE_CHARS: usize = 100;

#[derive(Debug, thiserror::Error)]
pub(crate) enum StoreError {
    #[error(
        "`{0}` is not a valid memory name: use lowercase letters, digits, `-` and `_`, starting \
         with a letter or digit, at most 64 characters"
    )]
    BadName(String),
    #[error("no memory named `{name}`; {}", existing(.available))]
    Missing {
        name: String,
        available: Vec<String>,
    },
    #[error(
        "`{name}` would be {size} bytes, over the {limit}-byte limit for one entry; keep entries \
         short — split it or drop what is stale"
    )]
    TooLarge {
        name: String,
        size: usize,
        limit: usize,
    },
    #[error("memory storage failed: {0}")]
    Io(#[from] std::io::Error),
}

fn existing(names: &[String]) -> String {
    if names.is_empty() {
        "there are no memories yet".to_string()
    } else {
        format!("existing memories: {}", names.join(", "))
    }
}

/// Whether `name` may name an entry: `[a-z0-9][a-z0-9_-]{0,63}`.
///
/// The whole safety argument for the directory lives here: a name that passes
/// contains no separator, no dot, and no uppercase, so `<dir>/<name>.md` cannot
/// leave `dir`, cannot be a dotfile, and cannot collide on a case-folding
/// filesystem.
pub(crate) fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    name.len() <= MAX_NAME_LEN
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub name: String,
    pub first_line: String,
    pub bytes: u64,
}

pub(crate) enum Written {
    Saved(usize),
    Deleted,
}

/// A memory directory and the size ceiling for one entry.
#[derive(Clone, Debug)]
pub(crate) struct Store {
    dir: AbsPath,
    entry_max_bytes: usize,
}

impl Store {
    pub(crate) fn new(dir: AbsPath, entry_max_bytes: u32) -> Self {
        Self {
            dir,
            entry_max_bytes: entry_max_bytes as usize,
        }
    }

    pub(crate) fn dir(&self) -> &AbsPath {
        &self.dir
    }

    fn path(&self, name: &str) -> Result<PathBuf, StoreError> {
        if !valid_name(name) {
            return Err(StoreError::BadName(name.to_string()));
        }
        Ok(self.dir.as_path().join(format!("{name}.md")))
    }

    /// Every entry, sorted by name. A missing directory is an empty memory, and
    /// anything that is not a regular `<valid-name>.md` file is not an entry.
    pub(crate) fn list(&self) -> Result<Vec<Entry>, StoreError> {
        let read = match std::fs::read_dir(self.dir.as_path()) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut entries = Vec::new();
        for item in read {
            let item = item?;
            let file_name = item.file_name();
            let Some(name) = file_name
                .to_str()
                .and_then(|file| file.strip_suffix(".md"))
                .filter(|name| valid_name(name))
            else {
                continue;
            };
            // `DirEntry::metadata` does not follow symlinks: a link planted in
            // the directory is not an entry, so it cannot redirect a read.
            let meta = item.metadata()?;
            if !meta.is_file() {
                continue;
            }
            let text = std::fs::read_to_string(item.path()).unwrap_or_default();
            entries.push(Entry {
                name: name.to_string(),
                first_line: first_line(&text),
                bytes: meta.len(),
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    fn names(&self) -> Vec<String> {
        self.list()
            .map(|entries| entries.into_iter().map(|entry| entry.name).collect())
            .unwrap_or_default()
    }

    fn missing(&self, name: &str) -> StoreError {
        StoreError::Missing {
            name: name.to_string(),
            available: self.names(),
        }
    }

    pub(crate) fn read(&self, name: &str) -> Result<String, StoreError> {
        let path = self.path(name)?;
        let is_file = std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file());
        if !is_file {
            return Err(self.missing(name));
        }
        Ok(std::fs::read_to_string(path)?)
    }

    /// Replace an entry. Empty content deletes it; the return says which.
    pub(crate) fn replace(&self, name: &str, content: &str) -> Result<Written, StoreError> {
        let path = self.path(name)?;
        if content.is_empty() {
            return match std::fs::remove_file(&path) {
                Ok(()) => Ok(Written::Deleted),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    Err(self.missing(name))
                }
                Err(error) => Err(error.into()),
            };
        }
        self.check_size(name, content.len())?;
        self.write_atomic(&path, content)?;
        Ok(Written::Saved(content.len()))
    }

    /// Add to the end of an entry, creating it if it is new.
    pub(crate) fn append(&self, name: &str, content: &str) -> Result<Written, StoreError> {
        let path = self.path(name)?;
        let mut text = match self.read(name) {
            Ok(text) => text,
            Err(StoreError::Missing { .. }) => String::new(),
            Err(error) => return Err(error),
        };
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(content);
        // Checked before anything is written, so a refused append leaves the
        // entry exactly as it was.
        self.check_size(name, text.len())?;
        self.write_atomic(&path, &text)?;
        Ok(Written::Saved(text.len()))
    }

    fn check_size(&self, name: &str, size: usize) -> Result<(), StoreError> {
        if size > self.entry_max_bytes {
            return Err(StoreError::TooLarge {
                name: name.to_string(),
                size,
                limit: self.entry_max_bytes,
            });
        }
        Ok(())
    }

    /// Temp file in the same directory, then rename: a reader sees the old
    /// entry or the new one, never half of either. The temp file is a dotfile,
    /// which `list` ignores, and is removed if anything fails before the rename.
    fn write_atomic(&self, path: &std::path::Path, text: &str) -> Result<(), StoreError> {
        std::fs::create_dir_all(self.dir.as_path())?;
        let mut file = tempfile::Builder::new()
            .prefix(".tmp-")
            .tempfile_in(self.dir.as_path())?;
        file.write_all(text.as_bytes())?;
        file.flush()?;
        file.persist(path).map_err(|error| error.error)?;
        Ok(())
    }
}

/// The first non-empty line, without markdown heading markers, bounded so one
/// long line cannot dominate a listing.
pub(crate) fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let line = line.trim_start_matches('#').trim_start();
    match line.char_indices().nth(FIRST_LINE_CHARS) {
        Some((index, _)) => format!("{}…", &line[..index]),
        None => line.to_string(),
    }
}
