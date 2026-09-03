//! Snapshots of the working tree, so winding a conversation back can put the
//! files back with it.
//!
//! A rewind that only forgot the conversation would leave a person reading a
//! transcript that stops before the edits they are still looking at on disk.
//! What the turn *did* has to be as undoable as what it was asked.
//!
//! The store is a git repository of keke's own, kept beside the session log
//! under `$KEKE_HOME` and pointed at the project as its work tree. Git rather
//! than a copied directory because it is what every project already has an
//! answer for: `.gitignore` is honoured, so `node_modules` and build output
//! stay out without keke inventing a second exclusion language, and unchanged
//! files cost nothing to snapshot twice.
//!
//! It is deliberately *not* the project's own repository. keke never writes to
//! that: no commit, no stash, no index of the person's that a snapshot could
//! disturb. Someone who runs `git status` mid-session must see exactly what
//! they would have seen without keke running.
//!
//! It is also deliberately not a `git worktree`. A worktree is the other tool
//! for this shape of problem — codex has a whole crate for them — but it
//! answers a different question: *isolation*, giving an agent a checkout of
//! its own to work in. Isolation is not undo. A worktree would move the edits
//! out of the directory the person is looking at, needs the project to be a
//! git repository at all, cannot represent an uncommitted tree the person
//! already had, and leaves a second checkout on disk to reconcile afterwards.
//! Snapshotting in place keeps the person's own directory the one thing keke
//! and they are both looking at, works in a project that has no repository,
//! and costs one staged index rather than a full checkout.

use std::collections::BTreeSet;

use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;

use keke_paths::AbsPath;

/// Why a snapshot could not be taken or put back.
#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error("git is not installed, so keke cannot snapshot the working tree")]
    NoGit,
    #[error(
        "{megabytes} MB of files would go into every snapshot, over the {limit} MB \
         limit; raise checkpoints.max-tree-mb, or add what does not belong in a \
         snapshot to .gitignore"
    )]
    TooLarge { megabytes: u64, limit: u32 },
    #[error("git {command} failed: {message}")]
    Git { command: String, message: String },
    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// One snapshot of the working tree, named by the commit that holds it.
///
/// Opaque and cheap to copy: it goes in the session log, comes back on a
/// resume, and is handed to [`Checkpoints::restore`] much later.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Snapshot(String);

impl Snapshot {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for Snapshot {
    /// Rebuild one from what a log recorded. The store is the only thing that
    /// can say whether it still names anything, and it says so by failing to
    /// restore rather than by refusing to be named.
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl std::fmt::Display for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a restore did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Restored {
    /// The files that were put back, workspace-relative.
    pub files: Vec<String>,
    /// A snapshot of how the tree looked *before* the restore.
    ///
    /// Taken because a restore overwrites whatever is there now, including
    /// work a person did by hand while the turn ran. Undoing keke's undo has
    /// to be possible, or the safe move would be never to offer the restore.
    pub undo: Option<Snapshot>,
}

/// The snapshot store for one project, staging through an index of one
/// session's own.
///
/// The object database (`git_dir`) is shared by every session open on the
/// same project — that is what lets the first-ever session pay `git init`
/// once and every later `open` hit the early return. The index is not
/// shared: two sessions staging at once would otherwise serialize on the
/// same `index.lock`, and a session mid-`git add` would see the other's
/// half-staged tree. `--index-file` gives each session a private index over
/// the same objects and refs, so `git add`/`write-tree`/`commit-tree` never
/// contend across sessions; only concurrent snapshot-taking *and restoring*
/// files in the same project at the same time still race, at the working
/// tree itself rather than in git, and no index scheme changes that.
#[derive(Clone, Debug)]
pub struct Checkpoints {
    git_dir: PathBuf,
    work_tree: PathBuf,
    index_file: PathBuf,
    session: String,
}

/// Where a snapshot's ref lives.
///
/// Every snapshot is anchored under a ref of its own, because a commit that no
/// ref names is *unreachable*, and unreachable is what `git gc` exists to
/// delete. The store is a git repository like any other: the day something
/// runs a `gc` in it — git's own automatic one, a person tidying up, a backup
/// tool — every snapshot a resumed session still names would be gone, and the
/// session would find out by failing to restore. Naming them makes keke, not
/// git's default policy, the one that decides when a snapshot stops existing.
const REFS: &str = "refs/keke/snapshots";

/// The identity snapshots are committed under.
///
/// keke's own, never the person's: a snapshot is not authored by them, and one
/// that borrowed their name would put commits they did not make in a log that
/// looks like theirs. Passed per command rather than configured into the
/// repository so it cannot be picked up from a global `.gitconfig` that is
/// missing one.
const IDENTITY: [&str; 4] = ["-c", "user.name=keke", "-c", "user.email=keke@localhost"];

impl Checkpoints {
    /// Open (creating if needed) the store in `dir`, snapshotting `work_tree`.
    ///
    /// `dir` is keke's, not the project's. Creating it is idempotent, so a
    /// resumed session goes on adding to the snapshots the first run took.
    ///
    /// `keep_out` names directories that must never enter a snapshot — keke's
    /// own home above all. Usually it is elsewhere and this costs nothing, but
    /// nothing stops a deployment from putting `$KEKE_HOME` inside the project,
    /// and a store that snapshotted it would offer to restore the session log
    /// it is in the middle of writing.
    ///
    /// `session` names the caller's own index within this store — see the
    /// type's doc comment. Any string unique to the caller works; a session
    /// id is what every caller of this happens to already have.
    ///
    /// `keep_days` is how long a snapshot survives. Pruning runs here, once
    /// per session, rather than on every snapshot: it is housekeeping, and
    /// housekeeping that ran per turn would be a subprocess per turn for a
    /// store that changes size in days.
    ///
    /// `max_tree_mb` is the working tree keke refuses to snapshot at all — see
    /// [`Self::refuse_if_too_large`]. Checked only when the store is created,
    /// so it is a decision about a project rather than a check a session
    /// repeats.
    pub async fn open(
        dir: &Path,
        work_tree: &AbsPath,
        keep_out: &[&Path],
        session: &str,
        keep_days: u32,
        max_tree_mb: u32,
    ) -> Result<Self, CheckpointError> {
        let store = Self {
            git_dir: dir.to_path_buf(),
            work_tree: work_tree.as_path().to_path_buf(),
            index_file: dir.join(format!("index.{}", ref_safe(session))),
            session: ref_safe(session),
        };
        if store.git_dir.join("HEAD").exists() {
            // Best effort: a store that could not be tidied is a store that is
            // larger than it needs to be, which is not a reason to leave a
            // session with no snapshots at all.
            if let Err(error) = store.prune(keep_days).await {
                tracing::warn!(%error, "could not prune old snapshots");
            }
            store.seed_index().await;
            return Ok(store);
        }
        tokio::fs::create_dir_all(&store.git_dir)
            .await
            .map_err(|source| CheckpointError::Io {
                path: store.git_dir.display().to_string(),
                source,
            })?;
        // Plain `git init`, not the `--git-dir`/`--work-tree` form the rest of
        // the store uses: git refuses a work tree on the command that is
        // creating the directory it would belong to.
        let init = tokio::process::Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg("--bare")
            .arg(&store.git_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|source| {
                if source.kind() == std::io::ErrorKind::NotFound {
                    CheckpointError::NoGit
                } else {
                    CheckpointError::Io {
                        path: "git".to_string(),
                        source,
                    }
                }
            })?;
        if !init.status.success() {
            return Err(CheckpointError::Git {
                command: "init".to_string(),
                message: String::from_utf8_lossy(&init.stderr).trim().to_string(),
            });
        }
        // Bare is how the directory is laid out; it still has a work tree,
        // which is the project. Without this git refuses every command that
        // touches one.
        store.git(&["config", "core.bare", "false"], false).await?;
        store.keep_out(keep_out).await?;
        // Only on the store's first creation. A project that was small enough
        // yesterday is not re-measured every session, and one that has grown
        // past the limit since is not suddenly cut off from the snapshots it
        // already has.
        if let Err(error) = store.refuse_if_too_large(max_tree_mb).await {
            // Leave nothing behind. A store that was refused must not be
            // mistaken for an existing one by the next session, which would
            // take the early return above and skip the measuring entirely.
            let _ = tokio::fs::remove_dir_all(&store.git_dir).await;
            return Err(error);
        }
        Ok(store)
    }

