//! What the model can do with the scheduler, through the tool it is given.

use std::sync::Arc;

use keke_paths::AbsPath;
use keke_protocol::ToolCallId;
use keke_tasks::TaskSource;
use keke_tool::Tool;
use keke_tool::ToolCallContext;

use crate::Origin;
use crate::SchedulePrompt;
use crate::Schedules;
use crate::tools::SchedulePromptArgs;

fn tool() -> (SchedulePrompt, Schedules) {
    let schedules = Schedules::default();
    (
        SchedulePrompt {
            schedules: schedules.clone(),
        },
        schedules,
    )
}

/// The tool touches nothing on disk, so the workspace root only has to be a
/// real absolute path.
fn context() -> ToolCallContext {
    ToolCallContext {
        call_id: ToolCallId::new("call-1"),
        workspace_root: AbsPath::new(std::env::temp_dir()).expect("absolute"),
        timeout_millis: None,
        cancelled: Arc::new(|| false),
    }
}

fn args(interval: &str, prompt: &str) -> SchedulePromptArgs {
    SchedulePromptArgs {
        interval: interval.to_string(),
        prompt: prompt.to_string(),
    }
}

#[tokio::test]
async fn a_scheduled_prompt_is_listed_as_an_outstanding_task() {
    let (tool, schedules) = tool();
    let output = tool
        .run(context(), args("10m", "check the build"))
        .await
        .unwrap();

    assert_eq!(output.task_id, "loop_1");
    let rows = schedules.snapshots();
    assert_eq!(rows[0].id, "loop_1");
    assert!(rows[0].description.contains("check the build"), "{rows:?}");
}

/// The model is mid-turn when it asks. A loop due at once would re-ask the
/// question it is already answering.
#[tokio::test]
async fn the_model_waits_out_the_first_interval() {
    let (tool, schedules) = tool();
    let output = tool
        .run(context(), args("5m", "check the build"))
        .await
        .unwrap();

    assert_eq!(output.first_in_seconds, 300);
    schedules.with(|scheduler| {
        assert!(scheduler.take_due(std::time::Instant::now()).is_none());
    });
}

#[tokio::test]
async fn an_interval_it_cannot_parse_says_what_one_looks_like() {
    let (tool, schedules) = tool();
    let error = tool
        .run(context(), args("soon", "check"))
        .await
        .unwrap_err();

    assert!(format!("{error}").contains("5m"), "{error}");
    assert!(schedules.is_empty());
}

#[tokio::test]
async fn a_busy_wait_is_refused_rather_than_rounded_up() {
    let (tool, schedules) = tool();
    let error = tool.run(context(), args("5s", "check")).await.unwrap_err();

    assert!(format!("{error}").contains("60s"), "{error}");
    assert!(schedules.is_empty());
}

/// The cap is what stops a model that schedules a prompt which schedules a
/// prompt from filling the session with timers.
#[tokio::test]
async fn the_cap_is_reported_rather_than_silently_dropping_the_loop() {
    let (tool, schedules) = tool();
    for _ in 0..crate::MAX_TASKS {
        schedules
            .add(
                std::time::Duration::from_secs(60),
                "check".into(),
                Origin::Model,
                false,
            )
            .unwrap();
    }
    let error = tool.run(context(), args("5m", "check")).await.unwrap_err();
    assert!(format!("{error}").contains("already running"), "{error}");
}

/// One registry, so the model does not have to know which verb goes with which
/// kind of outstanding work.
#[tokio::test]
async fn the_shared_verbs_reach_a_loop_by_the_id_the_tool_returned() {
    let (tool, schedules) = tool();
    let id = tool
        .run(context(), args("5m", "check"))
        .await
        .unwrap()
        .task_id;
    let source: Arc<dyn TaskSource> = Arc::new(schedules.clone());

    assert!(source.owns(&id));
    assert!(source.take_output(&id).unwrap().text.contains("not fired"));
    assert!(source.kill(&id));
    assert!(schedules.is_empty());
}
