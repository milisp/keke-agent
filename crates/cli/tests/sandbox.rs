//! The sandbox, enforced by the real kernel through the real binary.
//!
//! Lives here rather than in `keke-sandbox` because on Linux the launcher *is*
//! the `keke` binary, and only this crate's tests can name it. Nothing here is
//! skipped when the platform lacks a feature: a machine that cannot confine a
//! command fails these tests, because a suite that quietly passes without the
//! kernel feature it exists to check is the silent downgrade itself.

#![cfg(any(target_os = "macos", target_os = "linux"))]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io::Read as _;
use std::io::Write as _;
use std::net::TcpListener;
use std::path::Path;
use std::path::PathBuf;

use keke_config_types::SandboxMode;
use keke_config_types::SandboxPolicy;
use keke_paths::AbsPath;
use keke_sandbox::Sandbox;

fn sandbox(mode: SandboxMode, network_access: bool) -> Sandbox {
    Sandbox::new(
        SandboxPolicy {
            mode,
            network_access,
            writable_roots: Vec::new(),
        },
        Some(PathBuf::from(env!("CARGO_BIN_EXE_keke"))),
    )
    .expect("this machine must be able to enforce the sandbox")
}

fn workspace() -> (tempfile::TempDir, AbsPath) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = AbsPath::new(dir.path().canonicalize().expect("canonicalize")).expect("abs");
    (dir, root)
}

/// A directory no mode grants: not the workspace, not a temporary directory.
fn outside() -> tempfile::TempDir {
    let base = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let real = base.canonicalize().expect("canonicalize");
    let temp = std::env::temp_dir().canonicalize().expect("canonicalize");
    assert!(
        !real.starts_with(&temp) && !real.starts_with("/tmp") && !real.starts_with("/private/tmp"),
        "{} is under a temporary directory the sandbox grants, so it cannot stand for \
         somewhere outside",
        real.display()
    );
    tempfile::tempdir_in(real).expect("tempdir")
}

fn run(sandbox: &Sandbox, line: &str, workspace: &AbsPath) -> (bool, String) {
    let output = sandbox
        .shell(line, workspace)
        .output()
        .expect("the sandbox starts");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

#[test]
fn workspace_write_may_write_the_workspace() {
    let (_dir, root) = workspace();
    let (ok, text) = run(
        &sandbox(SandboxMode::WorkspaceWrite, false),
        "echo hi > made.txt && mkdir sub && echo there > sub/f && cat made.txt",
        &root,
    );
    assert!(ok, "{text}");
    assert_eq!(text.trim(), "hi");
}

#[test]
fn workspace_write_may_use_the_temporary_directory() {
    let (_dir, root) = workspace();
    let (ok, text) = run(
        &sandbox(SandboxMode::WorkspaceWrite, false),
        "f=$(mktemp) && echo x > \"$f\" && rm \"$f\" && echo x > /dev/null",
        &root,
    );
    assert!(ok, "{text}");
}

#[test]
fn workspace_write_cannot_write_outside_the_workspace() {
    let (_dir, root) = workspace();
    let outside = outside();
    let target = outside.path().join("escaped.txt");
    let (ok, text) = run(
        &sandbox(SandboxMode::WorkspaceWrite, false),
        // Through a child shell, because what is being confined is the
        // process tree and not one process.
        &format!("sh -c 'echo escaped > {}'", target.display()),
        &root,
    );
    assert!(!ok, "the write succeeded: {text}");
    assert!(!target.exists(), "{} was written", target.display());
}

#[test]
fn read_only_cannot_write_even_the_workspace() {
    let (_dir, root) = workspace();
    let (ok, text) = run(
        &sandbox(SandboxMode::ReadOnly, false),
        "cat /etc/hosts > /dev/null && echo x > made.txt",
        &root,
    );
    assert!(!ok, "the write succeeded: {text}");
    assert!(!root.as_path().join("made.txt").exists());
}

#[test]
fn every_mode_may_read_outside_the_workspace() {
    let (_dir, root) = workspace();
    let outside = outside();
    let file = outside.path().join("readable.txt");
    std::fs::write(&file, "visible").expect("write");
    for mode in [SandboxMode::ReadOnly, SandboxMode::WorkspaceWrite] {
        let (ok, text) = run(
            &sandbox(mode, false),
            &format!("cat {}", file.display()),
            &root,
        );
        assert!(ok, "{mode:?}: {text}");
        assert_eq!(text, "visible");
    }
}

/// A listener on loopback stands in for the network: if a command cannot
/// reach this, it cannot reach anything further away either.
fn fetch_from_loopback(sandbox: &Sandbox, root: &AbsPath) -> (bool, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 7\r\n\r\nreached");
        }
    });
    run(
        sandbox,
        &format!("curl -sS --max-time 5 http://127.0.0.1:{port}/"),
        root,
    )
}

#[test]
fn workspace_write_has_no_network_by_default() {
    let (_dir, root) = workspace();
    let (ok, text) = fetch_from_loopback(&sandbox(SandboxMode::WorkspaceWrite, false), &root);
    assert!(!ok, "the connection went through: {text}");
}

#[test]
fn network_access_opens_the_network() {
    let (_dir, root) = workspace();
    let (ok, text) = fetch_from_loopback(&sandbox(SandboxMode::WorkspaceWrite, true), &root);
    assert!(ok, "{text}");
    assert_eq!(text, "reached");
}

#[test]
fn danger_full_access_confines_nothing() {
    let (_dir, root) = workspace();
    let outside = outside();
    let target = outside.path().join("allowed.txt");
    let (ok, text) = run(
        &sandbox(SandboxMode::DangerFullAccess, false),
        &format!("echo ok > {}", target.display()),
        &root,
    );
    assert!(ok, "{text}");
    assert!(target.exists());
}