    /// Refuse a working tree whose every snapshot would be enormous.
    ///
    /// A snapshot holds everything git would not ignore, and nothing about a
    /// project stops that from being a hundred gigabytes of video, model
    /// weights or captured data that happen not to be in a `.gitignore`.
    /// Copying that into `$KEKE_HOME` is not a slow snapshot, it is somebody's
    /// disk filling up because they ran a coding agent in the wrong directory
    /// — and they never asked for a snapshot in the first place, so the
    /// failure has to be keke's to prevent rather than theirs to discover.
    ///
    /// Measured with `ls-files`, which walks and applies the ignore rules but
    /// hashes nothing: 0.01s on a one-gigabyte tree, 0.18s on one of sixty
    /// thousand files. A guard this cheap has no excuse not to run.
    async fn refuse_if_too_large(&self, limit: u32) -> Result<(), CheckpointError> {
        let listed = self
            .git(
                &[
                    "ls-files",
                    "--others",
                    "--cached",
                    "--exclude-standard",
                    "-z",
                ],
                true,
            )
            .await?;
        let mut bytes: u64 = 0;
        let ceiling = u64::from(limit) * 1024 * 1024;
        for path in listed.split('\0').filter(|path| !path.is_empty()) {
            // Symlinks are not followed: what a snapshot stores is the link,
            // and a link pointing at something enormous costs nothing.
            let Ok(meta) = tokio::fs::symlink_metadata(self.work_tree.join(path)).await else {
                continue;
            };
            bytes = bytes.saturating_add(meta.len());
            if bytes > ceiling {
                return Err(CheckpointError::TooLarge {
                    megabytes: bytes / (1024 * 1024),
                    limit,
                });
            }
        }
        Ok(())
    }

    /// Write the exclusion rules for keke's own directories.
    ///
    /// Everything in `keep_out`, plus the store itself, expressed relative to
    /// the work tree — the ones that are not inside it need no rule and get
    /// none.
    async fn keep_out(&self, keep_out: &[&Path]) -> Result<(), CheckpointError> {
        let mut rules = String::new();
        for path in std::iter::once(self.git_dir.as_path()).chain(keep_out.iter().copied()) {
            let Ok(inside) = path.strip_prefix(&self.work_tree) else {
                continue;
            };
            if inside.as_os_str().is_empty() {
                continue;
            }
            rules.push_str(&format!(
                "/{}/\n",
                inside.display().to_string().replace('\\', "/")
            ));
        }
        if rules.is_empty() {
            return Ok(());
        }
        let info = self.git_dir.join("info");
        tokio::fs::create_dir_all(&info)
            .await
            .map_err(|source| CheckpointError::Io {
                path: info.display().to_string(),
                source,
            })?;
        let path = info.join("exclude");
        tokio::fs::write(&path, rules)
            .await
            .map_err(|source| CheckpointError::Io {
                path: path.display().to_string(),
                source,
            })
    }

