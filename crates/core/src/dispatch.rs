//! Tool dispatch.
//!
//! The order of checks is the policy, and it is deliberate:
//!
//! 1. The tool must exist.
//! 2. Lifecycle contributors observe the call (they cannot block it).
//! 3. Guards run and may deny — and only deny.
//! 4. The tool body runs, under the budget it advertised.
//!
//! Putting guards after the observers, and giving them no way to allow, is what
//! makes denial monotonic: no registration order can turn a denial back into
//! permission.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use keke_config_types::ApprovalPolicy;
use keke_paths::AbsPath;
use keke_plugin_api::ApprovalDecision;
use keke_plugin_api::ApprovalRequest;
use keke_plugin_api::ExtensionContext;
use keke_plugin_api::ExtensionRegistry;
use keke_protocol::ContentBlock;
use keke_protocol::ToolCall;
use keke_protocol::ToolResult;
use keke_protocol::ToolStatus;
use keke_tool::ApprovalRequirement;
use keke_tool::ArcTool;
use keke_tool::ToolCallContext;
use keke_tool::ToolError;

use crate::approval::ApprovalMemory;
use crate::approval::approval_reason;

/// Human network grants shared by every call in a session. The mutex also
/// serializes network review so concurrent calls cannot ask for the same grant.
#[derive(Default)]
pub struct SessionNetworkPermissions {
    pub allowed: tokio::sync::Mutex<BTreeSet<String>>,
}

/// The tools available for a session, keyed by the name the model sees.
#[derive(Clone, Default)]
pub struct ToolSet {
    tools: BTreeMap<String, ArcTool>,
}

