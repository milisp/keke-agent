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
            auto_approve_bash: true,
            network_access,
            writable_roots: Vec::new(),
            ..Default::default()
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

#[cfg(target_os = "macos")]
#[test]
fn git_ssh_uses_the_domain_proxy_and_does_not_bypass_approval() {
    use std::sync::{Arc, Mutex};
    struct Refuse(Mutex<Vec<String>>);
    impl keke_sandbox::NetworkAuthorizer for Refuse {
        fn authorize(&self, host: &str) -> bool {
            self.0.lock().unwrap().push(host.to_string());
            false
        }
    }
    let (_dir, root) = workspace();
    let reviewer = Arc::new(Refuse(Mutex::new(Vec::new())));
    let base = Sandbox::new(
        SandboxPolicy {
            proxy_allowed_ports: vec![22, 80, 443, 8443],
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let scoped = base
        .with_network_authorizer(reviewer.clone(), Vec::new())
        .unwrap();
    let mut command = scoped.shell(
        "git ls-remote ssh://git@unapproved.invalid:22/repository",
        &root,
    );
    assert!(
        command.get_envs().any(|(key, _)| key == "GIT_SSH_COMMAND"),
        "test needs the default SSH transport, not an inherited override"
    );
    let output = command.output().unwrap();
    assert!(!output.status.success());
    assert_eq!(*reviewer.0.lock().unwrap(), vec!["unapproved.invalid"]);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("403"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn package_caches_are_writable_without_a_home_write_grant() {
    let (_dir, root) = workspace();
    let (ok, text) = run(
        &sandbox(SandboxMode::WorkspaceWrite, false),
        "mkdir -p \"$npm_config_cache\" \"$BUN_INSTALL_CACHE_DIR\" \"$PIP_CACHE_DIR\" \"$UV_CACHE_DIR\" \"$XDG_CACHE_HOME\" && for p in \"$npm_config_cache\" \"$BUN_INSTALL_CACHE_DIR\" \"$PIP_CACHE_DIR\" \"$UV_CACHE_DIR\" \"$XDG_CACHE_HOME\"; do echo cached > \"$p/keke-test\" || exit; done; echo cache-ok",
        &root,
    );
    assert!(ok, "{text}");
    assert_eq!(text.trim(), "cache-ok");
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires namespace launcher; scripts/test-linux-sandbox.sh runs this test"]
fn namespace_backend_protects_metadata_and_masks_denied_reads() {
    let launcher = std::env::var_os("KEKE_TEST_NAMESPACE_LAUNCHER")
        .expect("namespace test must configure its trusted launcher");
    let (_dir, root) = workspace();
    std::fs::create_dir(root.as_path().join(".agents")).unwrap();
    let secret = root.as_path().join("secret");
    std::fs::write(&secret, "credential-marker").unwrap();
    let confined = Sandbox::new(
        SandboxPolicy {
            linux_namespace_launcher: Some(AbsPath::new(PathBuf::from(launcher)).unwrap()),
            deny_read: vec![AbsPath::new(secret).unwrap()],
            ..Default::default()
        },
        Some(PathBuf::from(env!("CARGO_BIN_EXE_keke"))),
    )
    .unwrap();
    assert!(run(&confined, "echo allowed > normal", &root).0);
    assert!(!run(&confined, "echo forbidden > .agents/hook", &root).0);
    assert!(
        !run(
            &confined,
            "mkdir -p .keke; echo forbidden > .keke/config",
            &root
        )
        .0
    );
    let (_ok, text) = run(&confined, "cat secret", &root);
    assert!(!text.contains("credential-marker"));
    assert!(run(&confined, "test ! -e /proc/1/root", &root).0);
}

#[cfg(target_os = "linux")]
#[test]
fn unsupported_network_grants_fail_instead_of_running_unrestricted() {
    let helper = Some(PathBuf::from(env!("CARGO_BIN_EXE_keke")));
    for policy in [
        SandboxPolicy {
            allowed_domains: vec!["example.com".into()],
            ..Default::default()
        },
        SandboxPolicy {
            deny_read: vec![AbsPath::new(PathBuf::from("/credentials")).unwrap()],
            ..Default::default()
        },
    ] {
        assert!(Sandbox::new(policy, helper.clone()).is_err());
    }
    let base = sandbox(SandboxMode::WorkspaceWrite, false);
    assert!(base.with_local_network_ports(vec![8080]).is_err());
    assert!(
        base.with_local_network_sockets(vec!["/tmp/host.sock".into()])
            .is_err()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn restricted_commands_cannot_connect_to_host_unix_services() {
    use std::os::unix::net::UnixListener;
    let (_dir, root) = workspace();
    let path = root.as_path().join("host.sock");
    let _listener = UnixListener::bind(&path).unwrap();
    let output = sandbox(SandboxMode::WorkspaceWrite, false)
        .command(
            "curl",
            [
                "--noproxy",
                "*",
                "--max-time",
                "2",
                "--unix-socket",
                path.to_str().unwrap(),
                "http://localhost/",
            ],
            &root,
        )
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7), "{output:?}");
}

#[cfg(target_os = "macos")]
#[test]
fn unix_socket_client_grants_are_exact_and_command_scoped() {
    use std::os::unix::net::UnixListener;
    let (_workspace, root) = workspace();
    let host = outside();
    let path = host.path().join("host.sock");
    let other = host.path().join("other.sock");
    let _listener = UnixListener::bind(&path).unwrap();
    let _other_listener = UnixListener::bind(&other).unwrap();
    let base = sandbox(SandboxMode::WorkspaceWrite, false);
    let connect = |scope: &Sandbox, path: &Path| {
        scope
            .command(
                "/usr/bin/python3",
                [
                    "-c",
                    "import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])",
                    path.to_str().unwrap(),
                ],
                &root,
            )
            .output()
            .unwrap()
    };
    assert!(!connect(&base, &path).status.success());
    let scoped = base
        .with_local_network_sockets(vec![path.to_str().unwrap().into()])
        .unwrap();
    let result = connect(&scoped, &path);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!connect(&scoped, &other).status.success());
    assert!(!connect(&base, &path).status.success());
    assert!(
        !run(
            &scoped,
            &format!("echo forbidden > '{}'", host.path().join("file").display()),
            &root
        )
        .0
    );
    assert!(
        base.with_local_network_sockets(vec!["relative.sock".into()])
            .is_err()
    );
    assert!(
        sandbox(SandboxMode::ReadOnly, false)
            .with_local_network_sockets(vec![path.to_str().unwrap().into()])
            .is_err()
    );
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
fn additional_write_root_is_scoped_and_kernel_enforced() {
    let (_dir, root) = workspace();
    let host = outside();
    let base = sandbox(SandboxMode::WorkspaceWrite, false);
    assert!(base.is_enforced());
    let line = format!(
        "echo granted > '{}'",
        host.path().join("installed").display()
    );
    let (ok, text) = run(&base, &line, &root);
    assert!(!ok, "base write succeeded: {text}");
    assert!(
        !text.contains("sandbox_apply"),
        "sandbox must be active: {text}"
    );
    let scoped = base
        .with_additional_write_roots(vec![host.path().to_str().unwrap().into()])
        .unwrap();
    let (ok, text) = run(&scoped, &line, &root);
    assert!(ok, "scoped write failed: {text}");
    let (ok, text) = run(&base, &line, &root);
    assert!(!ok, "grant leaked: {text}");
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

#[cfg(target_os = "macos")]
#[test]
fn approved_local_client_port_is_scoped_and_other_ports_stay_blocked() {
    let (_dir, root) = workspace();
    let allowed = TcpListener::bind("127.0.0.1:0").expect("bind approved server");
    let denied = TcpListener::bind("127.0.0.1:0").expect("bind unrelated service");
    let port = allowed.local_addr().unwrap().port();
    let other = denied.local_addr().unwrap().port();
    let _keep_approved_listener_alive = allowed.try_clone().unwrap();
    allowed.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if let Ok((mut stream, _)) = allowed.accept() {
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0; 1024];
                let _ = stream.read(&mut request);
                stream
                    .write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 7\r\n\r\nreached")
                    .unwrap();
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    });
    let base = Sandbox::new(
        SandboxPolicy {
            allowed_domains: vec!["example.com".into()],
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let scoped = base.with_local_network_ports(vec![port]).unwrap();
    let (ok, text) = run(
        &scoped,
        &format!("curl -fsS --max-time 3 http://127.0.0.1:{port}/"),
        &root,
    );
    server.join().unwrap();
    assert!(ok, "approved port must bypass the HTTPS proxy: {text}");
    assert_eq!(text, "reached");
    for client in [&base, &scoped] {
        let (ok, text) = run(
            client,
            &format!("curl --noproxy '*' -fsS --max-time 2 http://127.0.0.1:{other}/"),
            &root,
        );
        assert!(!ok, "unrelated host service was reached: {text}");
        assert!(
            !text.contains("sandbox_apply"),
            "test requires actual confinement: {text}"
        );
    }
    let (ok, text) = run(
        &base,
        &format!("curl --noproxy '*' -fsS --max-time 2 http://127.0.0.1:{port}/"),
        &root,
    );
    assert!(!ok, "port grant leaked into the base sandbox: {text}");
    assert!(
        !text.contains("sandbox_apply"),
        "test requires actual confinement: {text}"
    );
    assert!(base.with_local_network_ports(vec![0]).is_err());
    assert!(
        sandbox(SandboxMode::ReadOnly, false)
            .with_local_network_ports(vec![port])
            .is_err()
    );
}

#[cfg(target_os = "macos")]
#[test]
fn domain_proxy_is_reachable_but_direct_network_is_not() {
    let (_dir, root) = workspace();
    let sandbox = Sandbox::new(
        SandboxPolicy {
            allowed_domains: vec!["example.com".to_string()],
            ..SandboxPolicy::default()
        },
        None,
    )
    .expect("domain sandbox");
    let (ok, text) = run(
        &sandbox,
        "curl -v --max-time 5 https://not-allowed.invalid/ 2>&1",
        &root,
    );
    assert!(!ok, "unexpected access: {text}");
    assert!(
        text.contains("403"),
        "must reach the proxy and receive its denial: {text}"
    );
    let (ok, text) = run(
        &sandbox,
        "curl -f -v --max-time 2 https://127.0.0.1:443 2>&1",
        &root,
    );
    assert!(!ok, "proxy must not reach loopback: {text}");
    assert!(
        text.contains("400"),
        "proxy must reject IP literals: {text}"
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("direct listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let port = listener.local_addr().expect("address").port();
    let (ok, text) = run(
        &sandbox,
        &format!("curl --noproxy '*' -sS --max-time 2 http://127.0.0.1:{port}"),
        &root,
    );
    assert!(!ok, "direct connection succeeded: {text}");
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "kernel must deny connection to a listening non-proxy port: {text}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn local_dev_server_can_listen_without_outbound_network_permission() {
    let (_dir, root) = workspace();
    let sandbox = sandbox(SandboxMode::WorkspaceWrite, false);
    let script = "import socket; s=socket.socket(); s.bind(('127.0.0.1',0)); s.listen(); print(s.getsockname()[1],flush=True); c,a=s.accept(); c.sendall(b'local-dev'); c.close()";
    let mut child = sandbox
        .command("/usr/bin/python3", ["-u", "-c", script], &root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("server starts");
    let mut line = String::new();
    use std::io::BufRead;
    std::io::BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("port");
    let port: u16 = match line.trim().parse() {
        Ok(port) => port,
        Err(_) => {
            let output = child.wait_with_output().expect("failure output");
            panic!(
                "listener did not start: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    };
    let mut stream = std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
        .expect("host accesses dev server");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .expect("timeout");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("response");
    assert_eq!(response, "local-dev");
    assert!(child.wait().expect("server exits").success());
    let restricted = Sandbox::new(
        SandboxPolicy {
            allow_local_binding: false,
            ..SandboxPolicy::default()
        },
        None,
    )
    .expect("restricted");
    let (ok, _) = run(
        &restricted,
        "exec /usr/bin/python3 -c \"import socket; s=socket.socket(); s.bind(('127.0.0.1',0)); s.listen()\"",
        &root,
    );
    assert!(!ok, "disabled binding must not run a server");
}

#[cfg(target_os = "macos")]
#[test]
fn bun_run_dev_serves_loopback_without_a_sandbox_escape() {
    let bun = std::process::Command::new("sh")
        .args(["-c", "command -v bun"])
        .output()
        .expect("find bun");
    if !bun.status.success() {
        eprintln!(
            "bun-specific test not run: bun is not installed; generic listener test covers kernel rules"
        );
        return;
    }
    let bun = String::from_utf8(bun.stdout).expect("bun path");
    let (_dir, root) = workspace();
    std::fs::write(
        root.as_path().join("package.json"),
        r#"{"scripts":{"dev":"bun dev.ts"}}"#,
    )
    .expect("package");
    std::fs::write(root.as_path().join("dev.ts"), "const s=Bun.serve({hostname:'127.0.0.1',port:0,fetch(){setTimeout(()=>s.stop(true),100);return new Response('bun-dev')}});console.log(s.port);").expect("server");
    let sandbox = sandbox(SandboxMode::WorkspaceWrite, false);
    let mut child = sandbox
        .command(bun.trim(), ["run", "dev"], &root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("bun starts");
    let mut line = String::new();
    use std::io::BufRead;
    std::io::BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("port");
    let port: u16 = match line.trim().parse() {
        Ok(port) => port,
        Err(_) => {
            let output = child.wait_with_output().expect("failure output");
            panic!(
                "bun dev did not start: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    };
    let mut stream =
        std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).expect("host connects");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .expect("timeout");
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("request");
    let mut response = String::new();
    let read = stream.read_to_string(&mut response);
    let _ = child.kill();
    let _ = child.wait();
    read.expect("response");
    assert!(response.contains("bun-dev"), "{response}");
}

#[cfg(target_os = "macos")]
#[test]
fn credential_denial_overrides_global_read_access_and_follows_symlinks() {
    let (_dir, root) = workspace();
    let secret = root.as_path().join("secret");
    std::fs::write(&secret, "credential").expect("secret");
    std::os::unix::fs::symlink(&secret, root.as_path().join("alias")).expect("alias");
    let (ok, text) = run(
        &sandbox(SandboxMode::WorkspaceWrite, false),
        "cat secret",
        &root,
    );
    assert!(
        ok && text == "credential",
        "control read must succeed: {text}"
    );
    let sandbox = Sandbox::new(
        SandboxPolicy {
            deny_read: vec![AbsPath::new(secret).expect("absolute")],
            ..SandboxPolicy::default()
        },
        None,
    )
    .expect("credential sandbox");
    for line in ["cat secret", "cat alias"] {
        let (ok, text) = run(&sandbox, line, &root);
        assert!(!ok, "{line} read the credential: {text}");
        assert!(!text.contains("credential"));
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn empty_domain_configuration_asks_during_the_original_command() {
    use keke_tool::Tool;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct Reviewer(Arc<AtomicUsize>);
    impl keke_tool::PermissionReviewer for Reviewer {
        fn permission(
            &self,
            domain: String,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(), keke_tool::ToolError>> + Send + '_>,
        > {
            Box::pin(async move {
                assert_eq!(domain, "additional.invalid");
                self.0.fetch_add(1, Ordering::SeqCst);
                Err(keke_tool::ToolError::denied("test refusal"))
            })
        }
    }
    let (_dir, root) = workspace();
    let calls = Arc::new(AtomicUsize::new(0));
    let bash = keke_tools::Bash {
        sandbox: Arc::new(sandbox(SandboxMode::WorkspaceWrite, false)),
        background: None,
    };
    let output = bash
        .run(
            keke_tool::ToolCallContext {
                call_id: keke_protocol::ToolCallId::new("dynamic-domain"),
                workspace_root: root,
                timeout_millis: Some(5000),
                cancelled: Arc::new(|| false),
                permission_reviewer: Some(Arc::new(Reviewer(Arc::clone(&calls)))),
                approved_network_domains: Vec::new(),
                approved_local_network_ports: Vec::new(),
                approved_local_network_sockets: Vec::new(),
                approved_writable_roots: Vec::new(),
            },
            keke_tools::BashArgs {
                command: "curl -v --max-time 3 https://additional.invalid/ 2>&1".into(),
                background: false,
                timeout_ms: Some(5000),
                allowed_domains: Vec::new(),
                allowed_local_ports: Vec::new(),
                allowed_unix_sockets: Vec::new(),
                additional_write_roots: Vec::new(),
            },
        )
        .await
        .expect("original command completes");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    match output {
        keke_tools::BashOutput::Finished {
            exit_code, output, ..
        } => {
            assert_ne!(exit_code, 0);
            assert!(
                output.contains("403"),
                "proxy denies after review: {output}"
            );
        }
        _ => panic!("expected foreground result"),
    }
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

/// A hook written into `.git` runs outside the sandbox at the next commit, and
/// `.keke` is the project's own configuration; both stay read-only inside a
/// writable workspace. Seatbelt can carve them out of the grant, Landlock
/// cannot, so this holds on macOS only — `docs/config.md` says so.
#[cfg(target_os = "macos")]
#[test]
fn workspace_metadata_stays_read_only() {
    let (_dir, root) = workspace();
    std::fs::create_dir_all(root.as_path().join(".git/hooks")).expect("mkdir");
    let sandbox = sandbox(SandboxMode::WorkspaceWrite, false);
    for line in [
        "echo 'curl evil' > .git/hooks/pre-commit",
        "mkdir .keke && echo 'sandbox_mode = \"danger_full_access\"' > .keke/config.toml",
    ] {
        let (ok, text) = run(&sandbox, line, &root);
        assert!(!ok, "`{line}` succeeded: {text}");
    }
    assert!(!root.as_path().join(".git/hooks/pre-commit").exists());
    assert!(!root.as_path().join(".keke").exists());
    let (ok, text) = run(&sandbox, "echo fine > src.txt", &root);
    assert!(ok, "the rest of the workspace stays writable: {text}");
}

/// Staging and committing are ordinary work, so they run as sandboxed commands
/// rather than as escapes a person must approve; what Git would later execute
/// on the person's behalf stays out of reach.
#[cfg(target_os = "macos")]
#[test]
fn a_sandboxed_command_can_commit_but_not_plant_a_hook() {
    let (_dir, root) = workspace();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root.as_path())
            .args(args)
            .output()
            .expect("git starts");
        assert!(output.status.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["config", "user.name", "Keke Test"]);
    git(&["config", "user.email", "keke@example.test"]);
    let sandbox = sandbox(SandboxMode::WorkspaceWrite, false);

    let (ok, text) = run(
        &sandbox,
        "echo hello > change.txt && git add change.txt && git commit -q -m 'Record change'",
        &root,
    );
    assert!(ok, "{text}");

    for line in [
        "echo 'curl evil' > .git/hooks/pre-commit",
        "git config core.hooksPath hooks",
        "mkdir -p .git/modules/x && echo evil > .git/modules/x/config",
    ] {
        let (ok, text) = run(&sandbox, line, &root);
        assert!(!ok, "`{line}` succeeded: {text}");
    }
    assert!(!root.as_path().join(".git/hooks/pre-commit").exists());
}