    /// Take a snapshot of the working tree as it stands, labelled `label`.
    ///
    /// Everything git would not ignore, which is the point of using git: a
    /// project's own `.gitignore` already says what is not worth keeping, and
    /// keke honouring it means no second list to maintain and no `target/`
    /// copied on every turn.
    ///
    /// `Ok(None)` when there is nothing in the tree to snapshot at all — an
    /// empty project has no state to put back.
    pub async fn take(&self, label: &str) -> Result<Option<Snapshot>, CheckpointError> {
        self.stage().await?;
        let tree = self.git(&["write-tree"], true).await?;
        let tree = tree.trim();
        if tree.is_empty() {
            return Ok(None);
        }
        // Every snapshot is a root commit: they are points to go back to, not
        // a history to walk, and a chain would make the store grow a parent
        // link per turn for nothing.
        let commit = self.git(&["commit-tree", tree, "-m", label], true).await?;
        let commit = commit.trim().to_string();
        if commit.is_empty() {
            return Ok(None);
        }
        // Anchored under a ref before it is handed out, so it is reachable
        // from the moment anything can name it — see [`REFS`]. The commit id
        // is the ref's own name: it needs no counter to stay unique, and two
        // sessions that snapshot an identical tree land on one ref rather than
        // two names for one object.
        self.git(
            &[
                "update-ref",
                &format!("{REFS}/{}/{commit}", self.session),
                &commit,
            ],
            false,
        )
        .await?;
        Ok(Some(Snapshot(commit)))
    }

    /// Read the working tree into the index without recording anything.
    ///
    /// A snapshot's cost is almost entirely the first one: an index with no
    /// stat cache makes `git add` read every file in the project, measured at
    /// 25 seconds on a sixty-thousand-file tree against 0.2 for every snapshot
    /// after it. Paid where it lands, that is 25 seconds of a person waiting
    /// in the middle of the first turn that edits a file.
    ///
    /// So it is paid somewhere else. Nothing here is recorded and no ref
    /// moves, which is what makes this safe to run at any moment: whatever
    /// changes between warming and the real snapshot is picked up by that
    /// snapshot's own staging, so the snapshot means exactly what it meant
    /// before.
    pub async fn warm(&self) -> Result<(), CheckpointError> {
        self.stage().await
    }

