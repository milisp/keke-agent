//! The two tools that move a session in and out of plan mode.
//!
//! Both are `ToolKind::Edit`, which is what makes
//! [`keke_core::approval_reason`] require a person's assent under
//! `ApprovalPolicy::OnRequest`. That is the whole enforcement: a tool body only
//! runs once approval has passed, so a refused `exit_plan_mode` never reaches
//! the code that would turn plan mode off, and the session stays planning.

use std::path::Path;
use std::sync::Arc;

use keke_protocol::ContentBlock;
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

use crate::PlanMode;

/// The plan file, or a failure that says why there isn't one yet.
fn plan_path(plan: &PlanMode) -> Result<&Path, ToolError> {
    plan.plan_path().ok_or_else(|| {
        ToolError::custom(
            "plan_file_unresolved",
            "plan mode has not been given a session directory yet",
        )
    })
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct EnterPlanModeArgs {
    /// Why this task needs planning first — the ambiguity you cannot resolve by
    /// reading the code. Shown to the person deciding whether to allow it.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EnterPlanModeOutput {
    pub plan_file: String,
    /// False when plan mode was already on, so the model does not read a
    /// no-op as a change of rules.
    pub entered: bool,
}

impl ToolOutput for EnterPlanModeOutput {
    fn render(&self) -> Vec<ContentBlock> {
        let text = if self.entered {
            format!(
                "Plan mode is now active. Research the codebase and write your plan to {}, the \
                 only file you may edit. Shell commands still work — use them to check what you \
                 are proposing. End your turn with `exit_plan_mode` to present the plan.",
                self.plan_file
            )
        } else {
            "Plan mode was already active.".to_string()
        };
        vec![ContentBlock::text(text)]
    }
}

/// Enters plan mode, once a person approves.
pub struct EnterPlanMode {
    plan: Arc<PlanMode>,
}

impl EnterPlanMode {
    #[must_use]
    pub fn new(plan: Arc<PlanMode>) -> Self {
        Self { plan }
    }
}

impl Tool for EnterPlanMode {
    type Args = EnterPlanModeArgs;
    type Output = EnterPlanModeOutput;

    fn id(&self) -> ToolId {
        ToolId::new("enter_plan_mode")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new(
            "Enter plan mode: research the task and write a proposal instead of implementing it. \
             Use this when the task has genuine ambiguity about the right approach — several \
             reasonable architectures, or a restructuring where the wrong approach wastes \
             significant work. Do not use it for a task with a clear implementation path. \
             Requires the user's approval. While plan mode is active you may read, search, and \
             run commands, but the only file you may write is the plan file.",
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::of_kind(ToolKind::Edit)
    }

    /// Nothing to enter when it is already entered.
    fn should_list(&self, _ctx: &ListToolsContext) -> bool {
        !self.plan.is_active()
    }

    async fn run(
        &self,
        _ctx: ToolCallContext,
        _args: Self::Args,
    ) -> Result<Self::Output, ToolError> {
        let path = plan_path(&self.plan)?.to_path_buf();
        let entered = self.plan.activate_from_tool();

        // Seeded empty so the path exists for the person to open, and so the
        // reminder can still say "no plan written yet" — an empty file is not
        // a plan.
        if entered
            && !path.exists()
            && let Some(parent) = path.parent()
        {
            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                ToolError::custom(
                    "plan_file_unwritable",
                    format!("{}: {error}", parent.display()),
                )
            })?;
            tokio::fs::write(&path, b"").await.map_err(|error| {
                ToolError::custom(
                    "plan_file_unwritable",
                    format!("{}: {error}", path.display()),
                )
            })?;
        }

        Ok(EnterPlanModeOutput {
            plan_file: path.display().to_string(),
            entered,
        })
    }
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ExitPlanModeArgs {
    /// The finished plan, in markdown. Optional: if you already wrote the plan
    /// file, leave this out and it is read from disk. Supplying it overwrites
    /// the plan file with this text.
    #[serde(default)]
    pub plan: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ExitPlanModeOutput {
    pub plan_file: String,
    /// Whether a plan was actually presented, or the file was empty.
    pub had_plan: bool,
    pub exited: bool,
}

impl ToolOutput for ExitPlanModeOutput {
    fn render(&self) -> Vec<ContentBlock> {
        let text = if !self.exited {
            "Plan mode was not active.".to_string()
        } else if self.had_plan {
            "The plan was approved and plan mode is off. You may now edit files and implement it."
                .to_string()
        } else {
            format!(
                "Plan mode is off, but {} was empty — say what you intend to do before editing.",
                self.plan_file
            )
        };
        vec![ContentBlock::text(text)]
    }
}

/// Presents the plan and, once a person approves, leaves plan mode.
pub struct ExitPlanMode {
    plan: Arc<PlanMode>,
}

impl ExitPlanMode {
    #[must_use]
    pub fn new(plan: Arc<PlanMode>) -> Self {
        Self { plan }
    }
}

impl Tool for ExitPlanMode {
    type Args = ExitPlanModeArgs;
    type Output = ExitPlanModeOutput;

    fn id(&self) -> ToolId {
        ToolId::new("exit_plan_mode")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new(
            "Present the finished plan and ask to leave plan mode. Call this only when the plan \
             is complete: it names the recommended approach, the files to change, what already \
             exists that should be reused, and how the result will be verified. The user reviews \
             the plan and approves or rejects; only an approval leaves plan mode, and a rejection \
             means keep planning.",
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::of_kind(ToolKind::Edit)
    }

    /// Nothing to exit when nothing was entered.
    fn should_list(&self, _ctx: &ListToolsContext) -> bool {
        self.plan.is_active()
    }

    async fn run(
        &self,
        _ctx: ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolError> {
        let path = plan_path(&self.plan)?.to_path_buf();

        // Written straight to disk rather than through `write_file`: this is
        // the plan being filed, not an edit the guard has an opinion about.
        if let Some(plan) = args
            .plan
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            if let Some(parent) = path.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            tokio::fs::write(&path, plan.as_bytes())
                .await
                .map_err(|error| {
                    ToolError::custom(
                        "plan_file_unwritable",
                        format!("{}: {error}", path.display()),
                    )
                })?;
        }

        let had_plan = tokio::fs::read_to_string(&path)
            .await
            .is_ok_and(|text| !text.trim().is_empty());

        // Reached only because approval passed — a denied call is refused in
        // dispatch and this body never runs, which is what leaves a rejected
        // plan still planning.
        let exited = self.plan.deactivate_approved();

        Ok(ExitPlanModeOutput {
            plan_file: path.display().to_string(),
            had_plan,
            exited,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use keke_config_types::SessionMode;
    use keke_core::SessionModeSwitch;
    use keke_paths::AbsPath;
    use keke_plugin_api::ExtensionContext;
    use keke_protocol::SessionId;
    use keke_protocol::ThreadId;
    use keke_protocol::ToolCallId;

    use crate::PlanLocation;

    fn fixture() -> (tempfile::TempDir, Arc<PlanMode>, ToolCallContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonicalize");
        let plan = Arc::new(PlanMode::new(
            Arc::new(SessionModeSwitch::new(SessionMode::Default)),
            PlanLocation::fixed(root.join("plan.md")),
        ));
        plan.resolve_path(&ExtensionContext::new(SessionId::new(), ThreadId::new()));
        let ctx = ToolCallContext {
            call_id: ToolCallId::new("call-1"),
            workspace_root: AbsPath::new(&root).expect("absolute"),
            timeout_millis: None,
            cancelled: Arc::new(|| false),
        };
        (dir, plan, ctx)
    }

    #[tokio::test]
    async fn entering_seeds_an_empty_plan_file_and_turns_the_mode_on() {
        let (_dir, plan, ctx) = fixture();
        let out = EnterPlanMode::new(Arc::clone(&plan))
            .run(ctx, EnterPlanModeArgs::default())
            .await
            .expect("enters");
        assert!(out.entered);
        assert!(plan.is_active());
        assert!(Path::new(&out.plan_file).exists());
    }

    #[tokio::test]
    async fn exiting_files_the_plan_and_turns_the_mode_off() {
        let (_dir, plan, ctx) = fixture();
        plan.activate_from_tool();

        let out = ExitPlanMode::new(Arc::clone(&plan))
            .run(
                ctx,
                ExitPlanModeArgs {
                    plan: Some("## Context\nDo the thing.".to_string()),
                },
            )
            .await
            .expect("exits");

        assert!(out.exited);
        assert!(out.had_plan);
        assert!(!plan.is_active());
        let written = std::fs::read_to_string(&out.plan_file).expect("plan file");
        assert!(written.contains("Do the thing."));
    }

    #[tokio::test]
    async fn an_empty_plan_still_exits_and_says_so() {
        let (_dir, plan, ctx) = fixture();
        plan.activate_from_tool();
        let out = ExitPlanMode::new(plan)
            .run(ctx, ExitPlanModeArgs::default())
            .await
            .expect("exits");
        assert!(!out.had_plan);
        assert!(
            super::ToolOutput::render(&out)[0]
                == ContentBlock::text(format!(
                    "Plan mode is off, but {} was empty — say what you intend to do before editing.",
                    out.plan_file
                ))
        );
    }

    /// The pair is only offered where it means something.
    #[test]
    fn each_tool_is_advertised_only_in_the_mode_it_can_change() {
        let (_dir, plan, _ctx) = fixture();
        let list = ListToolsContext::default();
        assert!(EnterPlanMode::new(Arc::clone(&plan)).should_list(&list));
        assert!(!ExitPlanMode::new(Arc::clone(&plan)).should_list(&list));

        plan.activate_from_tool();
        assert!(!EnterPlanMode::new(Arc::clone(&plan)).should_list(&list));
        assert!(ExitPlanMode::new(plan).should_list(&list));
    }

    /// Both must be things `approval_reason` asks about, or "requires approval"
    /// is only a doc comment.
    #[test]
    fn both_tools_require_approval_under_the_on_request_policy() {
        let (_dir, plan, _ctx) = fixture();
        for capabilities in [
            EnterPlanMode::new(Arc::clone(&plan)).capabilities(),
            ExitPlanMode::new(plan).capabilities(),
        ] {
            assert!(
                keke_core::approval_reason(
                    keke_config_types::ApprovalPolicy::OnRequest,
                    &capabilities
                )
                .is_some()
            );
        }
    }
}
