//! Plan mode: what the coarse [`SessionModeSwitch`] *means*.
//!
//! `keke-core` carries one bit — is this session planning? — and nothing else.
//! Everything a person would recognise as plan mode is here: the lifecycle, the
//! reminders the model reads, the refusal of edits outside the plan file, and
//! the two tools that enter and leave. A deployment wanting a different
//! planning discipline replaces this crate and touches no engine code.
//!
//! ## One source of truth per level
//!
//! Two things track plan mode and they are not peers.
//!
//! [`SessionModeSwitch`] is authoritative for the coarse Default-vs-Plan bit.
//! A person's toggle writes it through the seam, and `/new` resets it; neither
//! goes anywhere near this crate.
//!
//! [`PlanModeTracker`] owns the fine state — whether the model has been *told*
//! yet, whether an exit is deferred, which reminder comes next. It reconciles
//! *to* the switch at a turn boundary ([`TurnLifecycleContributor`]), never the
//! other way round. When the tracker moves on its own — the model called
//! `enter_plan_mode`, a person approved `exit_plan_mode` — it writes the switch
//! so the coarse answer stays the one a surface can read.
//!
//! The consequence worth stating: a mid-turn toggle is not lost and is not
//! obeyed instantly. It sits in the switch and lands at the next boundary,
//! because changing the rules under a model that is already reasoning about
//! them is how a "read-only" turn ends up having written something.

mod ported;
mod tools;

pub use ported::grok_build::plan_mode::PlanModeState;
pub use ported::grok_build::plan_mode::PlanModeTracker;
pub use tools::EnterPlanMode;
pub use tools::ExitPlanMode;

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use keke_config_types::SessionMode;
use keke_core::SessionModeSwitch;
use keke_plugin_api::ApprovalDecision;
use keke_plugin_api::ApprovalRequest;
use keke_plugin_api::ContextContributor;
use keke_plugin_api::ContextFragment;
use keke_plugin_api::ExtFuture;
use keke_plugin_api::ExtensionContext;
use keke_plugin_api::ExtensionRegistryBuilder;
use keke_plugin_api::ToolContributor;
use keke_plugin_api::TurnLifecycleContributor;
use keke_protocol::StopReason;
use keke_protocol::ToolCall;
use keke_protocol::TurnId;
use keke_tool::ArcTool;

use ported::grok_build::plan_mode as text;

/// Where the plan-mode reminder sits in the assembled prompt: after the tool
/// guidance slot, because it constrains what that guidance just offered.
pub const ORDER_PLAN_MODE: i32 = 200;

/// The name of the plan file inside a session's directory.
const PLAN_FILE: &str = "plan.md";

/// The tools plan mode refuses.
///
/// `bash` is deliberately absent. Plan mode blocks the *edit* tools, not shell
/// redirection: a planning agent still has to run `cargo check`, `git log`, and
/// `rg` to write a plan worth approving, and a person who wanted a sandbox
/// asked for a sandbox. Grok made the same call, and the alternative — a plan
/// mode that cannot read the build — is a planning mode that cannot plan.
const BLOCKED_TOOLS: &[&str] = &["write_file"];

/// Where a session's plan file lives.
///
/// Resolved through the session directory rather than a path this crate makes
/// up, so the plan sits beside the rollout log that records the turn that wrote
/// it.
#[derive(Clone, Debug)]
pub enum PlanLocation {
    /// `<project-dir>/<session-id>/plan.md`. The project directory comes from
    /// [`keke_core::project_dir`]; the session id is only known once a session
    /// exists, so the full path resolves at the first turn.
    SessionDir(PathBuf),
    /// An exact path. For a caller that already knows one — a test, or a
    /// deployment placing the plan somewhere of its own.
    Fixed(PathBuf),
}

impl PlanLocation {
    /// Plans live under `project`, one directory per session.
    #[must_use]
    pub fn under_project(project: impl Into<PathBuf>) -> Self {
        Self::SessionDir(project.into())
    }

    #[must_use]
    pub fn fixed(path: impl Into<PathBuf>) -> Self {
        Self::Fixed(path.into())
    }