    /// Which files differ between `snapshot` and the tree as it stands.
    ///
    /// What the confirm step reads to say how much a restore would touch —
    /// and, when it is empty, to say that restoring the files would do
    /// nothing, rather than offering it as though it would.
    pub async fn changed_since(&self, snapshot: &Snapshot) -> Result<Vec<String>, CheckpointError> {
        self.stage().await?;
        let out = self
            .git(
                &["diff", "--cached", "--name-only", snapshot.as_str()],
                true,
            )
            .await?;
        Ok(out
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Put the working tree back to `snapshot`.
    ///
    /// Files the turn changed go back to what they were, files it created are
    /// removed, files it deleted come back. Anything git ignores is left
    /// exactly where it is — it was never in the snapshot, so keke has no
    /// record of it to be right about.
    pub async fn restore(&self, snapshot: &Snapshot) -> Result<Restored, CheckpointError> {
        let files = self.changed_since(snapshot).await?;
        if files.is_empty() {
            return Ok(Restored::default());
        }
        let undo = self.take("before restore").await?;
        // `read-tree -u --reset` is the one that also removes what the turn
        // added: a plain `checkout -- .` restores contents and leaves new
        // files behind, which is a working tree matching neither snapshot.
        self.git(&["read-tree", "-u", "--reset", snapshot.as_str()], false)
            .await?;
        Ok(Restored { files, undo })
    }

    /// Drop snapshots older than `keep_days`, except any belonging to a
    /// session that is still working.
    ///
    /// A store nobody prunes grows for as long as the project does: every turn
    /// that writes adds a tree and the blobs it changed, and none of it was
    /// ever going to be asked for again once the session that took it ended.
    ///
    /// Age rather than a count, and this is the part that matters when more
    /// than one session is open on a project — which is the normal case under
    /// any kind of automation. A count is global: session B opening with a
    /// limit of *n* would delete the oldest snapshots in the store, and the
    /// oldest snapshots in the store are the ones session A took at the start
    /// of the long turn it is still running. A rewind in A would then fail on
    /// a snapshot B deleted, and neither session did anything wrong. Age is a
    /// property of the snapshot rather than of everything around it, so
    /// pruning cannot be made to depend on who else showed up.
    ///
    /// That leaves the session that has been running for longer than the
    /// window. Its own index file is the heartbeat that covers it: staging
    /// rewrites the index on every snapshot, so an index touched inside the
    /// window belongs to a session that has taken a snapshot inside the
    /// window, and none of its refs are touched however old they are.
    ///
    /// Deletes are batched through one `update-ref --stdin`: a store left
    /// unpruned for a long time has a lot of them, and one process is the
    /// difference between pruning being free and being the reason the first
    /// writing turn feels slow.
    async fn prune(&self, keep_days: u32) -> Result<(), CheckpointError> {
        let Some(cutoff) = cutoff(keep_days) else {
            return Ok(());
        };
        let listed = self
            .git(
                &[
                    "for-each-ref",
                    "--format=%(refname) %(committerdate:unix)",
                    REFS,
                ],
                true,
            )
            .await?;

        let mut expired: Vec<(&str, &str)> = Vec::new();
        let mut owners: BTreeSet<&str> = BTreeSet::new();
        for line in listed.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let Some((name, taken)) = line.rsplit_once(' ') else {
                continue;
            };
            let Some(session) = ref_owner(name) else {
                continue;
            };
            owners.insert(session);
            // A date git could not print is a ref keke does not understand the
            // age of, and a snapshot of unknown age is not one to delete.
            let Ok(taken) = taken.parse::<u64>() else {
                continue;
            };
            if taken < cutoff {
                expired.push((name, session));
            }
        }

        let mut working = BTreeSet::new();
        for session in &owners {
            if self.recently_active(session, cutoff).await {
                working.insert(*session);
            }
        }

        let mut deletes = String::new();
        for (name, session) in &expired {
            if working.contains(session) {
                continue;
            }
            deletes.push_str(&format!("delete {name}\n"));
        }
        if !deletes.is_empty() {
            self.git_with_stdin(&["update-ref", "--stdin"], deletes)
                .await?;
        }

        // A session still holding a snapshot, or still working, keeps its
        // index. Anything else is a file nothing can be staged against — an
        // index per session is cheap until nothing ever removes one.
        let mut keep: BTreeSet<&str> = working;
        let expired: BTreeSet<&str> = expired.iter().map(|(_, session)| *session).collect();
        for session in &owners {
            if !expired.contains(session) {
                keep.insert(session);
            }
        }
        self.drop_stale_indexes(&keep).await;
        Ok(())
    }

    /// Whether `session` has taken a snapshot since `cutoff`.
    ///
    /// Read from its index file's modification time, which staging rewrites on
    /// every snapshot. A session with no index file has never staged anything,
    /// and an unreadable time is treated as active — the cost of keeping a
    /// snapshot too long is disk, and the cost of deleting one too early is a
    /// rewind that cannot put the files back.
    async fn recently_active(&self, session: &str, cutoff: u64) -> bool {
        let path = self.git_dir.join(format!("index.{session}"));
        let Ok(metadata) = tokio::fs::metadata(&path).await else {
            return false;
        };
        let Ok(modified) = metadata.modified() else {
            return true;
        };
        modified
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(true, |since| since.as_secs() >= cutoff)
    }

    /// Start this session's index from an existing one rather than from
    /// nothing.
    ///
    /// An index is not only a list of paths: it carries a stat cache, and that
    /// cache is what lets `git add --all` skip re-reading a file whose size
    /// and mtime have not moved. A session starting with an empty index has no
    /// cache, so its first snapshot hashes the entire tree — measured at 3.7
    /// seconds against 0.27 on a sixty-thousand-file checkout, which is the
    /// difference between a second session starting and a second session
    /// stalling. Copying is not sharing: what the copy stages afterwards is
    /// its own, and the reason the index is per session in the first place
    /// survives intact.
    ///
    /// Best effort in both directions. Nothing to copy is the first session,
    /// which has to pay the cost once; a copy that fails leaves an empty index
    /// that is correct and merely slow.
    async fn seed_index(&self) {
        if self.index_file.exists() {
            return;
        }
        let Ok(mut entries) = tokio::fs::read_dir(&self.git_dir).await else {
            return;
        };
        let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with("index.") {
                continue;
            }
            let Ok(modified) = entry.metadata().await.and_then(|meta| meta.modified()) else {
                continue;
            };
            if newest.as_ref().is_none_or(|(seen, _)| modified > *seen) {
                newest = Some((modified, entry.path()));
            }
        }
        let Some((_, seed)) = newest else { return };
        if let Err(error) = tokio::fs::copy(&seed, &self.index_file).await {
            tracing::debug!(%error, "starting this session's index from nothing");
        }
    }

    /// Remove the per-session index files of sessions with nothing left.
    ///
    /// Best effort: an index that could not be removed is wasted disk, not a
    /// reason to fail the turn that was about to be snapshotted.
    async fn drop_stale_indexes(&self, live: &BTreeSet<&str>) {
        let Ok(mut entries) = tokio::fs::read_dir(&self.git_dir).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(session) = name.strip_prefix("index.") else {
                continue;
            };
            if session == self.session || live.contains(session) {
                continue;
            }
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }

    /// Bring the index up to date with the working tree.
    ///
    /// Every query and every snapshot starts here: the index is keke's own, so
    /// staging costs nothing anybody can see, and it is what makes
    /// `write-tree` and `diff --cached` describe the tree as it is *now*.
    async fn stage(&self) -> Result<(), CheckpointError> {
        self.git(&["add", "--all", "--", "."], false).await?;
        Ok(())
    }

    async fn git(&self, args: &[&str], want_output: bool) -> Result<String, CheckpointError> {
        self.run(args, want_output, None).await
    }

    /// Run a git command that reads its work from standard input.
    ///
    /// One process for a whole batch of ref deletes, rather than one per ref.
    async fn git_with_stdin(&self, args: &[&str], input: String) -> Result<(), CheckpointError> {
        self.run(args, false, Some(input)).await?;
        Ok(())
    }

