use keke_config_types::SandboxMode;
use keke_protocol::ContentBlock;
use keke_sandbox::Sandbox;
use keke_tasks::BackgroundTasks;
use keke_tool::ApprovalRequirement;
use keke_tool::ListToolsContext;
use keke_tool::Tool;
use keke_tool::ToolCallContext;
use keke_tool::ToolCapabilities;
use keke_tool::ToolDescription;
use keke_tool::ToolError;
use keke_tool::ToolId;
use keke_tool::ToolKind;
use keke_tool::ToolOutput;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::time::Instant;
use tokio::process::Command;

use crate::support;

/// Used when the model names no budget.
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// The ceiling the model cannot raise past. Advertised as this tool's
/// `ToolCapabilities::timeout_millis`, which is what the engine enforces, so
/// the two numbers are the same number rather than two that can drift.
const MAX_TIMEOUT_MS: u64 = 600_000;
/// How often cancellation is observed while the child runs.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BashArgs {
    /// Shell command line, run from the workspace root.
    pub command: String,
    /// Wall-clock budget in milliseconds. Defaults to two minutes, capped at
    /// ten. Ignored when `background` is set — a background command has no
    /// budget, because nothing is waiting on it.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Start it and return at once, with a task id instead of output. Use for
    /// anything long-lived: a dev server, a watch, a build you want to check
    /// back on.
    #[serde(default)]
    pub background: bool,
    /// HTTP download/HTTPS hosts required by this command. Auto reviews these with the
    /// command; unlisted hosts are refused and must be declared on a retry.
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    /// Localhost TCP ports this command needs to access. Reviewed with the
    /// command, never remembered; may reach host services outside the workspace.
    #[serde(default)]
    pub allowed_local_ports: Vec<u16>,
    /// Localhost dev-server ports exposed to the host for this command only.
    /// Requires fresh approval; does not grant client access to host services.
    #[serde(default)]
    pub exposed_local_ports: Vec<u16>,
    /// Absolute Unix socket paths required by this command. Explicit approval
    /// is command-scoped and may reach host services. macOS workspace-write only.
    #[serde(default)]
    pub allowed_unix_sockets: Vec<String>,
    /// Existing absolute directories to write for this command only (for
    /// example shared Cargo caches or install destinations). Requires approval.
    #[serde(default)]
    pub additional_write_roots: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum BashOutput {
    /// Ran to completion, which is what the model gets unless it asked
    /// otherwise.
    Finished {
        exit_code: i32,
        /// Interleaved stdout then stderr, already capped.
        output: String,
        truncated: bool,
    },
    /// Started and left running. The id is how every later call names it.
    Started { task_id: String },
}

impl ToolOutput for BashOutput {
    fn render(&self) -> Vec<ContentBlock> {
        let (exit_code, output) = match self {
            Self::Started { task_id } => {
                return vec![ContentBlock::text(format!(
                    "started {task_id} in the background — read it with `task_output`, stop it \
                     with `kill_task`"
                ))];
            }
            Self::Finished {
                exit_code, output, ..
            } => (*exit_code, output),
        };
        let mut text = output.clone();
        if text.is_empty() {
            text.push_str("(no output)");
        }
        if exit_code != 0 {
            text.push_str(&format!("\n[exit {exit_code}]"));
        }
        vec![ContentBlock::text(text)]
    }
}

/// Runs a shell command in the workspace root.
///
/// The background half is delegated rather than implemented here: a task that
/// outlives the turn cannot be owned by the call that started it, and
/// `keke-tasks` is the one place that records what a task is doing.
pub struct Bash {
    /// What confines the command. Held here rather than consulted per call
    /// because it was checked to be enforceable when it was built, and a
    /// sandbox that could fail to build mid-turn would need a fallback — the
    /// only one available being to run the command bare.
    pub sandbox: Arc<Sandbox>,
    /// Where a backgrounded command goes. `None` in a composition with no task
    /// registry, which makes `background: true` an error rather than a silent
    /// foreground run — the model asked not to wait, and quietly waiting is a
    /// different answer to a different question (`AGENTS.md` invariant 8).
    pub background: Option<Arc<BackgroundTasks>>,
}

