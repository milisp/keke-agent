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
    /// `keep` is how many snapshots the store is allowed to hold. Pruning runs
    /// here, before this session has taken any, so the newest `keep` of what
    /// earlier sessions left survive and the rest are dropped.
    pub async fn open(
        dir: &Path,
        work_tree: &AbsPath,
        keep_out: &[&Path],
        session: &str,
        keep: usize,
    ) -> Result<Self, CheckpointError> {
        let store = Self {
            git_dir: dir.to_path_buf(),
            work_tree: work_tree.as_path().to_path_buf(),
            index_file: dir.join(format!("index.{}", ref_safe(session))),
            session: ref_safe(session),
        };
        if store.git_dir.join("HEAD").exists() {
            store.prune(keep).await?;
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
        Ok(store)
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

    /// Drop everything but the newest `keep` snapshots.
    ///
    /// A store nobody prunes is a store that grows for as long as the project
    /// does: every turn that writes adds a tree and the blobs it changed, and
    /// none of it was ever going to be asked for again once the session that
    /// took it ended. Retention is keke's to decide rather than git's, which
    /// is the other half of why snapshots are anchored under refs at all.
    ///
    /// Ordered by commit time, which for a snapshot is the moment it was
    /// taken. Deletes are batched through one `update-ref --stdin`: a store
    /// left unpruned for a long time has a lot of them, and one process is the
    /// difference between pruning being free and being the reason the first
    /// writing turn feels slow.
    async fn prune(&self, keep: usize) -> Result<(), CheckpointError> {
        let listed = self
            .git(
                &[
                    "for-each-ref",
                    "--sort=-committerdate",
                    "--format=%(refname)",
                    REFS,
                ],
                true,
            )
            .await?;
        let refs: Vec<&str> = listed
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        let Some(doomed) = refs.get(keep..) else {
            return Ok(());
        };
        if doomed.is_empty() {
            return Ok(());
        }
        let mut deletes = String::new();
        for name in doomed {
            deletes.push_str(&format!("delete {name}\n"));
        }
        self.git_with_stdin(&["update-ref", "--stdin"], deletes)
            .await?;
        // The sessions that still own a ref are the ones whose index file is
        // still worth the disk. Any other belongs to a session whose snapshots
        // just went, or that ended long ago — an index per session is cheap
        // until nothing ever removes one.
        let live: BTreeSet<&str> = refs[..refs.len().min(keep)]
            .iter()
            .filter_map(|name| name.strip_prefix(REFS)?.trim_matches('/').split('/').next())
            .collect();
        self.drop_stale_indexes(&live).await;
        Ok(())
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

    /// Enough that no test in here trips over retention by accident; the two
    /// that are about retention say their own number.
    const KEEP: usize = 100;

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
        let store = Checkpoints::open(&git_dir, &root, &[], "test", KEEP)
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

        let first = Checkpoints::open(&git_dir, &root, &[], "session-a", KEEP)
            .await
            .expect("first session opens");
        let second = Checkpoints::open(&git_dir, &root, &[], "session-b", KEEP)
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

    /// A store nobody prunes grows for as long as the project does. Retention
    /// is keke's decision, taken when the store opens and before this session
    /// has added anything of its own.
    #[tokio::test]
    async fn the_store_keeps_only_the_newest_snapshots() {
        let project = project_storing_in(None).await;
        write(&project.root, "file.txt", "first");
        let oldest = project
            .store
            .take("turn 1")
            .await
            .expect("snapshots")
            .expect("a tree");
        // Snapshots are ordered by the time they were taken, which is the
        // second they were committed in.
        tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
        write(&project.root, "file.txt", "second");
        let newest = project
            .store
            .take("turn 2")
            .await
            .expect("snapshots")
            .expect("a tree");
        assert_eq!(snapshot_refs(&project.store).len(), 2);

        let reopened = Checkpoints::open(
            &project.store.git_dir,
            &project.root,
            &[],
            "later-session",
            1,
        )
        .await
        .expect("reopens");

        let kept = snapshot_refs(&reopened);
        assert_eq!(kept.len(), 1, "retention is what bounds the store");
        assert!(
            kept[0].ends_with(newest.as_str()),
            "the snapshot a rewind is most likely to want is the one kept"
        );
        assert!(!kept[0].ends_with(oldest.as_str()));
    }

    /// An index file per session is cheap until nothing ever removes one. A
    /// session whose snapshots have all been pruned has nothing left the index
    /// could be staged against.
    #[tokio::test]
    async fn a_pruned_sessions_index_goes_with_its_snapshots() {
        let project = project_storing_in(None).await;
        let git_dir = project.store.git_dir.clone();
        let earlier = Checkpoints::open(&git_dir, &project.root, &[], "earlier", KEEP)
            .await
            .expect("opens");
        write(&project.root, "file.txt", "first");
        earlier.take("turn 1").await.expect("snapshots");
        assert!(git_dir.join("index.earlier").exists());

        tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
        write(&project.root, "file.txt", "second");
        let later = Checkpoints::open(&git_dir, &project.root, &[], "later", KEEP)
            .await
            .expect("opens");
        later.take("turn 2").await.expect("snapshots");

        // Reopening with room for one snapshot prunes the earlier session's,
        // and its index with it.
        Checkpoints::open(&git_dir, &project.root, &[], "later", 1)
            .await
            .expect("reopens");
        assert!(
            !git_dir.join("index.earlier").exists(),
            "an index nothing can be staged against is disk keke never gets back"
        );
        assert!(
            git_dir.join("index.later").exists(),
            "a session that still owns a snapshot keeps its index"
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