    async fn run(
        &self,
        args: &[&str],
        want_output: bool,
        input: Option<String>,
    ) -> Result<String, CheckpointError> {
        let mut command = tokio::process::Command::new("git");
        command
            .args(IDENTITY)
            .arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.work_tree)
            // A private index per session, over the shared object store — see
            // the `Checkpoints` doc comment.
            .env("GIT_INDEX_FILE", &self.index_file)
            .current_dir(&self.work_tree)
            .args(args)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(if want_output {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stderr(Stdio::piped());
        let output = match input {
            None => command.output().await,
            Some(input) => {
                use tokio::io::AsyncWriteExt as _;

                match command.spawn() {
                    Err(error) => Err(error),
                    Ok(mut child) => {
                        if let Some(mut stdin) = child.stdin.take() {
                            let _ = stdin.write_all(input.as_bytes()).await;
                            let _ = stdin.shutdown().await;
                        }
                        child.wait_with_output().await
                    }
                }
            }
        };
        let output = output.map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                CheckpointError::NoGit
            } else {
                CheckpointError::Io {
                    path: "git".to_string(),
                    source,
                }
            }
        })?;
        if !output.status.success() {
            return Err(CheckpointError::Git {
                command: args.first().copied().unwrap_or("git").to_string(),
                message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// The instant before which a snapshot has outlived `keep_days`.
///
/// `None` when the clock is before the epoch or the window is longer than the
/// clock has run: neither is a reason to start deleting.
fn cutoff(keep_days: u32) -> Option<u64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    now.checked_sub(u64::from(keep_days) * 86_400)
}

/// Which session took the snapshot a ref names.
fn ref_owner(name: &str) -> Option<&str> {
    name.strip_prefix(REFS)?.trim_matches('/').split('/').next()
}

/// A session id as a single ref path component.
///
/// Callers pass whatever names them — a session id today, something else if
/// that ever changes — and git refuses a ref name with a space, a `~`, or a
/// `..` in it. Substituting rather than rejecting because a store that refused
/// to open over the shape of a name would take snapshots away from a session
/// that has no say in what it is called.
fn ref_safe(session: &str) -> String {
    let cleaned: String = session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "session".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    struct Project {
        _dir: tempfile::TempDir,
        /// Where `$KEKE_HOME` stands in for these tests: outside the project,
        /// which is where a real store lives.
        _home: tempfile::TempDir,
        root: AbsPath,
        store: Checkpoints,
    }

    async fn project() -> Project {
        project_storing_in(None).await
    }

    /// A window no test in here trips over by accident; the ones that are
    /// about retention build their own old snapshots.
    const KEEP: u32 = 14;

    /// Larger than any tree these tests build; the one about the limit says
    /// its own number.
    const ROOMY: u32 = 1_024;

    /// `inside` names a directory *within* the project to keep the store in,
    /// for the one test about that hazard.
    async fn project_storing_in(inside: Option<&str>) -> Project {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonicalize");
        let root = AbsPath::new(root).expect("absolute");
        let git_dir = match inside {
            Some(name) => root.as_path().join(name),
            None => std::fs::canonicalize(home.path())
                .expect("canonicalize")
                .join("checkpoints.git"),
        };
        let store = Checkpoints::open(&git_dir, &root, &[], "test", KEEP, ROOMY)
            .await
            .expect("opens");
        Project {
            _dir: dir,
            _home: home,
            root,
            store,
        }
    }

    fn write(root: &AbsPath, name: &str, text: &str) {
        let path = root.as_path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(path, text).expect("write");
    }

    fn read(root: &AbsPath, name: &str) -> Option<String> {
        std::fs::read_to_string(root.as_path().join(name)).ok()
    }

    #[tokio::test]
    async fn a_restore_undoes_every_kind_of_change_a_turn_can_make() {
        let project = project().await;
        write(&project.root, "kept.txt", "before");
        write(&project.root, "removed.txt", "delete me later");
        let snapshot = project
            .store
            .take("turn 1")
            .await
            .expect("snapshots")
            .expect("a tree with files in it");

        // What a turn might do: edit one file, delete another, add a third.
        write(&project.root, "kept.txt", "after");
        std::fs::remove_file(project.root.as_path().join("removed.txt")).expect("remove");
        write(&project.root, "added.txt", "new");

        let restored = project.store.restore(&snapshot).await.expect("restores");

        assert_eq!(read(&project.root, "kept.txt").as_deref(), Some("before"));
        assert_eq!(
            read(&project.root, "removed.txt").as_deref(),
            Some("delete me later"),
            "a file the turn deleted comes back"
        );
        assert_eq!(
            read(&project.root, "added.txt"),
            None,
            "a file the turn created goes away, or the tree matches neither snapshot"
        );
        let mut files = restored.files;
        files.sort();
        assert_eq!(files, vec!["added.txt", "kept.txt", "removed.txt"]);
    }

    /// The restore is itself undoable, because it overwrites whatever is on
    /// disk now — including work somebody did by hand while the turn ran.
    #[tokio::test]
    async fn a_restore_hands_back_a_way_to_undo_itself() {
        let project = project().await;
        write(&project.root, "file.txt", "first");
        let first = project
            .store
            .take("turn 1")
            .await
            .expect("snapshots")
            .expect("a tree");
        write(&project.root, "file.txt", "second");

        let restored = project.store.restore(&first).await.expect("restores");
        assert_eq!(read(&project.root, "file.txt").as_deref(), Some("first"));

        let undo = restored.undo.expect("a restore is undoable");
        project.store.restore(&undo).await.expect("undoes");
        assert_eq!(
            read(&project.root, "file.txt").as_deref(),
            Some("second"),
            "undoing keke's undo puts back what was there before it ran"
        );
    }

    /// A turn that changed nothing has nothing to put back, and the surface
    /// needs to be able to say so rather than offering a restore that would do
    /// nothing.
    #[tokio::test]
    async fn an_unchanged_tree_reports_nothing_to_restore() {
        let project = project().await;
        write(&project.root, "file.txt", "same");
        let snapshot = project
            .store
            .take("turn 1")
            .await
            .expect("snapshots")
            .expect("a tree");

        assert!(
            project
                .store
                .changed_since(&snapshot)
                .await
                .expect("compares")
                .is_empty()
        );
        let restored = project.store.restore(&snapshot).await.expect("restores");
        assert_eq!(restored, Restored::default());
        assert!(
            restored.undo.is_none(),
            "a restore that did nothing leaves no undo point behind"
        );
    }

    /// The project's own ignore rules are the exclusion list: keke does not
    /// invent a second one, and must not drag `target/` into a snapshot.
    #[tokio::test]
    async fn ignored_files_are_neither_snapshotted_nor_restored() {
        let project = project().await;
        write(&project.root, ".gitignore", "build/\n");
        write(&project.root, "src.txt", "source");
        write(&project.root, "build/out.bin", "artifact");
        let snapshot = project
            .store
            .take("turn 1")
            .await
            .expect("snapshots")
            .expect("a tree");

        write(&project.root, "src.txt", "edited");
        write(&project.root, "build/out.bin", "rebuilt");
        let restored = project.store.restore(&snapshot).await.expect("restores");

        assert_eq!(read(&project.root, "src.txt").as_deref(), Some("source"));
        assert_eq!(
            read(&project.root, "build/out.bin").as_deref(),
            Some("rebuilt"),
            "an ignored file was never in the snapshot, so the restore leaves it alone"
        );
        assert_eq!(restored.files, vec!["src.txt".to_string()]);
    }

    /// `$KEKE_HOME` is usually elsewhere, but a deployment may put it inside
    /// the project — and a store that snapshotted itself would fight git's own
    /// lock on its index halfway through a restore.
    #[tokio::test]
    async fn a_store_inside_the_project_stays_out_of_its_own_snapshots() {
        let project = project_storing_in(Some(".keke/checkpoints.git")).await;
        write(&project.root, "file.txt", "before");
        let snapshot = project
            .store
            .take("turn 1")
            .await
            .expect("snapshots")
            .expect("a tree");

        write(&project.root, "file.txt", "after");
        let restored = project.store.restore(&snapshot).await.expect("restores");

        assert_eq!(read(&project.root, "file.txt").as_deref(), Some("before"));
        assert_eq!(
            restored.files,
            vec!["file.txt".to_string()],
            "the store's own files must not appear as things a turn changed"
        );
    }

    /// A `$KEKE_HOME` inside the project is keke's own bookkeeping, not the
    /// person's work: snapshotting it would put the session log in a snapshot,
    /// and restoring one would roll back the log keke is writing into.
    #[tokio::test]
    async fn keke_s_own_home_never_enters_a_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonicalize");
        let root = AbsPath::new(root).expect("absolute");
        let home = root.as_path().join(".keke");
        let store = Checkpoints::open(
            &home.join("checkpoints.git"),
            &root,
            &[home.as_path()],
            "test",
            KEEP,
            ROOMY,
        )
        .await
        .expect("opens");

        write(&root, "file.txt", "before");
        write(&root, ".keke/sessions/log.jsonl", "turn one");
        let snapshot = store
            .take("turn 1")
            .await
            .expect("snapshots")
            .expect("a tree");

        write(&root, "file.txt", "after");
        write(&root, ".keke/sessions/log.jsonl", "turn one\nturn two");
        let restored = store.restore(&snapshot).await.expect("restores");

        assert_eq!(restored.files, vec!["file.txt".to_string()]);
        assert_eq!(
            std::fs::read_to_string(root.as_path().join(".keke/sessions/log.jsonl"))
                .expect("still there"),
            "turn one\nturn two",
            "the session log is keke's own record and must survive a restore"
        );
    }

    /// keke's store is keke's. A person running `git log` in their own project
    /// must not find keke's snapshots in it.
    #[tokio::test]
    async fn snapshots_never_touch_the_projects_own_repository() {
        let project = project().await;
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .current_dir(project.root.as_path())
                .args(args)
                .output()
                .expect("git runs")
        };
        git(&["init", "--quiet"]);
        git(&["config", "user.email", "someone@example.com"]);
        git(&["config", "user.name", "Someone"]);
        write(&project.root, "file.txt", "theirs");
        git(&["add", "-A"]);
        git(&["commit", "-qm", "their commit"]);

        write(&project.root, "file.txt", "the agent's edit");
        project.store.take("turn 1").await.expect("snapshots");

        let log = git(&["log", "--oneline"]);
        let log = String::from_utf8_lossy(&log.stdout);
        assert_eq!(log.lines().count(), 1, "keke committed into their history");
        let status = git(&["status", "--porcelain"]);
        assert_eq!(
            String::from_utf8_lossy(&status.stdout).trim(),
            "M file.txt",
            "their working tree must read exactly as it would without keke running"
        );
    }

    /// Two sessions of the same project share one object store, but must not
    /// share one index: a shared index would serialize concurrent `git add`
    /// on the same `index.lock`, and could interleave one session's staged
    /// tree with the other's.
    #[tokio::test]
    async fn two_sessions_snapshot_the_same_project_without_contending() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonicalize");
        let root = AbsPath::new(root).expect("absolute");
        let git_dir = root.as_path().join("checkpoints.git");

        let first = Checkpoints::open(&git_dir, &root, &[], "session-a", KEEP, ROOMY)
            .await
            .expect("first session opens");
        let second = Checkpoints::open(&git_dir, &root, &[], "session-b", KEEP, ROOMY)
            .await
            .expect("second session hits the HEAD-exists early return");

        write(&root, "a.txt", "from session a");
        write(&root, "b.txt", "from session b");
        let (a, b) = tokio::join!(first.take("turn from a"), second.take("turn from b"));
        assert!(a.expect("session a snapshots").is_some());
        assert!(b.expect("session b snapshots").is_some());
    }