    fn resolve(&self, ctx: &ExtensionContext) -> PathBuf {
        match self {
            Self::SessionDir(project) => project.join(ctx.session.to_string()).join(PLAN_FILE),
            Self::Fixed(path) => path.clone(),
        }
    }
}

/// The shared plan-mode state: the tracker, the switch it reconciles to, and
/// the plan file everything else is measured against.
pub struct PlanMode {
    tracker: Mutex<PlanModeTracker>,
    mode: Arc<SessionModeSwitch>,
    location: PlanLocation,
    /// Set when the tracker re-entered plan mode having been there before in
    /// this session, so the next reminder is the short "returning to plan mode"
    /// one instead of rules the transcript already carries.
    reentry: std::sync::atomic::AtomicBool,
    /// Written once, the first time a contributor hands over an
    /// [`ExtensionContext`] naming the session. A guard and a tool have no
    /// context of their own, so they read what a turn boundary already settled.
    path: OnceLock<PathBuf>,
    /// Whether leaving plan mode is a question even where the approval policy
    /// would not ask one. See `require_plan_approval` in the configuration.
    require_exit_approval: bool,
}

impl PlanMode {
    #[must_use]
    pub fn new(mode: Arc<SessionModeSwitch>, location: PlanLocation) -> Self {
        Self::with_exit_approval(mode, location, false)
    }

    /// [`Self::new`], saying whether leaving plan mode must be asked about even
    /// under a policy that would not ask.
    #[must_use]
    pub fn with_exit_approval(
        mode: Arc<SessionModeSwitch>,
        location: PlanLocation,
        require_exit_approval: bool,
    ) -> Self {
        Self {
            tracker: Mutex::new(PlanModeTracker::new()),
            mode,
            location,
            reentry: std::sync::atomic::AtomicBool::new(false),
            path: OnceLock::new(),
            require_exit_approval,
        }
    }

    /// Whether `exit_plan_mode` asks whatever the policy says.
    #[must_use]
    pub fn requires_exit_approval(&self) -> bool {
        self.require_exit_approval
    }

    /// The plan file, once a session has named itself.
    #[must_use]
    pub fn plan_path(&self) -> Option<&Path> {
        self.path.get().map(PathBuf::as_path)
    }

    fn resolve_path(&self, ctx: &ExtensionContext) -> &Path {
        self.path.get_or_init(|| self.location.resolve(ctx))
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        self.tracker.lock().is_ok_and(|tracker| tracker.is_active())
    }

    #[must_use]
    pub fn state(&self) -> PlanModeState {
        self.tracker
            .lock()
            .map_or(PlanModeState::Inactive, |tracker| tracker.state())
    }

    /// Push the tracker's answer back into the coarse switch.
    ///
    /// Called only where the tracker moved by itself; reconciliation reads the
    /// switch instead, so the two can never chase each other.
    fn publish(&self, active: bool) {
        self.mode.set(if active {
            SessionMode::Plan
        } else {
            SessionMode::Default
        });
    }

    /// The model asked to plan, and a person approved the call.
    pub fn activate_from_tool(&self) -> bool {
        let Ok(mut tracker) = self.tracker.lock() else {
            return false;
        };
        let changed = tracker.activate_from_tool();
        drop(tracker);
        if changed {
            self.publish(true);
        }
        changed
    }

    /// A person approved the plan, so planning is over.
    pub fn deactivate_approved(&self) -> bool {
        let Ok(mut tracker) = self.tracker.lock() else {
            return false;
        };
        let changed = tracker.deactivate_approved();
        drop(tracker);
        if changed {
            self.publish(false);
        }
        changed
    }

