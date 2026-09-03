//! `schedule_prompt` — the model's way to arrange to be asked again.
//!
//! Only creation lives here. Listing a loop, reading what it has done, and
//! stopping it are the shared task verbs in `keke-tasks`, because a loop is one
//! more thing the session has left outstanding and the model should not have to
//! learn a second set of names for it.
//!
//! There is no verb for "fire this now": the model asking to be prompted this
//! instant is the model continuing, which it can already do by carrying on.

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

use crate::handle::Schedules;
use crate::scheduler::MAX_TASKS;
use crate::scheduler::MIN_INTERVAL;
use crate::scheduler::Origin;
use crate::scheduler::parse_interval;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SchedulePromptArgs {
    /// How often to send it: a whole number and one of `s`, `m`, `h`, `d` —
    /// `90s`, `5m`, `2h`, `1d`. At least 60 seconds.
    pub interval: String,
    /// The instruction to send each time, written the way a person would write
    /// it. It arrives with no memory of this turn attached beyond the
    /// conversation itself, so name what to look at.
    pub prompt: String,
}

#[derive(Debug, Serialize)]
pub struct SchedulePromptOutput {
    pub task_id: String,
    pub interval: String,
    pub first_in_seconds: u64,
}

impl ToolOutput for SchedulePromptOutput {
    fn render(&self) -> Vec<ContentBlock> {
        vec![ContentBlock::text(format!(
            "{} scheduled every {} — first in {}s. `list_tasks` shows it, `task_output {}` says \
             what it has done, `kill_task {}` stops it.",
            self.task_id, self.interval, self.first_in_seconds, self.task_id, self.task_id,
        ))]
    }
}

/// Ask to be sent a prompt again on an interval.
pub struct SchedulePrompt {
    pub(crate) schedules: Schedules,
}

impl Tool for SchedulePrompt {
    type Args = SchedulePromptArgs;
    type Output = SchedulePromptOutput;

    fn id(&self) -> ToolId {
        ToolId::new("schedule_prompt")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new(format!(
            "Arrange for a prompt to be sent to you again every interval, so you can come back to \
             something later without holding this turn open — checking a long build, re-reading a \
             log, following a deploy. The prompt starts a fresh turn when it fires, and never \
             interrupts one that is running. Minimum interval {}s, at most {MAX_TASKS} at once. \
             Use it for work that must be looked at repeatedly over time; to wait once for \
             something this session started, use `wait_tasks` instead.",
            MIN_INTERVAL.as_secs(),
        ))
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            // A standing prompt changes what the session will do next and
            // nothing outside it — the same kind of state a todo list is.
            kind: ToolKind::Meta,
            ..ToolCapabilities::default()
        }
    }

    async fn run(
        &self,
        _ctx: ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolError> {
        let Some(interval) = parse_interval(&args.interval) else {
            return Err(ToolError::custom(
                "bad_interval",
                format!(
                    "`{}` is not an interval — a whole number and one of s, m, h, d, as in `5m`",
                    args.interval
                ),
            ));
        };
        // Not immediately: the model is mid-turn, and a loop due the instant it
        // is created would ask again the moment this turn ends.
        let task_id = self
            .schedules
            .add(interval, args.prompt, Origin::Model, false)
            .map_err(|refusal| ToolError::custom("schedule_refused", refusal))?;
        Ok(SchedulePromptOutput {
            task_id,
            interval: crate::scheduler::format_interval(interval),
            first_in_seconds: interval.as_secs(),
        })
    }
}