    /// Run a git command directly against a store, the way something outside
    /// keke would.
    fn in_store(store: &Checkpoints, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(&store.git_dir)
            .arg("--work-tree")
            .arg(&store.work_tree)
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn snapshot_refs(store: &Checkpoints) -> Vec<String> {
        in_store(store, &["for-each-ref", "--format=%(refname)", REFS])
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// A snapshot no ref names is an unreachable commit, and unreachable is
    /// exactly what `git gc` deletes. The store is an ordinary repository that
    /// git's own automatic housekeeping — or a person tidying up — may collect
    /// at any time, so a snapshot a resumed session still names has to survive
    /// one.
    #[tokio::test]
    async fn a_snapshot_survives_a_garbage_collection() {
        let project = project().await;
        write(&project.root, "file.txt", "before");
        let snapshot = project
            .store
            .take("turn 1")
            .await
            .expect("snapshots")
            .expect("a tree");

        in_store(&project.store, &["gc", "--prune=now", "--quiet"]);

        write(&project.root, "file.txt", "after");
        let restored = project.store.restore(&snapshot).await.expect("restores");
        assert_eq!(restored.files, vec!["file.txt".to_string()]);
        assert_eq!(read(&project.root, "file.txt").as_deref(), Some("before"));
    }

    /// Give `session` a snapshot that was taken `days` ago.
    ///
    /// Built directly rather than by winding a clock: a snapshot's age is its
    /// commit's, and `commit-tree` takes that from the environment.
    fn backdate(store: &Checkpoints, session: &str, days: u64) -> String {
        const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        let when = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_secs()
            - days * 86_400;
        let out = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(&store.git_dir)
            .arg("--work-tree")
            .arg(&store.work_tree)
            .env("GIT_COMMITTER_DATE", format!("{when} +0000"))
            .env("GIT_AUTHOR_DATE", format!("{when} +0000"))
            .env("GIT_COMMITTER_NAME", "keke")
            .env("GIT_COMMITTER_EMAIL", "keke@localhost")
            .env("GIT_AUTHOR_NAME", "keke")
            .env("GIT_AUTHOR_EMAIL", "keke@localhost")
            .args(["commit-tree", EMPTY_TREE, "-m", "an old turn"])
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let commit = String::from_utf8_lossy(&out.stdout).trim().to_string();
        in_store(
            store,
            &["update-ref", &format!("{REFS}/{session}/{commit}"), &commit],
        );
        commit
    }

    /// Leave behind the index file a session that stopped a month ago would
    /// have left: present, and untouched since.
    fn stale_index(git_dir: &Path, session: &str, days: u64) {
        let path = git_dir.join(format!("index.{session}"));
        std::fs::write(&path, b"what a departed session left").expect("write");
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86_400);
        let file = std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open");
        file.set_times(std::fs::FileTimes::new().set_modified(when))
            .expect("backdate");
    }

    fn refs_owned_by(store: &Checkpoints, session: &str) -> Vec<String> {
        snapshot_refs(store)
            .into_iter()
            .filter(|name| ref_owner(name) == Some(session))
            .collect()
    }

    /// A store nobody prunes grows for as long as the project does, and what
    /// bounds it has to be the snapshot's own age.
    ///
    /// A count would be a property of everything *around* the snapshot: a
    /// second session opening on the project would delete the oldest snapshots
    /// in the store, which are the ones the first session took at the start of
    /// the turn it is still running. Under any kind of automation that is the
    /// normal case, and neither session did anything wrong.
    /// Nobody asks for a snapshot; keke takes one. So a project whose every
    /// snapshot would be enormous — a dataset, model weights, video, anything
    /// large that is simply not in a `.gitignore` — has to be keke's to refuse
    /// rather than the person's to discover when their disk is full.
    #[tokio::test]
    async fn an_enormous_working_tree_is_refused_rather_than_copied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonicalize");
        let root = AbsPath::new(root).expect("absolute");
        let git_dir = std::fs::canonicalize(home.path())
            .expect("canonicalize")
            .join("checkpoints.git");

        write(&root, "small.txt", "fine");
        std::fs::write(root.as_path().join("data.bin"), vec![0u8; 3 * 1024 * 1024]).expect("write");

        let refused = Checkpoints::open(&git_dir, &root, &[], "test", KEEP, 1).await;
        assert!(
            matches!(refused, Err(CheckpointError::TooLarge { limit: 1, .. })),
            "a tree over the limit is refused, not copied"
        );
        assert!(
            !git_dir.exists(),
            "a refused store leaves nothing behind, or the next session would \
             take the early return and never measure at all"
        );

        // The same project under a ceiling that fits is ordinary.
        let store = Checkpoints::open(&git_dir, &root, &[], "test", KEEP, ROOMY)
            .await
            .expect("opens");
        assert!(store.take("turn 1").await.expect("snapshots").is_some());
    }