    /// Bring the tracker in line with whatever the switch now says.
    ///
    /// Run at a turn boundary, which is the only moment both answers can be
    /// changed without one of them being read half-applied.
    fn reconcile(&self, turn_in_flight: bool) {
        let Ok(mut tracker) = self.tracker.lock() else {
            return;
        };
        match (self.mode.get(), tracker.state()) {
            (SessionMode::Plan, PlanModeState::Inactive | PlanModeState::ExitPending) => {
                tracker.enter_pending();
                // Read while still `Pending`, which is the only state that can
                // tell a return from a first entry.
                if tracker.is_reentry() {
                    self.reentry
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                // A turn is starting, so "pending" would be a state nothing
                // ever leaves: the prompt that would have activated it is this
                // one.
                tracker.activate();
            }
            (SessionMode::Plan, _) => {}
            (SessionMode::Default, PlanModeState::Pending | PlanModeState::Active) => {
                tracker.user_exit(turn_in_flight);
            }
            (SessionMode::Default, PlanModeState::ExitPending) => {
                tracker.complete_deferred_exit();
            }
            (SessionMode::Default, PlanModeState::Inactive) => {}
        }
    }

    /// Why this call is refused, if it is.
    ///
    /// Denial-only by construction (`AGENTS.md` invariant 7): the plan file
    /// itself returns `None` here so the reviewer below can *allow* it. A guard
    /// has no allow, and a call a guard denies is already final by the time
    /// anybody is asked.
    fn denial(&self, call: &ToolCall) -> Option<String> {
        if !self.is_active() || !BLOCKED_TOOLS.contains(&call.name.as_str()) {
            return None;
        }
        let plan = self.plan_path()?;
        if targets(call, plan) {
            return None;
        }
        Some(text::edit_rejected(&plan.display().to_string()))
    }
}

/// Whether a call writes the plan file.
///
/// Compared as the model was given it: the reminder hands over the absolute
/// path, so an exact match is what a model following the instruction produces.
/// A relative spelling of the same file is refused, which is a refusal to guess
/// rather than a bug — the message names the path to use.
fn targets(call: &ToolCall, plan: &Path) -> bool {
    call.arguments
        .get("path")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|path| Path::new(path) == plan)
}

/// Register plan mode.
///
/// `mode` must be the same switch the session is built with
/// (`SessionBuilder::mode_switch`) — two cells would let a surface say
/// "planning" while the guards had already stopped. `keke-cli` is the only
/// caller, and creates it once for both.
pub fn install(
    registry: &mut ExtensionRegistryBuilder,
    mode: Arc<SessionModeSwitch>,
    location: PlanLocation,
    require_exit_approval: bool,
) -> Arc<PlanMode> {
    let plan = Arc::new(PlanMode::with_exit_approval(
        mode,
        location,
        require_exit_approval,
    ));
    let extension = Arc::new(PlanExtension {
        plan: Arc::clone(&plan),
    });

    registry
        .turn_lifecycle_contributor(Arc::clone(&extension) as Arc<dyn TurnLifecycleContributor>);
    registry.context_contributor(Arc::clone(&extension) as Arc<dyn ContextContributor>);
    registry.tool_contributor(Arc::clone(&extension) as Arc<dyn ToolContributor>);
    // Before the surface's own reviewer, so a plan-file write is allowed
    // without a person being asked about the file they told the agent to write.
    registry.approval_review_contributor(extension);

    let guard = Arc::clone(&plan);
    registry.tool_guard(Box::new(move |call| guard.denial(call)));

    plan
}

/// One value behind every contributor, so they cannot disagree about the state.
struct PlanExtension {
    plan: Arc<PlanMode>,
}

impl TurnLifecycleContributor for PlanExtension {
    fn on_turn_start<'a>(&'a self, ctx: &'a ExtensionContext, _turn: TurnId) -> ExtFuture<'a, ()> {
        Box::pin(async move {
            self.plan.resolve_path(ctx);
            self.plan.reconcile(false);
        })
    }

    fn on_turn_end<'a>(
        &'a self,
        _ctx: &'a ExtensionContext,
        _turn: TurnId,
        _reason: &'a StopReason,
    ) -> ExtFuture<'a, ()> {
        Box::pin(async move {
            // A toggle that arrived mid-turn lands here rather than mid-step.
            self.plan.reconcile(false);
        })
    }
}

