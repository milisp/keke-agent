//! The three verbs, over whatever sources the composition registered.
//!
//! None of them owns state: each finds the source that claims the id and asks
//! it. That is what keeps every kind of outstanding work answering to one set
//! of names, and what keeps the lifecycle of each kind in exactly one place.

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

use crate::source::TaskSources;

/// How long a wait may last when the model names no budget.
const DEFAULT_WAIT_MS: u64 = 30_000;
/// The ceiling on one wait. Longer than this and the turn is simply blocked,
/// which is what the background flag existed to avoid.
const MAX_WAIT_MS: u64 = 600_000;
/// How often a wait looks at the rows again.
const POLL: std::time::Duration = std::time::Duration::from_millis(100);

fn no_such_task(id: &str) -> ToolError {
    ToolError::custom(
        "no_such_task",
        format!("no task named `{id}` — `list_tasks` has the ids"),
    )
}

// --- list -------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListTasksArgs {}

#[derive(Debug, Serialize)]
pub struct ListedTask {
    pub task_id: String,
    pub kind: String,
    pub status: String,
    pub description: String,
}

#[derive(Debug, Serialize)]
pub struct ListTasksOutput {
    pub tasks: Vec<ListedTask>,
}

impl ToolOutput for ListTasksOutput {
    fn render(&self) -> Vec<ContentBlock> {
        if self.tasks.is_empty() {
            return vec![ContentBlock::text("no tasks are running")];
        }
        let rows: Vec<String> = self
            .tasks
            .iter()
            .map(|task| {
                format!(
                    "{} [{}] {} — {}",
                    task.task_id, task.kind, task.status, task.description
                )
            })
            .collect();
        vec![ContentBlock::text(rows.join("\n"))]
    }
}

/// Everything this session has left running.
pub struct ListTasks {
    pub(crate) sources: TaskSources,
}

impl Tool for ListTasks {
    type Args = ListTasksArgs;
    type Output = ListTasksOutput;

    fn id(&self) -> ToolId {
        ToolId::new("list_tasks")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new(
            "List every background command and subagent this session has started, with its id, \
             what it is doing, and whether it is still running.",
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            kind: ToolKind::Read,
            ..ToolCapabilities::default()
        }
    }

    async fn run(
        &self,
        _ctx: ToolCallContext,
        _args: Self::Args,
    ) -> Result<Self::Output, ToolError> {
        Ok(ListTasksOutput {
            tasks: self
                .sources
                .snapshots()
                .into_iter()
                .map(|row| ListedTask {
                    task_id: row.id,
                    kind: row.kind.to_string(),
                    status: row.state.label(),
                    description: row.description,
                })
                .collect(),
        })
    }
}

// --- output -----------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskOutputArgs {
    /// The task to read, as returned when it was started.
    pub task_id: String,
    /// Wait up to this many milliseconds for the task to finish before
    /// answering. Omit to read what is there and return at once.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct TaskOutputResult {
    pub task_id: String,
    pub status: String,
    /// Everything the task has said since the last read.
    pub output: String,
    /// Bytes dropped because the task outran its buffer, if any.
    pub dropped_bytes: u64,
}

impl ToolOutput for TaskOutputResult {
    fn render(&self) -> Vec<ContentBlock> {
        let mut text = String::new();
        if self.dropped_bytes > 0 {
            text.push_str(&format!(
                "[{} earlier bytes dropped — this is the tail]\n",
                self.dropped_bytes
            ));
        }
        if self.output.is_empty() {
            text.push_str("(no new output)\n");
        } else {
            text.push_str(&self.output);
            if !text.ends_with('\n') {
                text.push('\n');
            }
        }
        text.push_str(&format!("[{}]", self.status));
        vec![ContentBlock::text(text)]
    }
}

/// Read what a task has said since the last read.
pub struct TaskOutputTool {
    pub(crate) sources: TaskSources,
}

impl Tool for TaskOutputTool {
    type Args = TaskOutputArgs;
    type Output = TaskOutputResult;

    fn id(&self) -> ToolId {
        ToolId::new("task_output")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new(
            "Read what a background task has produced since you last read it, and whether it is \
             still running. Each read consumes what it returns, so poll rather than re-reading. \
             Set `timeout_ms` to wait for the task to finish instead of returning immediately.",
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            kind: ToolKind::Read,
            timeout_millis: Some(MAX_WAIT_MS),
            ..ToolCapabilities::default()
        }
    }

    async fn run(&self, ctx: ToolCallContext, args: Self::Args) -> Result<Self::Output, ToolError> {
        let source = self
            .sources
            .find(&args.task_id)
            .ok_or_else(|| no_such_task(&args.task_id))?;

        if let Some(budget) = args.timeout_ms {
            let ceiling = ctx.timeout_millis.unwrap_or(MAX_WAIT_MS);
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_millis(budget.min(ceiling));
            while std::time::Instant::now() < deadline {
                if ctx.is_cancelled() {
                    return Err(ToolError::Cancelled);
                }
                match source.snapshot(&args.task_id) {
                    Some(row) if row.state.is_running() => {}
                    _ => break,
                }
                tokio::time::sleep(POLL).await;
            }
        }

        let row = source
            .snapshot(&args.task_id)
            .ok_or_else(|| no_such_task(&args.task_id))?;
        let output = source.take_output(&args.task_id).unwrap_or_default();
        Ok(TaskOutputResult {
            task_id: args.task_id,
            status: row.state.label(),
            output: output.text,
            dropped_bytes: output.dropped,
        })
    }
}