    /// Warming is what makes the expensive first read of the tree happen off
    /// the critical path, and it is only safe if it records nothing. Whatever
    /// the tree does between the warming and the snapshot has to be what the
    /// snapshot says — otherwise a turn would be rewound to a tree that
    /// existed before it started rather than as it started.
    #[tokio::test]
    async fn warming_changes_when_the_reading_happens_and_nothing_else() {
        let project = project().await;
        write(&project.root, "file.txt", "as the turn started");
        project.store.warm().await.expect("warms");
        assert!(
            snapshot_refs(&project.store).is_empty(),
            "warming records nothing: no snapshot exists to rewind to yet"
        );

        // What a person does between keke warming the index and the turn
        // reaching its first writing tool.
        write(&project.root, "file.txt", "edited by hand meanwhile");
        write(&project.root, "late.txt", "and a new file");
        let snapshot = project
            .store
            .take("turn 1")
            .await
            .expect("snapshots")
            .expect("a tree");

        write(&project.root, "file.txt", "and then the turn wrote");
        std::fs::remove_file(project.root.as_path().join("late.txt")).expect("remove");
        project.store.restore(&snapshot).await.expect("restores");

        assert_eq!(
            read(&project.root, "file.txt").as_deref(),
            Some("edited by hand meanwhile"),
            "the snapshot is the tree as the turn found it, not as warming left it"
        );
        assert_eq!(
            read(&project.root, "late.txt").as_deref(),
            Some("and a new file"),
            "a file created after warming is still in the snapshot"
        );
    }