impl ToolSet {
    /// Collect every contributed tool.
    ///
    /// A later contributor shadowing an earlier one's id is allowed and
    /// intentional: it is how a plugin replaces a built-in implementation.
    pub fn from_registry(registry: &ExtensionRegistry, ctx: &ExtensionContext) -> Self {
        let mut tools = BTreeMap::new();
        for contributor in registry.tool_contributors() {
            for tool in contributor.tools(ctx) {
                tools.insert(tool.id().to_string(), tool);
            }
        }
        Self { tools }
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ArcTool> {
        self.tools.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &ArcTool> {
        self.tools.values()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// Everything one dispatch needs that is not the call itself.
///
/// A struct rather than eight positional arguments: the two `&`-borrows that
/// matter for correctness — the registry and the approval memory — are named at
/// the call site instead of hiding in the middle of a parameter list.
pub struct Dispatch<'a> {
    pub tools: &'a ToolSet,
    pub registry: &'a ExtensionRegistry,
    pub ext_ctx: &'a ExtensionContext,
    pub workspace_root: &'a AbsPath,
    pub cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
    pub policy: ApprovalPolicy,
    /// Standing permissions this session has already been given.
    pub memory: &'a ApprovalMemory,
    /// What the reviewer should see beside the call, when the caller has it.
    pub evidence: Option<String>,
}

/// What a dispatch produced.
pub struct Dispatched {
    pub result: ToolResult,
    /// The reviewer asked for the turn to stop, not just for this call to be
    /// refused. Distinct from a plain denial because a denial goes back to the
    /// model to work around, and an abort must not.
    pub abort: bool,
}

/// Run one tool call through the full policy pipeline.
pub async fn dispatch(call: &ToolCall, ctx: Dispatch<'_>) -> Dispatched {
    let Dispatch {
        tools,
        registry,
        ext_ctx,
        workspace_root,
        cancelled,
        policy,
        memory,
        evidence,
    } = ctx;

    let Some(tool) = tools.get(&call.name) else {
        // An unknown tool is the model's mistake and the model can correct it,
        // so this is an error result rather than an aborted turn.
        return Dispatched {
            result: ToolResult::error(
                call.id.clone(),
                format!(
                    "unknown tool `{}`; available tools: {}",
                    call.name,
                    tools
                        .iter()
                        .map(|tool| tool.id().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ),
            abort: false,
        };
    };

    for contributor in registry.tool_lifecycle_contributors() {
        contributor.on_tool_start(ext_ctx, call).await;
    }

    // Only the runtime proxy may mark a request as an observed destination.
    // Letting model arguments supply this marker could persist invented grants.
    if call.arguments.get("requested_network_domain").is_some() {
        return refuse(
            call,
            registry,
            ext_ctx,
            "requested_network_domain is reserved for runtime network approval".into(),
            false,
        )
        .await;
    }
    if let Some(reason) = registry.first_denial(call) {
        return refuse(call, registry, ext_ctx, reason, false).await;
    }

    // After the guards, so a guard's denial is already final and no answer here
    // can undo it, and before the body, so nothing runs unapproved.
    let mut approval_note = None;
    let mut capabilities = tool.capabilities();
    capabilities.approval = tool.call_approval(workspace_root, &call.arguments);
    let domains = tool.network_domains(&call.arguments);
    let ports = tool.local_network_ports(&call.arguments);
    let listener_ports = tool.local_listener_ports(&call.arguments);
    let sockets = tool.local_network_sockets(&call.arguments);
    let roots = tool.writable_roots(&call.arguments);
    let mut approved_writable_roots = Vec::new();
    let mut approved_local_network_sockets = Vec::new();
    let mut approved_local_network_ports = Vec::new();
    let mut approved_local_listener_ports = Vec::new();
    let mut approved_network_domains = Vec::new();
    let human = !matches!(policy, ApprovalPolicy::Auto | ApprovalPolicy::Never);
    let session_permissions = ext_ctx.get_or_insert_with(SessionNetworkPermissions::default);
    let mut session_allowed = if human && !domains.is_empty() {
        Some(session_permissions.allowed.lock().await)
    } else {
        None
    };
    let needs_network_review = !domains.is_empty()
        && !session_allowed.as_ref().is_some_and(|allowed| {
            domains.iter().all(|d| {
                allowed.contains(d)
                    || registry
                        .network_permission_persistence()
                        .is_some_and(|store| store.contains(d))
            })
        });
    if !needs_network_review {
        approved_network_domains = domains.clone();
    }
    if (needs_network_review
        || !ports.is_empty()
        || !listener_ports.is_empty()
        || !sockets.is_empty()
        || !roots.is_empty())
        && capabilities.approval != ApprovalRequirement::Always
    {
        capabilities.approval = ApprovalRequirement::ReviewRequired;
    }
    // A tool that exists to be decided is asked about every time. Remembering
    // "allow always" for one would answer every later call on a person's
    // behalf, which is the thing it said must not happen — a plan approved
    // once would then approve every plan after it, unasked.
    let standing = matches!(
        capabilities.approval,
        ApprovalRequirement::ByPolicy | ApprovalRequirement::Confined
    ) && memory.is_always_allowed(&call.name);
    if let Some(reason) = approval_reason(policy, &capabilities)
        && !standing
    {
        let request = ApprovalRequest {
            call: call.clone(),
            reason: if !needs_network_review
                && ports.is_empty()
                && listener_ports.is_empty()
                && sockets.is_empty()
                && roots.is_empty()
            {
                reason
            } else {
                let mut access = Vec::new();
                if !domains.is_empty() {
                    access.push(format!(
                        "HTTP download/HTTPS domains: {}",
                        domains.join(", ")
                    ));
                }
                if !ports.is_empty() {
                    access.push(format!(
                        "localhost TCP ports: {} (access to services running on the host; this command only)",
                        ports.iter().map(u16::to_string).collect::<Vec<_>>().join(", ")
                    ));
                }
                if !listener_ports.is_empty() {
                    access.push(format!(
                        "localhost server ports exposed to the host: {} (this command only)",
                        listener_ports
                            .iter()
                            .map(u16::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                if !sockets.is_empty() {
                    access.push(format!("Unix sockets: {} (host service access, potentially privileged; this command only)", sockets.join(", ")));
                }
                if !roots.is_empty() {
                    access.push(format!("additional writable directories: {} (outside-workspace filesystem access; this command only)", roots.join(", ")));
                }
                format!(
                    "{}; requested network access — {}",
                    reason,
                    access.join("; ")
                )
            },
            evidence,
        };
        match review(registry, ext_ctx, &request, policy, capabilities.approval).await {
            ApprovalDecision::Allow { note } => {
                approval_note = note;
                approved_network_domains = domains.clone();
                approved_local_network_ports = ports.clone();
                approved_local_listener_ports = listener_ports.clone();
                approved_local_network_sockets = sockets.clone();
                approved_writable_roots = roots.clone();
            }
            ApprovalDecision::AllowAlways => {
                if ports.is_empty()
                    && listener_ports.is_empty()
                    && sockets.is_empty()
                    && roots.is_empty()
                {
                    memory.allow_always(&call.name);
                }
                approved_network_domains = domains.clone();
                approved_local_network_ports = ports.clone();
                approved_local_listener_ports = listener_ports.clone();
                approved_local_network_sockets = sockets.clone();
                approved_writable_roots = roots.clone();
            }
            ApprovalDecision::Deny { reason } => {
                return refuse(call, registry, ext_ctx, reason, false).await;
            }
            ApprovalDecision::Abort { reason } => {
                return refuse(call, registry, ext_ctx, reason, true).await;
            }
        }
    } else if policy == ApprovalPolicy::Never {
        // Skipping review authorizes the declared rights for this call only.
        approved_network_domains = domains.clone();
        approved_local_network_ports = ports.clone();
        approved_local_listener_ports = listener_ports.clone();
        approved_local_network_sockets = sockets.clone();
        approved_writable_roots = roots.clone();
    }
    if let Some(allowed) = session_allowed.as_mut() {
        allowed.extend(approved_network_domains.iter().cloned());
    }
    drop(session_allowed);

    // The advertised budget is enforced here rather than trusted to the tool.
    // A tool keeping its own copy of the number would let the two drift, and a
    // tool that simply ignored it would overrun with nothing to stop it.
    let budget = tool.capabilities().timeout_millis;
    let permission_reviewer = Arc::new(RuntimePermissionReviewer {
        registry: registry.clone(),
        ext_ctx: ext_ctx.clone(),
        call: call.clone(),
        policy,
        cancelled: cancelled.clone(),
        closed: std::sync::atomic::AtomicBool::new(false),
        allowed: tokio::sync::Mutex::new(approved_network_domains.clone()),
    });
    let ctx = ToolCallContext {
        call_id: call.id.clone(),
        workspace_root: workspace_root.clone(),
        timeout_millis: budget,
        cancelled,
        permission_reviewer: Some(permission_reviewer.clone()),
        approved_network_domains,
        approved_local_network_ports,
        approved_local_listener_ports,
        approved_local_network_sockets,
        approved_writable_roots,
    };

    let running = tool.call(ctx, call.arguments.clone());
    let outcome = match budget {
        Some(millis) => {
            match tokio::time::timeout(Duration::from_millis(millis), running).await {
                Ok(outcome) => outcome,
                // Dropping the future drops whatever the tool was holding; tools
                // that spawn children set `kill_on_drop`, so this really stops.
                Err(_) => Err(ToolError::Timeout { millis }),
            }
        }
        None => running.await,
    };
    // Background commands retain their scoped reviewer until their child
    // exits. Failed calls cannot retain permission to ask for further access.
    if outcome.is_err() {
        permission_reviewer
            .closed
            .store(true, std::sync::atomic::Ordering::Release);
    }
    let result = match &outcome {
        Ok(output) => {
            let mut content = output.model_output.clone();
            // Appended to what the tool said rather than replacing it: the
            // person's words are about the call, not the call's answer, and a
            // model that lost the tool's own output would be reading a
            // comment on something it can no longer see.
            if let Some(note) = &approval_note {
                content.push(ContentBlock::text(format!(
                    "The person approved this and added:\n\n{note}"
                )));
            }
            ToolResult {
                id: call.id.clone(),
                status: ToolStatus::Ok,
                content,
                value: Some(output.value.clone()),
            }
        }
        Err(error) => ToolResult {
            id: call.id.clone(),
            status: error.status(),
            content: vec![ContentBlock::text(error.to_string())],
            value: None,
        },
    };

    let observed = outcome.as_ref().map(|_| ());
    for contributor in registry.tool_lifecycle_contributors() {
        contributor.on_tool_finish(ext_ctx, call, observed).await;
    }

    Dispatched {
        result,
        abort: false,
    }
}

struct RuntimePermissionReviewer {
    registry: ExtensionRegistry,
    ext_ctx: ExtensionContext,
    call: ToolCall,
    policy: ApprovalPolicy,
    cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
    closed: std::sync::atomic::AtomicBool,
    allowed: tokio::sync::Mutex<Vec<String>>,
}

impl keke_tool::PermissionReviewer for RuntimePermissionReviewer {
    fn permission(
        &self,
        domain: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ToolError>> + Send + '_>>
    {
        Box::pin(async move {
            let cancellation = async {
                loop {
                    if (self.cancelled)() || self.closed.load(std::sync::atomic::Ordering::Acquire)
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            };
            let request = async {
                let mut allowed = self.allowed.lock().await;
                if (self.cancelled)() || self.closed.load(std::sync::atomic::Ordering::Acquire) {
                    return Err(ToolError::Cancelled);
                }
                if allowed.contains(&domain) {
                    return Ok(());
                }
                if self.policy == ApprovalPolicy::Never {
                    // Never grants stay scoped to this command, not human consent stores.
                    allowed.push(domain);
                    return Ok(());
                }
                if self.policy == ApprovalPolicy::Auto {
                    return Err(ToolError::denied(format!(
                        "network domain {domain} was not approved; add it to allowed_domains and rerun"
                    )));
                }
                let permissions = self
                    .ext_ctx
                    .get_or_insert_with(SessionNetworkPermissions::default);
                let mut session_allowed = permissions.allowed.lock().await;
                if (self.cancelled)() || self.closed.load(std::sync::atomic::Ordering::Acquire) {
                    return Err(ToolError::Cancelled);
                }
                if session_allowed.contains(&domain) {
                    return Ok(());
                }
                if self
                    .registry
                    .network_permission_persistence()
                    .is_some_and(|store| store.contains(&domain))
                {
                    session_allowed.insert(domain);
                    return Ok(());
                }
                // Only the review copy changes: execution retains its original arguments.
                let mut call = self.call.clone();
                let Some(arguments) = call.arguments.as_object_mut() else {
                    return Err(ToolError::denied(
                        "network review requires object arguments",
                    ));
                };
                arguments.insert("requested_network_domain".into(), domain.clone().into());
                let request = ApprovalRequest {
                    call,
                    reason: format!(
                        "Command {} {} requests network domain {domain}",
                        self.call.name, self.call.arguments
                    ),
                    evidence: None,
                };
                match review(
                    &self.registry,
                    &self.ext_ctx,
                    &request,
                    self.policy,
                    ApprovalRequirement::ReviewRequired,
                )
                .await
                {
                    decision @ (ApprovalDecision::Allow { .. } | ApprovalDecision::AllowAlways) => {
                        if (self.cancelled)()
                            || self.closed.load(std::sync::atomic::Ordering::Acquire)
                        {
                            return Err(ToolError::Cancelled);
                        }
                        if decision == ApprovalDecision::AllowAlways {
                            let store = self.registry.network_permission_persistence().ok_or_else(
                                || {
                                    ToolError::denied(
                                        "persistent domain permission storage is unavailable",
                                    )
                                },
                            )?;
                            store.remember(&domain).map_err(ToolError::denied)?;
                        }
                        session_allowed.insert(domain.clone());
                        allowed.push(domain);
                        Ok(())
                    }
                    ApprovalDecision::Deny { reason } | ApprovalDecision::Abort { reason } => {
                        Err(ToolError::denied(reason))
                    }
                }
            };
            tokio::select! {
                biased;
                () = cancellation => Err(ToolError::Cancelled),
                result = request => result,
            }
        })
    }
}

/// Ask the reviewers, in registration order; the first answer wins.
///
/// Nobody answering means nobody is there to ask — a non-interactive surface,
/// or a session with no approval extension installed. That is a denial, not a
/// silent allow: a harness that runs unreviewed commands because its prompt had
/// no listener is exactly the failure this pipeline exists to prevent.
async fn review(
    registry: &ExtensionRegistry,
    ext_ctx: &ExtensionContext,
    request: &ApprovalRequest,
    policy: ApprovalPolicy,
    requirement: ApprovalRequirement,
) -> ApprovalDecision {
    for contributor in registry.approval_contributors() {
        let eligible = if requirement == ApprovalRequirement::Always {
            !contributor.automatic()
        } else if policy == ApprovalPolicy::Auto {
            contributor.automatic()
        } else if requirement == ApprovalRequirement::ReviewRequired {
            !contributor.automatic()
        } else {
            contributor.on_request()
        };
        if !eligible {
            continue;
        }
        if let Some(decision) = contributor.review(ext_ctx, request).await {
            return decision;
        }
    }
    ApprovalDecision::Deny {
        reason: format!(
            "{} needs approval and there is nobody to ask",
            request.call.name
        ),
    }
}

/// Refuse a call, telling the lifecycle observers the same story the model gets.
async fn refuse(
    call: &ToolCall,
    registry: &ExtensionRegistry,
    ext_ctx: &ExtensionContext,
    reason: String,
    abort: bool,
) -> Dispatched {
    let result = ToolResult {
        id: call.id.clone(),
        status: ToolStatus::Denied,
        content: vec![ContentBlock::text(format!("Denied: {reason}"))],
        value: None,
    };
    let error = ToolError::denied(reason);
    for contributor in registry.tool_lifecycle_contributors() {
        contributor.on_tool_finish(ext_ctx, call, Err(&error)).await;
    }
    Dispatched { result, abort }
}

#[cfg(test)]
mod permission_tests {
    use super::*;
    use keke_plugin_api::{ApprovalReviewContributor, ExtFuture, ExtensionRegistryBuilder};
    use keke_tool::{PermissionReviewer, Tool, ToolDescription, ToolId, ToolOutput};

    struct Reviewer(bool, bool);
    impl ApprovalReviewContributor for Reviewer {
        fn automatic(&self) -> bool {
            self.1
        }
        fn review<'a>(
            &'a self,
            _: &'a ExtensionContext,
            request: &'a ApprovalRequest,
        ) -> ExtFuture<'a, Option<ApprovalDecision>> {
            Box::pin(async move {
                assert!(request.reason.contains("example.com"));
                Some(if self.0 {
                    ApprovalDecision::Allow { note: None }
                } else {
                    ApprovalDecision::Deny {
                        reason: "no".into(),
                    }
                })
            })
        }
    }

    #[derive(serde::Serialize)]
    struct Output;
    impl ToolOutput for Output {
        fn render(&self) -> Vec<ContentBlock> {
            Vec::new()
        }
    }
    struct NetworkTool;
    impl Tool for NetworkTool {
        type Args = serde_json::Value;
        type Output = Output;
        fn id(&self) -> ToolId {
            ToolId::new("network")
        }
        fn description(&self, _: &keke_tool::ListToolsContext) -> ToolDescription {
            unreachable!()
        }
        fn capabilities(&self) -> keke_tool::ToolCapabilities {
            keke_tool::ToolCapabilities {
                approval: ApprovalRequirement::AutoApproved,
                ..Default::default()
            }
        }
        fn network_domains(&self, _: &Self::Args) -> Vec<String> {
            vec!["example.com".into()]
        }
        fn local_network_ports(&self, args: &Self::Args) -> Vec<u16> {
            if args.get("ports").is_some() {
                vec![8080]
            } else {
                Vec::new()
            }
        }
        fn local_listener_ports(&self, args: &Self::Args) -> Vec<u16> {
            if args.get("listeners").is_some() {
                vec![3000]
            } else {
                Vec::new()
            }
        }
        fn local_network_sockets(&self, args: &Self::Args) -> Vec<String> {
            if args.get("sockets").is_some() {
                vec!["/tmp/test-host.sock".into()]
            } else {
                Vec::new()
            }
        }
        fn writable_roots(&self, args: &Self::Args) -> Vec<String> {
            if args.get("roots").is_some() {
                vec!["/tmp/test-cache".into()]
            } else {
                Vec::new()
            }
        }
        async fn run(&self, ctx: ToolCallContext, args: Self::Args) -> Result<Output, ToolError> {
            assert_eq!(ctx.approved_network_domains, vec!["example.com"]);
            assert_eq!(
                ctx.approved_local_listener_ports,
                self.local_listener_ports(&args)
            );
            assert_eq!(
                ctx.approved_local_network_ports,
                self.local_network_ports(&args)
            );
            assert_eq!(
                ctx.approved_local_network_sockets,
                self.local_network_sockets(&args)
            );
            assert_eq!(ctx.approved_writable_roots, self.writable_roots(&args));
            Ok(Output)
        }
    }

    fn context() -> ExtensionContext {
        ExtensionContext::new(
            keke_protocol::SessionId::new(),
            keke_protocol::ThreadId::new(),
        )
    }
    fn call() -> ToolCall {
        ToolCall {
            id: keke_protocol::ToolCallId::new("c"),
            name: "network".into(),
            arguments: serde_json::json!({"command": "curl example.com"}),
        }
    }

    struct UnexpectedReviewer;
    impl ApprovalReviewContributor for UnexpectedReviewer {
        fn automatic(&self) -> bool {
            true
        }
        fn review<'a>(
            &'a self,
            _: &'a ExtensionContext,
            _: &'a ApprovalRequest,
        ) -> ExtFuture<'a, Option<ApprovalDecision>> {
            panic!("runtime Auto must not consult the guardian")
        }
    }

    fn runtime(
        registry: ExtensionRegistry,
        ext_ctx: ExtensionContext,
        policy: ApprovalPolicy,
    ) -> RuntimePermissionReviewer {
        RuntimePermissionReviewer {
            registry,
            ext_ctx,
            call: call(),
            policy,
            cancelled: Arc::new(|| false),
            closed: std::sync::atomic::AtomicBool::new(false),
            allowed: tokio::sync::Mutex::new(Vec::new()),
        }
    }

    #[tokio::test]
    async fn human_grants_cross_calls_but_auto_and_never_ignore_them() {
        let ctx = context();
        let mut builder = ExtensionRegistryBuilder::new();
        builder.approval_review_contributor(Arc::new(Reviewer(true, false)));
        let first = runtime(builder.build(), ctx.clone(), ApprovalPolicy::OnRequest);
        first.permission("example.com".into()).await.unwrap();
        let second = runtime(
            ExtensionRegistryBuilder::new().build(),
            ctx.clone(),
            ApprovalPolicy::AcceptEdits,
        );
        second.permission("example.com".into()).await.unwrap();
        for policy in [ApprovalPolicy::Auto, ApprovalPolicy::Never] {
            let mut builder = ExtensionRegistryBuilder::new();
            builder.approval_review_contributor(Arc::new(UnexpectedReviewer));
            builder.approval_review_contributor(Arc::new(Reviewer(true, false)));
            let reviewer = runtime(builder.build(), ctx.clone(), policy);
            if policy == ApprovalPolicy::Never {
                reviewer.permission("new.example.com".into()).await.unwrap();
                assert_eq!(*reviewer.allowed.lock().await, vec!["new.example.com"]);
            } else {
                let error = reviewer.permission("example.com".into()).await.unwrap_err();
                assert!(error.to_string().contains("example.com"));
                assert!(error.to_string().contains("allowed_domains"));
            }
            reviewer.allowed.lock().await.push("declared.com".into());
            reviewer.permission("declared.com".into()).await.unwrap();
        }
    }

    #[tokio::test]
    async fn declared_domains_are_reviewed_in_auto_and_granted_without_review_in_never() {
        for policy in [ApprovalPolicy::Auto, ApprovalPolicy::Never] {
            for decision in [None, Some(false), Some(true)] {
                let mut builder = ExtensionRegistryBuilder::new();
                if let Some(allow) = decision {
                    builder.approval_review_contributor(Arc::new(Reviewer(
                        allow,
                        policy == ApprovalPolicy::Auto,
                    )));
                }
                let registry = builder.build();
                let mut tools = ToolSet::default();
                tools.tools.insert("network".into(), Arc::new(NetworkTool));
                let result = dispatch(
                    &call(),
                    Dispatch {
                        tools: &tools,
                        registry: &registry,
                        ext_ctx: &context(),
                        workspace_root: &AbsPath::new(std::env::current_dir().unwrap()).unwrap(),
                        cancelled: Arc::new(|| false),
                        policy,
                        memory: &ApprovalMemory::default(),
                        evidence: None,
                    },
                )
                .await;
                assert_eq!(
                    result.result.status,
                    if policy == ApprovalPolicy::Never || decision == Some(true) {
                        ToolStatus::Ok
                    } else {
                        ToolStatus::Denied
                    }
                );
            }
        }
    }

    #[tokio::test]
    async fn local_ports_require_fresh_command_review_and_never_caches_them() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct PortReviewer {
            calls: Arc<AtomicUsize>,
            automatic: bool,
            allow: bool,
        }
        impl ApprovalReviewContributor for PortReviewer {
            fn automatic(&self) -> bool {
                self.automatic
            }
            fn review<'a>(
                &'a self,
                _: &'a ExtensionContext,
                request: &'a ApprovalRequest,
            ) -> ExtFuture<'a, Option<ApprovalDecision>> {
                Box::pin(async move {
                    self.calls.fetch_add(1, Ordering::SeqCst);
                    assert!(request.reason.contains("HTTPS domains: example.com"));
                    assert!(request.reason.contains("localhost TCP ports: 8080"));
                    assert!(request.reason.contains("services running on the host"));
                    assert!(
                        request
                            .reason
                            .contains("localhost server ports exposed to the host: 3000")
                    );
                    assert!(request.reason.contains("Unix sockets: /tmp/test-host.sock"));
                    assert!(
                        request
                            .reason
                            .contains("additional writable directories: /tmp/test-cache")
                    );
                    Some(if self.allow {
                        ApprovalDecision::AllowAlways
                    } else {
                        ApprovalDecision::Deny {
                            reason: "no ports".into(),
                        }
                    })
                })
            }
        }

        for policy in [
            ApprovalPolicy::Auto,
            ApprovalPolicy::Never,
            ApprovalPolicy::OnRequest,
        ] {
            for allow in [false, true] {
                let calls = Arc::new(AtomicUsize::new(0));
                let mut builder = ExtensionRegistryBuilder::new();
                builder.approval_review_contributor(Arc::new(PortReviewer {
                    calls: calls.clone(),
                    automatic: policy == ApprovalPolicy::Auto,
                    allow,
                }));
                let registry = builder.build();
                let ext_ctx = context();
                let memory = ApprovalMemory::default();
                memory.allow_always("network");
                let session = ext_ctx.get_or_insert_with(SessionNetworkPermissions::default);
                session.allowed.lock().await.insert("example.com".into());
                let mut tools = ToolSet::default();
                tools.tools.insert("network".into(), Arc::new(NetworkTool));
                let mut command = call();
                command.arguments["ports"] = serde_json::json!([8080]);
                command.arguments["listeners"] = serde_json::json!([3000]);
                command.arguments["sockets"] = serde_json::json!(["/tmp/test-host.sock"]);
                command.arguments["roots"] = serde_json::json!(["/tmp/test-cache"]);
                for _ in 0..2 {
                    let result = dispatch(
                        &command,
                        Dispatch {
                            tools: &tools,
                            registry: &registry,
                            ext_ctx: &ext_ctx,
                            workspace_root: &AbsPath::new(std::env::current_dir().unwrap())
                                .unwrap(),
                            cancelled: Arc::new(|| false),
                            policy,
                            memory: &memory,
                            evidence: None,
                        },
                    )
                    .await;
                    assert_eq!(
                        result.result.status,
                        if allow || policy == ApprovalPolicy::Never {
                            ToolStatus::Ok
                        } else {
                            ToolStatus::Denied
                        }
                    );
                }
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    if policy == ApprovalPolicy::Never {
                        0
                    } else {
                        2
                    }
                );
                assert_eq!(
                    *session.allowed.lock().await,
                    BTreeSet::from(["example.com".into()])
                );
            }
        }
    }

    #[tokio::test]
    async fn never_runtime_grants_are_call_scoped_and_cancellation_still_fails() {
        struct UnexpectedStore;
        impl keke_plugin_api::NetworkPermissionPersistence for UnexpectedStore {
            fn contains(&self, _: &str) -> bool {
                panic!("Never must not consume human grants")
            }
            fn remember(&self, _: &str) -> Result<(), String> {
                panic!("Never must not persist grants")
            }
        }
        let mut builder = ExtensionRegistryBuilder::new();
        builder.network_permission_persistence(Arc::new(UnexpectedStore));
        let ctx = context();
        let mut reviewer = runtime(builder.build(), ctx.clone(), ApprovalPolicy::Never);
        reviewer.permission("new.example.com".into()).await.unwrap();
        assert!(
            ctx.get_or_insert_with(SessionNetworkPermissions::default)
                .allowed
                .lock()
                .await
                .is_empty()
        );
        reviewer.cancelled = Arc::new(|| true);
        for domain in ["new.example.com", "another.example.com"] {
            assert!(matches!(
                reviewer.permission(domain.into()).await,
                Err(ToolError::Cancelled)
            ));
        }
        reviewer.cancelled = Arc::new(|| false);
        reviewer
            .closed
            .store(true, std::sync::atomic::Ordering::Release);
        assert!(matches!(
            reviewer.permission("another.example.com".into()).await,
            Err(ToolError::Cancelled)
        ));
    }

    #[tokio::test]
    async fn runtime_permissions_fail_closed_and_cache_human_allows() {
        for decision in [None, Some(false), Some(true)] {
            let mut builder = ExtensionRegistryBuilder::new();
            if let Some(allow) = decision {
                builder.approval_review_contributor(Arc::new(Reviewer(allow, false)));
            }
            let reviewer = RuntimePermissionReviewer {
                registry: builder.build(),
                ext_ctx: context(),
                call: call(),
                policy: ApprovalPolicy::OnRequest,
                cancelled: Arc::new(|| false),
                closed: std::sync::atomic::AtomicBool::new(false),
                allowed: tokio::sync::Mutex::new(Vec::new()),
            };
            assert_eq!(
                reviewer.permission("example.com".into()).await.is_ok(),
                decision == Some(true)
            );
            assert_eq!(
                reviewer.allowed.lock().await.len(),
                usize::from(decision == Some(true))
            );
            assert!(
                reviewer
                    .call
                    .arguments
                    .get("requested_network_domain")
                    .is_none()
            );
            reviewer
                .closed
                .store(true, std::sync::atomic::Ordering::Release);
            assert!(matches!(
                reviewer.permission("example.com".into()).await,
                Err(ToolError::Cancelled)
            ));
        }
    }

    #[tokio::test]
    async fn permanent_human_grants_never_authorize_auto_runtime_access() {
        struct Store;
        impl keke_plugin_api::NetworkPermissionPersistence for Store {
            fn contains(&self, domain: &str) -> bool {
                domain == "example.com"
            }
            fn remember(&self, _: &str) -> Result<(), String> {
                Ok(())
            }
        }
        let mut builder = ExtensionRegistryBuilder::new();
        builder.network_permission_persistence(Arc::new(Store));
        let registry = builder.build();
        for policy in [
            ApprovalPolicy::OnRequest,
            ApprovalPolicy::Auto,
            ApprovalPolicy::Never,
        ] {
            let reviewer = RuntimePermissionReviewer {
                registry: registry.clone(),
                ext_ctx: context(),
                call: call(),
                policy,
                cancelled: Arc::new(|| false),
                closed: std::sync::atomic::AtomicBool::new(false),
                allowed: tokio::sync::Mutex::new(Vec::new()),
            };
            assert_eq!(
                reviewer.permission("example.com".into()).await.is_ok(),
                policy != ApprovalPolicy::Auto
            );
        }
    }
}
