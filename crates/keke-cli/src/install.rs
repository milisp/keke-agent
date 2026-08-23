//! Fetching a plugin onto this machine.
//!
//! The one place in keke that reaches a network for plugin content, kept out of
//! `keke-plugin` so that resolving and listing stay incapable of it.
//!
//! Fetching is done by shelling out to `git`. A person installing a plugin from
//! a repository already has git and already trusts it; linking a git
//! implementation to avoid calling the one on their machine would be a large
//! dependency bought with nothing.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;

/// Clone `url` into `into`, at `reference` if one is named.
///
/// Submodules are deliberately not recursed. A submodule is another repository
/// the person did not name, fetched under a URL the plugin author controls, and
/// nothing in the plugin format needs one.
pub(crate) fn clone(url: &str, reference: Option<&str>, into: &Path) -> Result<()> {
    let mut command = Command::new("git");
    command
        .arg("clone")
        .arg("--quiet")
        .arg("--no-recurse-submodules")
        .arg("--depth")
        .arg("1");
    if let Some(reference) = reference {
        command.arg("--branch").arg(reference);
    }
    command.arg("--").arg(url).arg(into);

    let output = command
        .output()
        .with_context(|| format!("running git clone for {url}"))?;

    if output.status.success() {
        return Ok(());
    }

    // A commit id cannot be cloned with `--branch`, and a shallow clone does not
    // contain it. Retry as a full clone plus a checkout rather than telling the
    // person their perfectly good sha is not a ref.
    if reference.is_some() {
        return clone_and_checkout(url, reference.unwrap_or_default(), into);
    }

    bail!(
        "git clone of {url} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn clone_and_checkout(url: &str, reference: &str, into: &Path) -> Result<()> {
    if into.exists() {
        std::fs::remove_dir_all(into).with_context(|| format!("clearing {}", into.display()))?;
    }
    run(
        Command::new("git")
            .arg("clone")
            .arg("--quiet")
            .arg("--no-recurse-submodules")
            .arg("--")
            .arg(url)
            .arg(into),
        &format!("cloning {url}"),
    )?;
    run(
        Command::new("git")
            .arg("-C")
            .arg(into)
            .arg("checkout")
            .arg("--quiet")
            .arg(reference),
        &format!("checking out {reference}"),
    )
}

/// The commit currently checked out in `repo`.
///
/// Recorded at install time so a person can say which version they are running
/// and `update` can say what changed. A working tree with no commit at all is
/// not an error here — a plugin copied from a directory is still a plugin.
pub(crate) fn revision(repo: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|revision| !revision.is_empty())
}

/// Copy a directory tree, skipping `.git`.
///
/// The history of where a plugin came from is not part of the plugin, and
/// copying it would put a second repository inside the person's plugin
/// directory for no one's benefit.
pub(crate) fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry.with_context(|| format!("reading {}", from.display()))?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let target = to.join(&name);
        let kind = entry.file_type().with_context(|| "reading a file type")?;
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target)
                .with_context(|| format!("copying {}", entry.path().display()))?;
        }
        // Symlinks are skipped rather than followed. A link in a fetched tree
        // points wherever its author chose, and resolution would refuse it
        // later anyway; not copying it keeps the refusal from ever being needed.
    }
    Ok(())
}

/// Replace `target` with `staged`, keeping the old copy until the move lands.
///
/// An update that fails halfway must not leave a person with half a plugin. The
/// old directory is moved aside first and only removed once the new one is in
/// place, so a failure leaves the previous version installed.
pub(crate) fn swap_in(staged: &Path, target: &Path) -> Result<()> {
    let previous: Option<PathBuf> = target.exists().then(|| {
        let mut aside = target.as_os_str().to_owned();
        aside.push(".replacing");
        PathBuf::from(aside)
    });

    if let Some(previous) = &previous {
        if previous.exists() {
            std::fs::remove_dir_all(previous)
                .with_context(|| format!("clearing {}", previous.display()))?;
        }
        std::fs::rename(target, previous)
            .with_context(|| format!("moving {} aside", target.display()))?;
    }

    match std::fs::rename(staged, target) {
        Ok(()) => {
            if let Some(previous) = &previous {
                let _ = std::fs::remove_dir_all(previous);
            }
            Ok(())
        }
        Err(error) => {
            if let Some(previous) = &previous {
                let _ = std::fs::rename(previous, target);
            }
            Err(error).with_context(|| format!("installing into {}", target.display()))
        }
    }
}

fn run(command: &mut Command, doing: &str) -> Result<()> {
    let output = command.output().with_context(|| doing.to_string())?;
    if !output.status.success() {
        bail!(
            "{doing} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