    #[tokio::test]
    async fn snapshots_are_retired_by_age_and_never_by_who_else_showed_up() {
        let project = project_storing_in(None).await;
        let git_dir = project.store.git_dir.clone();

        // A session that finished a fortnight ago, and one that took a
        // snapshot just now.
        backdate(&project.store, "departed", 30);
        stale_index(&git_dir, "departed", 30);
        write(&project.root, "file.txt", "now");
        let recent = project
            .store
            .take("turn 1")
            .await
            .expect("snapshots")
            .expect("a tree");

        Checkpoints::open(&git_dir, &project.root, &[], "arriving", 14, ROOMY)
            .await
            .expect("reopens");

        assert!(
            refs_owned_by(&project.store, "departed").is_empty(),
            "a month-old snapshot of a session that is gone is what retention is for"
        );
        assert!(
            snapshot_refs(&project.store)
                .iter()
                .any(|name| name.ends_with(recent.as_str())),
            "a snapshot inside the window stays, whoever else opens the store"
        );
        assert!(
            !git_dir.join("index.departed").exists(),
            "an index nothing can be staged against is disk keke never gets back"
        );
        assert!(
            git_dir.join("index.test").exists(),
            "a session that still owns a snapshot keeps its index"
        );
    }

    /// The session that has been running longer than the retention window is
    /// the one a store must not tidy up around. Its index file is the
    /// heartbeat that says so: staging rewrites it on every snapshot, so an
    /// index touched inside the window belongs to a session still working.
    #[tokio::test]
    async fn a_working_sessions_old_snapshots_are_left_alone() {
        let project = project_storing_in(None).await;
        let git_dir = project.store.git_dir.clone();

        let busy = Checkpoints::open(&git_dir, &project.root, &[], "busy", 14, ROOMY)
            .await
            .expect("opens");
        let old = backdate(&project.store, "busy", 30);
        // What makes it busy rather than departed: a snapshot taken now, which
        // is what writes its index.
        write(&project.root, "file.txt", "still going");
        busy.take("turn 40").await.expect("snapshots");

        Checkpoints::open(&git_dir, &project.root, &[], "arriving", 14, ROOMY)
            .await
            .expect("reopens");

        assert!(
            snapshot_refs(&project.store)
                .iter()
                .any(|name| name.ends_with(&old)),
            "a rewind to the start of a long turn must still find its snapshot"
        );
        assert!(git_dir.join("index.busy").exists());
    }

    /// A new session hashing the whole tree again is the difference between a
    /// second session starting in a moment and starting in seconds — measured
    /// at 3.7s against 0.27s on a sixty-thousand-file tree. The index carries
    /// a stat cache, and copying one is what makes the copy warm.
    #[tokio::test]
    async fn a_new_session_starts_from_an_existing_index() {
        let project = project_storing_in(None).await;
        let git_dir = project.store.git_dir.clone();
        write(&project.root, "file.txt", "first");
        let snapshot = project
            .store
            .take("turn 1")
            .await
            .expect("snapshots")
            .expect("a tree");

        let second = Checkpoints::open(&git_dir, &project.root, &[], "second", 14, ROOMY)
            .await
            .expect("opens");

        assert!(
            git_dir.join("index.second").exists(),
            "a new session's index is seeded rather than built from nothing"
        );
        // Seeded, not shared: what the second session stages is its own.
        assert_ne!(second.index_file, project.store.index_file);
        assert!(
            second
                .changed_since(&snapshot)
                .await
                .expect("compares")
                .is_empty(),
            "a seeded index describes the same tree the one it was copied from did"
        );
        write(&project.root, "file.txt", "changed by the second session");
        assert_eq!(
            second.changed_since(&snapshot).await.expect("compares"),
            vec!["file.txt".to_string()],
            "and goes on tracking the tree from there"
        );
    }

    /// git refuses a ref name with a space or a `..` in it, and a session that
    /// has no say in what it is called must not lose its snapshots over one.
    #[test]
    fn an_awkward_session_name_still_makes_a_ref() {
        assert_eq!(ref_safe("019a-b2c3"), "019a-b2c3");
        assert_eq!(ref_safe("a session..name"), "a-session--name");
        assert_eq!(ref_safe(""), "session");
    }
}