/// What a confined command cannot do, in words a model can act on. `None`
/// when commands are not confined.
fn sandbox_limits(sandbox: &Sandbox) -> Option<String> {
    if !sandbox.confines() {
        return None;
    }
    let policy = sandbox.policy();
    let writes = match policy.mode {
        SandboxMode::ReadOnly => "nothing may be written",
        _ => {
            "writes are allowed only inside the workspace and the temporary directory; \
              `git add` and `git commit` work, but Git hooks and configuration are read-only"
        }
    };
    let listener = if cfg!(target_os = "macos")
        && policy.mode == SandboxMode::WorkspaceWrite
        && policy.allow_local_binding
    {
        "; local TCP servers may bind loopback (127.0.0.1 or ::1); configure dev servers to use localhost rather than 0.0.0.0; this does not grant client access to arbitrary local services"
    } else {
        ""
    };
    let network = if policy.mode == SandboxMode::WorkspaceWrite && policy.network_access {
        ""
    } else if cfg!(target_os = "macos") && policy.mode == SandboxMode::WorkspaceWrite {
        "; HTTP GET/HEAD downloads and HTTPS CONNECT use a domain-filtering proxy; required new domains are approved within the command flow, direct connections and explicitly denied domains stay blocked"
    } else {
        "; there is no network access"
    };
    Some(format!("{writes}{network}{listener}"))
}

struct NetworkReview {
    reviewer: Arc<dyn keke_tool::PermissionReviewer>,
    runtime: tokio::runtime::Handle,
    stopped: Arc<AtomicBool>,
    cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
    refused: Arc<std::sync::Mutex<Vec<String>>>,
}

impl keke_sandbox::NetworkAuthorizer for NetworkReview {
    fn authorize(&self, domain: &str) -> bool {
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        let reviewer = Arc::clone(&self.reviewer);
        let domain = domain.to_string();
        let refused = Arc::clone(&self.refused);
        let task = self.runtime.spawn(async move {
            let decision = reviewer.permission(domain).await;
            if let Err(error) = &decision
                && let Ok(mut failures) = refused.lock()
            {
                let message = error.to_string();
                if !failures.contains(&message) {
                    failures.push(message);
                }
            }
            let _ = send.send(decision.is_ok());
        });
        loop {
            if self.stopped.load(Ordering::Acquire) || (self.cancelled)() {
                task.abort();
                return false;
            }
            match receive.recv_timeout(POLL_INTERVAL) {
                Ok(allowed) => return allowed,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return false;
                }
            }
        }
    }
}

struct NetworkScope(Arc<AtomicBool>);
impl Drop for NetworkScope {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl Tool for Bash {
    type Args = BashArgs;
    type Output = BashOutput;

    fn id(&self) -> ToolId {
        ToolId::new("bash")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        let mut text = String::from(
            "Run a shell command from the workspace root. Returns stdout and stderr combined, \
             plus the exit code when it is non-zero. Long output is truncated, so pipe through \
             `head` when you expect a lot. Set `background` for anything long-lived — a dev \
             server, a watch, a long build — to get a task id back immediately instead of \
             blocking the turn. Declare required HTTP download/HTTPS hosts in `allowed_domains` for combined \
             command/network approval. In Auto mode, if a domain is reported as unapproved, \
             add it to `allowed_domains` and rerun inside the sandbox, not with an escape.",
        );
        text.push_str(" Declare localhost TCP client ports in `allowed_local_ports` (1..=65535) for command-scoped approval, including HTTP, WebSocket and browser tests. This may access host services; it is not restricted to servers started by this session. Use localhost, 127.0.0.1 or ::1 directly. Supported in macOS workspace-write only; no local port grants are remembered.");
        text.push_str(" Declare absolute Unix socket paths in `allowed_unix_sockets` for explicit command-scoped host-service approval. macOS workspace-write only; grants are never remembered and do not grant filesystem writes.");
        text.push_str(" Declare localhost dev-server ports in `exposed_local_ports` (1..=65535) to expose them to the host for this command only. Requires fresh approval and does not grant client access to host services.");
        text.push_str(" Declare existing absolute directories in `additional_write_roots` for scoped cache/install writes without escaping (for example Cargo registry caches). Prefer narrow cache directories, not HOME or credential/toolchain roots. npm/Bun/pip/uv/XDG caches are redirected to temp by default when enabled. Approval policy Never automatically authorizes these execution permissions without asking; explicit denials and kernel restrictions still apply.");
        text.push_str(" Remote proxy ports are constrained by user `proxy_allowed_ports` (default 80/443); a domain approval does not override that ceiling. macOS default Git SSH is routed through CONNECT; SSH on port 22 needs that port in user configuration. Existing SSH transport overrides are preserved but direct networking stays denied. Standalone SSH needs an explicit proxy configuration.");
        // The model cannot tell a sandbox denial from any other failure unless
        // it knows the sandbox is there, and would otherwise retry the same
        // command or give up on a task a person would happily approve.
        if let Some(limits) = sandbox_limits(&self.sandbox) {
            text.push_str(&format!(
                "\n\nCommands run in a sandbox: {limits}. Network permissions are handled by \
                 the harness during execution; do not request sandbox escape for an unapproved domain. \
                 When a command fails because of \
                 that — \"Operation not permitted\", \"Permission denied\", or a network error \
                 — and it genuinely needs more, rerun it with `bash_unsandboxed`, which requires \
                 review first (automatic in Auto mode, by a person in human modes; Never skips execution review)."
            ));
        }
        ToolDescription::new(text)
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            // Where the configured sandbox does not exist, a person stands in
            // for it on every command — including the ones a standing "allow
            // always" or a permissive policy would have waved through, since
            // both assumed the command would be confined.
            approval: if !self.sandbox.is_enforced() {
                ApprovalRequirement::Always
            } else if self.sandbox.policy().mode == SandboxMode::WorkspaceWrite
                && self.sandbox.policy().auto_approve_bash
            {
                ApprovalRequirement::AutoApproved
            } else {
                ApprovalRequirement::ByPolicy
            },
            kind: ToolKind::Execute,
            // A shell command can touch anything the other calls in the step
            // are touching, so it never runs beside a sibling.
            concurrency_safe: false,
            timeout_millis: Some(MAX_TIMEOUT_MS),
        }
    }