impl ContextContributor for PlanExtension {
    fn contribute_turn_context<'a>(
        &'a self,
        ctx: &'a ExtensionContext,
    ) -> ExtFuture<'a, Vec<ContextFragment>> {
        Box::pin(async move {
            let plan_path = self.plan.resolve_path(ctx).to_path_buf();
            let has_content = plan_file_has_content(&plan_path).await;
            let display = plan_path.display().to_string();

            let mut fragments = Vec::new();
            {
                let Ok(mut tracker) = self.plan.tracker.lock() else {
                    return fragments;
                };
                if tracker.has_pending_exit_reminder() {
                    tracker.clear_pending_exit_reminder();
                    fragments.push(ContextFragment::new(
                        "plan-mode/exit",
                        ORDER_PLAN_MODE,
                        text::exit_reminder(),
                    ));
                }
                if tracker.is_active() {
                    tracker.take_pending_activation();
                    let body = if self
                        .plan
                        .reentry
                        .swap(false, std::sync::atomic::Ordering::Relaxed)
                    {
                        text::reentry_reminder(&display)
                    } else if tracker.should_use_full_reminder() {
                        text::full_reminder(&display, has_content)
                    } else {
                        text::sparse_reminder().to_string()
                    };
                    tracker.record_reminder_injected();
                    fragments.push(ContextFragment::new(
                        "plan-mode/active",
                        ORDER_PLAN_MODE + 1,
                        reminder(&body),
                    ));
                }
            }

            fragments
        })
    }
}

impl ToolContributor for PlanExtension {
    fn tools(&self, ctx: &ExtensionContext) -> Vec<ArcTool> {
        // The first thing every turn does with a context, so a tool called in
        // this turn already has a plan file to compare against.
        self.plan.resolve_path(ctx);
        vec![
            Arc::new(EnterPlanMode::new(Arc::clone(&self.plan))),
            Arc::new(ExitPlanMode::new(Arc::clone(&self.plan))),
        ]
    }
}

impl keke_plugin_api::ApprovalReviewContributor for PlanExtension {
    fn review<'a>(
        &'a self,
        _ctx: &'a ExtensionContext,
        request: &'a ApprovalRequest,
    ) -> ExtFuture<'a, Option<ApprovalDecision>> {
        Box::pin(async move {
            // The plan is what the agent was told to write, so asking about
            // every revision of it is a prompt with only one sensible answer.
            // Everything else falls through to whoever else is registered.
            let plan = self.plan.plan_path()?;
            (self.plan.is_active()
                && BLOCKED_TOOLS.contains(&request.call.name.as_str())
                && targets(&request.call, plan))
            .then_some(ApprovalDecision::Allow)
        })
    }
}

/// Wrap text the way the model is trained to read out-of-band instruction.
fn reminder(body: &str) -> String {
    format!("<system-reminder>\n{body}\n</system-reminder>")
}

