//! The model-facing surface: two tools, both thin.
//!
//! Neither owns lifecycle state. They translate a model's request into a call
//! on [`SubagentHost`] and translate the report back, which is what keeps the
//! single place that records a subagent's transitions single.

use std::sync::Arc;

use keke_plugin_api::ExtensionContext;
use keke_protocol::ContentBlock;
use keke_protocol::SessionEvent;
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

use crate::host::AgentReport;
use crate::host::SubagentError;
use crate::host::SubagentHost;

/// A report as the model sees it.
#[derive(Debug, Serialize)]
pub struct ReportedAgent {
    pub agent_id: String,
    pub status: String,
    pub summary: String,
    /// Charged back so the parent's model can see what delegating cost.
    pub tokens: u64,
}

impl From<&AgentReport> for ReportedAgent {
    fn from(report: &AgentReport) -> Self {
        Self {
            agent_id: report.id.clone(),
            status: report.status.as_str().to_string(),
            summary: report.summary.clone(),
            tokens: report.usage.total(),
        }
    }
}

fn tool_error(error: SubagentError) -> ToolError {
    match error {
        SubagentError::Unattached => ToolError::custom("subagents_unavailable", error.to_string()),
        SubagentError::Unknown(_) => ToolError::custom("no_such_subagent", error.to_string()),
        SubagentError::Lost(_) => ToolError::custom("subagent_lost", error.to_string()),
    }
}

// --- spawn ------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SpawnAgentArgs {
    /// The complete instruction for the subagent. It shares no history with
    /// this conversation, so everything it needs must be stated here.
    pub task: String,
    /// Wait for the result instead of returning a handle. Leave this off when
    /// starting several subagents that should run at the same time.
    #[serde(default)]
    pub wait: bool,
}

#[derive(Debug, Serialize)]
pub enum SpawnAgentOutput {
    /// Started, not waited for.
    Started { agent_id: String },
    /// Started and finished, because `wait` was set.
    Finished(Box<ReportedAgent>),
}

impl ToolOutput for SpawnAgentOutput {
    fn render(&self) -> Vec<ContentBlock> {
        match self {
            Self::Started { agent_id } => vec![ContentBlock::text(format!(
                "{agent_id} started. Call `collect_agent` for its result."
            ))],
            Self::Finished(report) => vec![ContentBlock::text(render_one(report))],
        }
    }
}

fn render_one(report: &ReportedAgent) -> String {
    format!(
        "{} [{}] ({} tokens)\n{}",
        report.agent_id, report.status, report.tokens, report.summary
    )
}

/// Starts an isolated child session working on one task.
pub struct SpawnAgent {
    pub(crate) host: Arc<SubagentHost>,
    /// This turn's extension context, captured when the tool set was assembled.
    /// It is how a tool records session events: the tool ABI deliberately hands
    /// out no engine handle, and a subagent that ran unlogged would break
    /// *model-visible implies logged* (`AGENTS.md` invariant 6).
    pub(crate) ctx: ExtensionContext,
}

impl SpawnAgent {
    fn record_start(&self, agent: &str, task: &str) {
        if let Some(turn) = self.ctx.turn() {
            self.ctx.record(SessionEvent::SubagentStart {
                turn,
                agent: agent.to_string(),
                task: task.to_string(),
            });
        }
    }

    fn record_end(&self, report: &AgentReport) {
        if let Some(turn) = self.ctx.turn() {
            self.ctx.record(crate::host::end_event(turn, report));
        }
    }
}

impl Tool for SpawnAgent {
    type Args = SpawnAgentArgs;
    type Output = SpawnAgentOutput;