    fn network_domains(&self, args: &Self::Args) -> Vec<String> {
        if cfg!(target_os = "macos")
            && self.sandbox.policy().mode == SandboxMode::WorkspaceWrite
            && !self.sandbox.policy().network_access
        {
            args.allowed_domains
                .iter()
                .map(|domain| domain.to_ascii_lowercase())
                .collect()
        } else {
            Vec::new()
        }
    }

    fn local_network_ports(&self, args: &Self::Args) -> Vec<u16> {
        if self.sandbox.confines() && !self.sandbox.policy().network_access {
            args.allowed_local_ports.clone()
        } else {
            Vec::new()
        }
    }

    fn local_listener_ports(&self, args: &Self::Args) -> Vec<u16> {
        args.exposed_local_ports.clone()
    }

    fn local_network_sockets(&self, args: &Self::Args) -> Vec<String> {
        args.allowed_unix_sockets.clone()
    }

    fn writable_roots(&self, args: &Self::Args) -> Vec<String> {
        args.additional_write_roots.clone()
    }

    async fn run(&self, ctx: ToolCallContext, args: Self::Args) -> Result<Self::Output, ToolError> {
        if args.exposed_local_ports.contains(&0) {
            return Err(ToolError::denied(
                "exposed_local_ports must be in 1..=65535",
            ));
        }
        if args
            .exposed_local_ports
            .iter()
            .any(|port| !ctx.approved_local_listener_ports.contains(port))
        {
            return Err(ToolError::denied(
                "localhost server ports exposed to the host require explicit command approval",
            ));
        }
        for path in &args.additional_write_roots {
            if !ctx.approved_writable_roots.contains(path) {
                return Err(ToolError::denied(
                    "additional write roots require explicit command approval",
                ));
            }
        }
        let write_sandbox = self
            .sandbox
            .with_additional_write_roots(args.additional_write_roots.clone())
            .map_err(|error| ToolError::denied(error.to_string()))?;
        for path in &args.allowed_unix_sockets {
            if !std::path::Path::new(path).is_absolute() || path.contains('\0') {
                return Err(ToolError::denied(
                    "allowed_unix_sockets must be absolute paths without NUL",
                ));
            }
            if !ctx.approved_local_network_sockets.contains(path) {
                return Err(ToolError::denied(
                    "Unix socket paths require explicit command approval",
                ));
            }
        }
        if args.allowed_local_ports.contains(&0) {
            return Err(ToolError::denied(
                "allowed_local_ports must be in 1..=65535",
            ));
        }
        if self.sandbox.confines()
            && !self.sandbox.policy().network_access
            && args
                .allowed_local_ports
                .iter()
                .any(|port| !ctx.approved_local_network_ports.contains(port))
        {
            return Err(ToolError::denied(
                "local TCP ports require explicit command approval",
            ));
        }
        for domain in &args.allowed_domains {
            keke_config_types::SandboxPolicy::check_domain_pattern(domain)
                .map_err(ToolError::denied)?;
            if domain.starts_with("*.") || !domain.contains('.') {
                return Err(ToolError::denied(
                    "per-command allowed_domains must name fully qualified exact hosts, not wildcards",
                ));
            }
        }
        let stopped = Arc::new(AtomicBool::new(false));
        let refused = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sandbox = if let Some(reviewer) = &ctx.permission_reviewer {
            Arc::new(
                write_sandbox
                    .with_network_authorizer(
                        Arc::new(NetworkReview {
                            reviewer: Arc::clone(reviewer),
                            runtime: tokio::runtime::Handle::current(),
                            stopped: Arc::clone(&stopped),
                            cancelled: Arc::clone(&ctx.cancelled),
                            refused: Arc::clone(&refused),
                        }),
                        ctx.approved_network_domains.clone(),
                    )
                    .map_err(|error| {
                        ToolError::custom("network_sandbox_unavailable", error.to_string())
                    })?,
            )
        } else {
            Arc::new(write_sandbox)
        };
        let sandbox = Arc::new(
            sandbox
                .with_local_network_ports(args.allowed_local_ports.clone())
                .and_then(|sandbox| {
                    sandbox.with_local_network_sockets(args.allowed_unix_sockets.clone())
                })
                .and_then(|sandbox| {
                    sandbox.with_local_listener_ports(args.exposed_local_ports.clone())
                })
                .map_err(|error| {
                    ToolError::custom("local_network_unavailable", error.to_string())
                })?,
        );
        // This must drop before the scoped proxy joins a pending reviewer.
        let scope = NetworkScope(stopped);
        if args.background {
            let Some(tasks) = self.background.as_ref() else {
                return Err(ToolError::custom(
                    "background_unavailable",
                    "this session has no background task registry",
                ));
            };
            let id = tasks
                .spawn_with_sandbox(args.command, &ctx.workspace_root, sandbox, scope)
                .map_err(|error| ToolError::custom("background_refused", error.to_string()))?;
            return Ok(BashOutput::Started { task_id: id });
        }

        // Clamp to the budget the engine is enforcing rather than to a local
        // copy of it: overrunning gets the call killed from outside, losing
        // whatever output the command had produced.
        let ceiling = ctx.timeout_millis.unwrap_or(MAX_TIMEOUT_MS);
        let millis = args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).min(ceiling);
        let deadline = Instant::now() + Duration::from_millis(millis);