/// Whether a plan worth reading is already on disk.
///
/// An empty file — the seed `enter_plan_mode` writes — counts as no plan, so
/// the reminder still asks for one.
async fn plan_file_has_content(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|meta| meta.len() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    use keke_protocol::SessionId;
    use keke_protocol::ThreadId;
    use keke_protocol::ToolCallId;

    fn plan_mode(dir: &Path) -> (Arc<PlanMode>, Arc<SessionModeSwitch>) {
        let switch = Arc::new(SessionModeSwitch::new(SessionMode::Default));
        let plan = Arc::new(PlanMode::new(
            Arc::clone(&switch),
            PlanLocation::fixed(dir.join("plan.md")),
        ));
        let ctx = ExtensionContext::new(SessionId::new(), ThreadId::new());
        plan.resolve_path(&ctx);
        (plan, switch)
    }

    fn write_call(path: &Path) -> ToolCall {
        ToolCall {
            id: ToolCallId::new("c1"),
            name: "write_file".to_string(),
            arguments: serde_json::json!({ "path": path.display().to_string(), "content": "x" }),
        }
    }

    #[test]
    fn the_tracker_follows_the_switch_a_person_wrote() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (plan, switch) = plan_mode(dir.path());
        assert!(!plan.is_active());

        switch.set(SessionMode::Plan);
        plan.reconcile(false);
        assert!(plan.is_active());

        switch.set(SessionMode::Default);
        plan.reconcile(false);
        assert!(!plan.is_active());
    }

    #[test]
    fn a_tool_driven_entry_writes_the_switch_the_surface_reads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (plan, switch) = plan_mode(dir.path());
        assert!(plan.activate_from_tool());
        assert_eq!(switch.get(), SessionMode::Plan);
        assert!(plan.deactivate_approved());
        assert_eq!(switch.get(), SessionMode::Default);
    }

    #[test]
    fn a_write_outside_the_plan_file_is_denied_while_planning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (plan, _switch) = plan_mode(dir.path());
        plan.activate_from_tool();

        let denial = plan
            .denial(&write_call(&dir.path().join("src/main.rs")))
            .expect("denied");
        assert!(denial.contains("plan.md"), "{denial}");
    }

    #[test]
    fn the_plan_file_passes_the_guard_so_the_reviewer_can_allow_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (plan, _switch) = plan_mode(dir.path());
        plan.activate_from_tool();
        assert!(
            plan.denial(&write_call(&dir.path().join("plan.md")))
                .is_none()
        );
    }

    #[test]
    fn bash_is_not_blocked_because_a_planner_still_has_to_read_the_build() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (plan, _switch) = plan_mode(dir.path());
        plan.activate_from_tool();
        let call = ToolCall {
            id: ToolCallId::new("c2"),
            name: "bash".to_string(),
            arguments: serde_json::json!({ "command": "cargo check" }),
        };
        assert!(plan.denial(&call).is_none());
    }

    #[test]
    fn nothing_is_denied_when_plan_mode_is_off() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (plan, _switch) = plan_mode(dir.path());
        assert!(
            plan.denial(&write_call(&dir.path().join("src/main.rs")))
                .is_none()
        );
    }

    /// The engine records every fragment it assembles, so this asserts only
    /// what the extension is responsible for: that the reminder is a fragment
    /// at all, and says what it must.
    #[tokio::test]
    async fn an_active_session_is_reminded_that_it_is_planning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (plan, switch) = plan_mode(dir.path());
        switch.set(SessionMode::Plan);
        let extension = PlanExtension {
            plan: Arc::clone(&plan),
        };
        let ctx = ExtensionContext::new(SessionId::new(), ThreadId::new()).in_turn(TurnId::new());

        extension.on_turn_start(&ctx, TurnId::new()).await;
        let fragments = extension.contribute_turn_context(&ctx).await;

        assert_eq!(fragments.len(), 1);
        assert_eq!(fragments[0].name, "plan-mode/active");
        assert!(fragments[0].text.contains("<system-reminder>"));
        assert!(fragments[0].text.contains("Plan mode is active"));
    }

    #[tokio::test]
    async fn reminders_alternate_full_and_sparse_across_turns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (plan, switch) = plan_mode(dir.path());
        switch.set(SessionMode::Plan);
        let extension = PlanExtension { plan };
        let ctx = ExtensionContext::new(SessionId::new(), ThreadId::new()).in_turn(TurnId::new());
        extension.on_turn_start(&ctx, TurnId::new()).await;

        let first = extension.contribute_turn_context(&ctx).await;
        let second = extension.contribute_turn_context(&ctx).await;
        assert!(first[0].text.contains("Plan File"));
        assert!(second[0].text.contains("still active"));
    }

    #[tokio::test]
    async fn leaving_plan_mode_tells_the_model_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (plan, switch) = plan_mode(dir.path());
        switch.set(SessionMode::Plan);
        let extension = PlanExtension {
            plan: Arc::clone(&plan),
        };
        let ctx = ExtensionContext::new(SessionId::new(), ThreadId::new()).in_turn(TurnId::new());
        extension.on_turn_start(&ctx, TurnId::new()).await;

        switch.set(SessionMode::Default);
        extension.on_turn_start(&ctx, TurnId::new()).await;
        let fragments = extension.contribute_turn_context(&ctx).await;
        assert_eq!(fragments.len(), 1);
        assert!(fragments[0].text.contains("exited plan mode"));

        assert!(extension.contribute_turn_context(&ctx).await.is_empty());
    }
}