    fn id(&self) -> ToolId {
        ToolId::new("spawn_agent")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new(format!(
            "Start a subagent: a fresh session with the same tools and workspace that works on \
             one task and reports back a summary. Use it for work whose intermediate output you \
             do not need to read — searching a large codebase, trying an approach that may not \
             pan out — so its context stays out of yours.\n\nThe subagent shares none of this \
             conversation: state the task completely. It cannot start subagents of its own. Up \
             to {} run at once; further ones wait their turn. Set `wait` to block for the \
             result, or leave it off and call `collect_agent` later — starting several without \
             waiting is how you get them running in parallel.",
            self.host.limits().max_concurrent
        ))
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            // A subagent runs the same tools this session has, shell included,
            // so the kind reflects what it can do rather than what starting it
            // does. Concurrency-safe because running several at once is the
            // point; the pool, not the dispatcher, is what bounds them.
            concurrency_safe: true,
            ..ToolCapabilities::of_kind(ToolKind::Execute)
        }
    }

    /// Withheld from a subagent's own turns, which is what keeps the tree one
    /// level deep without a depth counter to configure or get wrong.
    fn should_list(&self, _ctx: &ListToolsContext) -> bool {
        self.host.is_attached() && !self.host.is_child(self.ctx.session)
    }

    async fn run(&self, ctx: ToolCallContext, args: Self::Args) -> Result<Self::Output, ToolError> {
        let task = args.task.trim().to_string();
        if task.is_empty() {
            return Err(ToolError::custom(
                "empty_task",
                "a subagent needs a task; it shares no history to infer one from",
            ));
        }

        let id = self
            .host
            .spawn(self.ctx.session, task.clone(), Arc::clone(&ctx.cancelled))
            .map_err(tool_error)?;
        self.record_start(&id, &task);

        if !args.wait {
            return Ok(SpawnAgentOutput::Started { agent_id: id });
        }

        let report = self.host.collect(&id).await.map_err(tool_error)?;
        self.record_end(&report);
        Ok(SpawnAgentOutput::Finished(Box::new(ReportedAgent::from(
            &report,
        ))))
    }
}

// --- collect ----------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CollectAgentArgs {
    /// Which subagent to collect. Omit to wait for every one still outstanding.
    #[serde(default)]
    pub agent_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CollectAgentOutput {
    pub agents: Vec<ReportedAgent>,
}

impl ToolOutput for CollectAgentOutput {
    fn render(&self) -> Vec<ContentBlock> {
        if self.agents.is_empty() {
            return vec![ContentBlock::text("No subagents were outstanding.")];
        }
        let text = self
            .agents
            .iter()
            .map(render_one)
            .collect::<Vec<_>>()
            .join("\n\n");
        vec![ContentBlock::text(text)]
    }
}

/// Waits for subagents and returns what they reported.
pub struct CollectAgent {
    pub(crate) host: Arc<SubagentHost>,
    pub(crate) ctx: ExtensionContext,
}

impl Tool for CollectAgent {
    type Args = CollectAgentArgs;
    type Output = CollectAgentOutput;

    fn id(&self) -> ToolId {
        ToolId::new("collect_agent")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new(
            "Wait for subagents started by `spawn_agent` and return their reports. Name one with \
             `agent_id`, or omit it to wait for all outstanding ones at once — which is what you \
             want after starting several. A subagent is reported once; collecting it again is an \
             error, not a repeat.",
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        // Waiting changes nothing on its own; what the subagent already did is
        // accounted for under `spawn_agent`.
        ToolCapabilities::of_kind(ToolKind::Meta)
    }

    fn should_list(&self, _ctx: &ListToolsContext) -> bool {
        self.host.is_attached() && !self.host.is_child(self.ctx.session)
    }

    async fn run(
        &self,
        _ctx: ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolError> {
        let wanted = match args.agent_id {
            Some(id) => vec![id],
            None => self.host.outstanding(),
        };

        let mut agents = Vec::with_capacity(wanted.len());
        for id in wanted {
            let report = self.host.collect(&id).await.map_err(tool_error)?;
            if let Some(turn) = self.ctx.turn() {
                self.ctx.record(crate::host::end_event(turn, &report));
            }
            agents.push(ReportedAgent::from(&report));
        }
        Ok(CollectAgentOutput { agents })
    }
}