        let child = Command::from(sandbox.shell(&args.command, &ctx.workspace_root))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Abandoning the wait future must not leave the child running: both
            // the timeout and cancellation paths drop it.
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| ToolError::custom("spawn_failed", error.to_string()))?;
        // `kill_on_drop` stops only the shell; this stops what it started too,
        // on timeout, cancellation and any other way out of this function.
        let _tree = keke_sandbox::ProcessTree::of(child.id());

        let wait = child.wait_with_output();
        tokio::pin!(wait);

        let output = loop {
            tokio::select! {
                finished = &mut wait => {
                    break finished.map_err(|error| {
                        ToolError::custom("wait_failed", error.to_string())
                    })?;
                }
                () = tokio::time::sleep(POLL_INTERVAL) => {
                    if ctx.is_cancelled() {
                        return Err(ToolError::Cancelled);
                    }
                    if Instant::now() >= deadline {
                        return Err(ToolError::Timeout { millis });
                    }
                }
            }
        };

        let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.stderr.is_empty() {
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        if let Ok(failures) = refused.lock() {
            for failure in failures.iter() {
                combined.push_str(&format!("\nNetwork permission: {failure}\n"));
            }
        }
        let (text, truncated) = support::cap(combined, "output truncated");

        Ok(BashOutput::Finished {
            exit_code: output.status.code().unwrap_or(-1),
            output: text,
            truncated,
        })
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;

    #[test]
    fn bash_domain_declarations_are_optional_in_existing_calls() {
        let args: BashArgs = serde_json::from_str(r#"{"command":"gh pr list"}"#).unwrap();
        assert!(args.allowed_domains.is_empty());
        assert!(args.allowed_local_ports.is_empty());
        assert!(args.exposed_local_ports.is_empty());
        assert!(args.allowed_unix_sockets.is_empty());
    }

    #[tokio::test]
    async fn listener_ports_require_nonzero_values_and_dispatch_approval_before_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("spawned");
        let tool = Bash {
            sandbox: Arc::new(
                Sandbox::new(
                    keke_config_types::SandboxPolicy {
                        mode: SandboxMode::DangerFullAccess,
                        ..Default::default()
                    },
                    None,
                )
                .unwrap(),
            ),
            background: None,
        };
        for (ports, expected) in [
            (vec![0], "1..=65535"),
            (vec![3000], "explicit command approval"),
        ] {
            let ctx = ToolCallContext {
                call_id: keke_protocol::ToolCallId::new("listener"),
                workspace_root: keke_paths::AbsPath::new(dir.path()).unwrap(),
                timeout_millis: None,
                cancelled: Arc::new(|| false),
                permission_reviewer: None,
                approved_network_domains: Vec::new(),
                approved_local_network_ports: Vec::new(),
                approved_local_listener_ports: Vec::new(),
                approved_local_network_sockets: Vec::new(),
                approved_writable_roots: Vec::new(),
            };
            let args: BashArgs = serde_json::from_value(serde_json::json!({
                "command": format!("touch '{}'", marker.display()),
                "exposed_local_ports": ports,
            }))
            .unwrap();
            assert_eq!(Tool::local_listener_ports(&tool, &args), ports);
            let error = tool.run(ctx, args).await.unwrap_err();
            assert!(error.to_string().contains(expected));
            assert!(!marker.exists());
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn unix_sockets_without_dispatch_approval_cannot_start_a_command() {
        let tool = Bash {
            sandbox: Arc::new(Sandbox::new(Default::default(), None).unwrap()),
            background: None,
        };
        let ctx = ToolCallContext {
            call_id: keke_protocol::ToolCallId::new("unapproved-socket"),
            workspace_root: keke_paths::AbsPath::new(std::env::current_dir().unwrap()).unwrap(),
            timeout_millis: None,
            cancelled: Arc::new(|| false),
            permission_reviewer: None,
            approved_network_domains: Vec::new(),
            approved_local_network_ports: Vec::new(),
            approved_local_listener_ports: Vec::new(),
            approved_local_network_sockets: Vec::new(),
            approved_writable_roots: Vec::new(),
        };
        let error = tool
            .run(
                ctx,
                BashArgs {
                    command: "echo should-not-run".into(),
                    timeout_ms: None,
                    background: false,
                    allowed_domains: Vec::new(),
                    allowed_local_ports: Vec::new(),
                    exposed_local_ports: Vec::new(),
                    allowed_unix_sockets: vec!["/host/service.sock".into()],
                    additional_write_roots: Vec::new(),
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("explicit command approval"));
    }

    #[tokio::test]
    async fn write_roots_without_approval_cannot_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("spawned");
        let tool = Bash {
            sandbox: Arc::new(
                Sandbox::new(
                    keke_config_types::SandboxPolicy {
                        mode: keke_config_types::SandboxMode::DangerFullAccess,
                        ..Default::default()
                    },
                    None,
                )
                .unwrap(),
            ),
            background: None,
        };
        let ctx = ToolCallContext {
            call_id: keke_protocol::ToolCallId::new("unapproved-write"),
            workspace_root: keke_paths::AbsPath::new(dir.path()).unwrap(),
            timeout_millis: None,
            cancelled: Arc::new(|| false),
            permission_reviewer: None,
            approved_network_domains: Vec::new(),
            approved_local_network_ports: Vec::new(),
            approved_local_listener_ports: Vec::new(),
            approved_local_network_sockets: Vec::new(),
            approved_writable_roots: Vec::new(),
        };
        let error = tool
            .run(
                ctx,
                BashArgs {
                    command: format!("touch '{}'", marker.display()),
                    timeout_ms: None,
                    background: false,
                    allowed_domains: Vec::new(),
                    allowed_local_ports: Vec::new(),
                    exposed_local_ports: Vec::new(),
                    allowed_unix_sockets: Vec::new(),
                    additional_write_roots: vec![dir.path().to_str().unwrap().into()],
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("explicit command approval"));
        assert!(!marker.exists());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn local_ports_without_dispatch_approval_cannot_start_a_command() {
        let sandbox = Sandbox::new(Default::default(), None).unwrap();
        let tool = Bash {
            sandbox: Arc::new(sandbox),
            background: None,
        };
        let ctx = ToolCallContext {
            call_id: keke_protocol::ToolCallId::new("unapproved-local"),
            workspace_root: keke_paths::AbsPath::new(std::env::current_dir().unwrap()).unwrap(),
            timeout_millis: None,
            cancelled: Arc::new(|| false),
            permission_reviewer: None,
            approved_network_domains: Vec::new(),
            approved_local_network_ports: Vec::new(),
            approved_local_listener_ports: Vec::new(),
            approved_local_network_sockets: Vec::new(),
            approved_writable_roots: Vec::new(),
        };
        let error = tool
            .run(
                ctx,
                BashArgs {
                    command: "echo should-not-run".into(),
                    timeout_ms: None,
                    background: false,
                    allowed_domains: Vec::new(),
                    allowed_local_ports: vec![3000],
                    exposed_local_ports: Vec::new(),
                    allowed_unix_sockets: Vec::new(),
                    additional_write_roots: Vec::new(),
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("explicit command approval"));
    }

    #[tokio::test]
    async fn proxy_bridge_works_on_a_single_thread_runtime() {
        struct Reviewer;
        impl keke_tool::PermissionReviewer for Reviewer {
            fn permission(
                &self,
                domain: String,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<(), ToolError>> + Send + '_>,
            > {
                Box::pin(async move {
                    if domain == "example.com" {
                        Ok(())
                    } else {
                        Err(ToolError::denied("not granted"))
                    }
                })
            }
        }
        let stopped = Arc::new(AtomicBool::new(false));
        let authorizer = Arc::new(NetworkReview {
            reviewer: Arc::new(Reviewer),
            runtime: tokio::runtime::Handle::current(),
            stopped: Arc::clone(&stopped),
            cancelled: Arc::new(|| false),
            refused: Arc::new(std::sync::Mutex::new(Vec::new())),
        });
        let bridge = authorizer.clone();
        assert!(
            tokio::task::spawn_blocking(move || keke_sandbox::NetworkAuthorizer::authorize(
                &*bridge,
                "example.com"
            ))
            .await
            .unwrap()
        );
        stopped.store(true, Ordering::Release);
        assert!(
            !tokio::task::spawn_blocking(move || keke_sandbox::NetworkAuthorizer::authorize(
                &*authorizer,
                "example.com"
            ))
            .await
            .unwrap()
        );
    }
}

#[cfg(unix)]
#[cfg(test)]
mod process_tree_tests {
    use super::*;

    fn context(dir: &tempfile::TempDir, timeout_millis: Option<u64>) -> ToolCallContext {
        ToolCallContext {
            call_id: keke_protocol::ToolCallId::new("tree"),
            workspace_root: keke_paths::AbsPath::new(dir.path()).unwrap(),
            timeout_millis,
            cancelled: Arc::new(|| false),
            permission_reviewer: None,
            approved_network_domains: Vec::new(),
            approved_local_network_ports: Vec::new(),
            approved_local_listener_ports: Vec::new(),
            approved_local_network_sockets: Vec::new(),
            approved_writable_roots: Vec::new(),
        }
    }

    /// A zombie is dead for this purpose: PID 1 in a container may never reap
    /// the orphans a killed group leaves behind.
    fn running(pid: u32) -> bool {
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            let state = stat
                .rsplit(')')
                .next()
                .and_then(|rest| rest.trim().chars().next());
            return state != Some('Z');
        }
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    /// Overrunning the budget used to kill the shell and leave whatever it had
    /// started — a dev server, say — running after the call reported failure.
    #[tokio::test]
    async fn a_timed_out_command_leaves_nothing_it_started_running() {
        let dir = tempfile::tempdir().unwrap();
        let tool = Bash {
            sandbox: Arc::new(
                Sandbox::new(
                    keke_config_types::SandboxPolicy {
                        mode: SandboxMode::DangerFullAccess,
                        ..Default::default()
                    },
                    None,
                )
                .unwrap(),
            ),
            background: None,
        };
        let args: BashArgs = serde_json::from_value(serde_json::json!({
            "command": "sleep 60 & echo $! > child.pid; wait",
        }))
        .unwrap();
        let error = tool.run(context(&dir, Some(700)), args).await.unwrap_err();
        assert!(matches!(error, ToolError::Timeout { .. }), "{error}");

        let child: u32 = std::fs::read_to_string(dir.path().join("child.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        for _ in 0..250 {
            if !running(child) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the background child outlived the timed-out call");
    }
}