// --- wait -------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema, Default, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum WaitMode {
    /// Return as soon as any one of them finishes.
    #[default]
    Any,
    /// Return only once every one of them has.
    All,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitTasksArgs {
    /// The tasks to wait for.
    pub task_ids: Vec<String>,
    #[serde(default)]
    pub mode: WaitMode,
    /// How long to wait. Defaults to 30 seconds, capped at ten minutes.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct WaitTasksOutput {
    pub finished: Vec<TaskOutputResult>,
    pub still_running: Vec<String>,
    /// Whether the wait ended because the budget ran out rather than because
    /// the condition was met.
    pub timed_out: bool,
}

impl ToolOutput for WaitTasksOutput {
    fn render(&self) -> Vec<ContentBlock> {
        let mut blocks: Vec<ContentBlock> = self
            .finished
            .iter()
            .flat_map(|result| {
                let mut text = format!("--- {} ---\n", result.task_id);
                for block in result.render() {
                    if let ContentBlock::Text { text: part } = block {
                        text.push_str(&part);
                    }
                }
                vec![ContentBlock::text(text)]
            })
            .collect();
        if !self.still_running.is_empty() {
            blocks.push(ContentBlock::text(format!(
                "still running: {}",
                self.still_running.join(", ")
            )));
        }
        if blocks.is_empty() {
            blocks.push(ContentBlock::text("nothing to wait for"));
        }
        blocks
    }
}

/// Block until some or all of several tasks are done.
pub struct WaitTasks {
    pub(crate) sources: TaskSources,
}

impl Tool for WaitTasks {
    type Args = WaitTasksArgs;
    type Output = WaitTasksOutput;

    fn id(&self) -> ToolId {
        ToolId::new("wait_tasks")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new(
            "Wait for several background tasks at once. `mode: any` returns when the first one \
             finishes, `mode: all` when every one has. Returns each finished task's output, so a \
             separate `task_output` call is not needed for the ones it reports.",
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            kind: ToolKind::Read,
            timeout_millis: Some(MAX_WAIT_MS),
            ..ToolCapabilities::default()
        }
    }

    async fn run(&self, ctx: ToolCallContext, args: Self::Args) -> Result<Self::Output, ToolError> {
        for id in &args.task_ids {
            if self.sources.find(id).is_none() {
                return Err(no_such_task(id));
            }
        }
        let ceiling = ctx.timeout_millis.unwrap_or(MAX_WAIT_MS);
        let budget = args.timeout_ms.unwrap_or(DEFAULT_WAIT_MS).min(ceiling);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(budget);

        let done = |id: &String| {
            self.sources
                .find(id)
                .and_then(|source| source.snapshot(id))
                .is_none_or(|row| !row.state.is_running())
        };

        let mut timed_out = false;
        loop {
            let finished = args.task_ids.iter().filter(|id| done(id)).count();
            let met = match args.mode {
                WaitMode::Any => finished > 0,
                WaitMode::All => finished == args.task_ids.len(),
            };
            if met || args.task_ids.is_empty() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                timed_out = true;
                break;
            }
            if ctx.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            tokio::time::sleep(POLL).await;
        }

        // Only the finished ones are drained. A task still running keeps its
        // output for the next read, so waiting for one sibling never costs the
        // lines another had produced meanwhile.
        let mut results = Vec::new();
        let mut still_running = Vec::new();
        for id in &args.task_ids {
            let Some(source) = self.sources.find(id) else {
                continue;
            };
            let Some(row) = source.snapshot(id) else {
                continue;
            };
            if row.state.is_running() {
                still_running.push(id.clone());
                continue;
            }
            let output = source.take_output(id).unwrap_or_default();
            results.push(TaskOutputResult {
                task_id: id.clone(),
                status: row.state.label(),
                output: output.text,
                dropped_bytes: output.dropped,
            });
        }
        Ok(WaitTasksOutput {
            finished: results,
            still_running,
            timed_out,
        })
    }
}

// --- kill -------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct KillTaskArgs {
    pub task_id: String,
}

#[derive(Debug, Serialize)]
pub struct KillTaskOutput {
    pub task_id: String,
    /// What the task had said and had not been read yet. Handed over rather
    /// than dropped: killing a task is often how a person finds out why it was
    /// stuck, and the answer is in its last lines.
    pub output: String,
}

impl ToolOutput for KillTaskOutput {
    fn render(&self) -> Vec<ContentBlock> {
        let mut text = format!("killed {}", self.task_id);
        if !self.output.is_empty() {
            text.push('\n');
            text.push_str(&self.output);
        }
        vec![ContentBlock::text(text)]
    }
}

/// Stop a task that is still running.
pub struct KillTask {
    pub(crate) sources: TaskSources,
}

impl Tool for KillTask {
    type Args = KillTaskArgs;
    type Output = KillTaskOutput;

    fn id(&self) -> ToolId {
        ToolId::new("kill_task")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new(
            "Stop a background command or subagent. Succeeds whether or not it was still running, \
             and returns whatever output had not been read yet.",
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            kind: ToolKind::Execute,
            ..ToolCapabilities::default()
        }
    }

    async fn run(
        &self,
        _ctx: ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolError> {
        let source = self
            .sources
            .find(&args.task_id)
            .ok_or_else(|| no_such_task(&args.task_id))?;
        if !source.kill(&args.task_id) {
            return Err(no_such_task(&args.task_id));
        }
        let output = source.take_output(&args.task_id).unwrap_or_default();
        Ok(KillTaskOutput {
            task_id: args.task_id,
            output: output.text,
        })
    }
}
