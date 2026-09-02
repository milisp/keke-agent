//! What a background command promises: it starts, it keeps what it says, it
//! stops when asked, and reading it twice does not repeat itself.

use std::sync::Arc;
use std::time::Duration;

use keke_config_types::BackgroundLimits;
use keke_paths::AbsPath;

use crate::BackgroundTasks;
use crate::TaskSource;
use crate::TaskState;

fn host(limits: BackgroundLimits) -> (Arc<BackgroundTasks>, tempfile::TempDir, AbsPath) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = AbsPath::new(dir.path()).expect("abs path");
    (Arc::new(BackgroundTasks::new(limits)), dir, root)
}

/// Poll rather than sleep a fixed time: the point is that the command finished,
/// not how fast this machine ran it.
async fn settle(tasks: &BackgroundTasks, id: &str) {
    for _ in 0..200 {
        match tasks.snapshot(id) {
            Some(row) if row.state.is_running() => {}
            _ => return,
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{id} never finished");
}

#[tokio::test]
async fn a_background_command_returns_before_it_finishes() {
    let (tasks, _dir, root) = host(BackgroundLimits::default());
    let id = tasks
        .spawn("sleep 0.2; echo done".to_string(), &root)
        .expect("spawn");

    // The spawn returned while the child was still asleep, which is the whole
    // claim being made.
    assert_eq!(
        tasks.snapshot(&id).expect("row").state,
        TaskState::Running,
        "spawn blocked until the command was done"
    );

    settle(&tasks, &id).await;
    let output = tasks.take_output(&id).expect("output");
    assert_eq!(output.text.trim(), "done");
    assert_eq!(
        tasks.snapshot(&id).expect("row").state,
        TaskState::Exited(Some(0))
    );
}

#[tokio::test]
async fn stderr_arrives_in_the_same_buffer_as_stdout() {
    let (tasks, _dir, root) = host(BackgroundLimits::default());
    let id = tasks
        .spawn("echo out; echo err 1>&2".to_string(), &root)
        .expect("spawn");
    settle(&tasks, &id).await;

    let text = tasks.take_output(&id).expect("output").text;
    assert!(text.contains("out"), "{text}");
    assert!(text.contains("err"), "{text}");
}

/// Reading is consuming, so a model polling a long-running command spends its
/// context on new lines rather than on the whole log every time.
#[tokio::test]
async fn a_read_takes_what_it_returns() {
    let (tasks, _dir, root) = host(BackgroundLimits::default());
    let id = tasks.spawn("echo once".to_string(), &root).expect("spawn");
    settle(&tasks, &id).await;

    assert_eq!(tasks.take_output(&id).expect("first").text.trim(), "once");
    assert_eq!(tasks.take_output(&id).expect("second").text, "");
}

#[tokio::test]
async fn a_killed_command_says_it_was_killed_rather_than_that_it_exited() {
    let (tasks, _dir, root) = host(BackgroundLimits::default());
    let id = tasks.spawn("sleep 30".to_string(), &root).expect("spawn");
    assert!(tasks.kill(&id));
    settle(&tasks, &id).await;

    assert_eq!(tasks.snapshot(&id).expect("row").state, TaskState::Killed);
}

/// Killing something that already finished is not an error: a caller cannot
/// know it finished between its last read and this call.
#[tokio::test]
async fn killing_a_finished_command_still_succeeds() {
    let (tasks, _dir, root) = host(BackgroundLimits::default());
    let id = tasks.spawn("true".to_string(), &root).expect("spawn");
    settle(&tasks, &id).await;

    assert!(tasks.kill(&id));
    assert_eq!(
        tasks.snapshot(&id).expect("row").state,
        TaskState::Exited(Some(0))
    );
}

#[tokio::test]
async fn an_unknown_id_is_owned_by_nobody() {
    let (tasks, _dir, _root) = host(BackgroundLimits::default());
    assert!(!tasks.owns("agent_1"));
    assert!(tasks.owns("command_1"));
    assert!(tasks.snapshot("command_99").is_none());
    assert!(!tasks.kill("command_99"));
}

/// A refusal rather than a queue: the model asked to start something and carry
/// on, and a start that silently waits is a different answer.
#[tokio::test]
async fn one_command_past_the_limit_is_refused() {
    let limits = BackgroundLimits {
        max_concurrent: 1,
        ..BackgroundLimits::default()
    };
    let (tasks, _dir, root) = host(limits);
    let id = tasks.spawn("sleep 30".to_string(), &root).expect("spawn");

    let error = tasks
        .spawn("sleep 30".to_string(), &root)
        .expect_err("the second should be refused");
    assert!(error.to_string().contains("already running"), "{error}");

    // The slot frees up once the first one is gone.
    tasks.kill(&id);
    settle(&tasks, &id).await;
    assert!(tasks.spawn("true".to_string(), &root).is_ok());
}

/// The buffer is a tail. A command that outruns it loses its oldest output and
/// says how much, rather than growing until the session notices.
#[tokio::test]
async fn output_past_the_cap_drops_the_oldest_and_says_so() {
    let limits = BackgroundLimits {
        output_bytes: BackgroundLimits::MIN_OUTPUT_BYTES,
        ..BackgroundLimits::default()
    };
    let (tasks, _dir, root) = host(limits);
    let id = tasks
        .spawn(
            "for i in $(seq 1 2000); do echo line-$i; done".to_string(),
            &root,
        )
        .expect("spawn");
    settle(&tasks, &id).await;

    let output = tasks.take_output(&id).expect("output");
    assert!(output.dropped > 0, "nothing was dropped");
    assert!(
        u64::try_from(output.text.len()).unwrap_or(u64::MAX) <= BackgroundLimits::MIN_OUTPUT_BYTES,
        "buffer grew past the cap: {} bytes",
        output.text.len()
    );
    assert!(output.text.contains("line-2000"), "the tail was not kept");
}

#[tokio::test]
async fn clearing_stops_everything_it_forgets() {
    let (tasks, _dir, root) = host(BackgroundLimits::default());
    let id = tasks.spawn("sleep 30".to_string(), &root).expect("spawn");
    tasks.clear();

    assert!(tasks.snapshot(&id).is_none());
    assert!(tasks.snapshots().is_empty());
}

/// The point of one id namespace: a caller uses the same verb whatever kind of
/// work the id names, and the dispatcher finds the source that claims it.
#[tokio::test]
async fn a_source_only_answers_for_the_ids_it_claims() {
    use crate::TaskSources;

    struct Agents;
    impl TaskSource for Agents {
        fn owns(&self, id: &str) -> bool {
            id.starts_with("agent_")
        }
        fn snapshots(&self) -> Vec<crate::TaskSnapshot> {
            vec![crate::TaskSnapshot {
                id: "agent_1".to_string(),
                kind: "subagent",
                description: "look something up".to_string(),
                state: TaskState::Running,
            }]
        }
        fn snapshot(&self, id: &str) -> Option<crate::TaskSnapshot> {
            self.snapshots().into_iter().find(|row| row.id == id)
        }
        fn take_output(&self, _id: &str) -> Option<crate::TaskOutput> {
            None
        }
        fn kill(&self, _id: &str) -> bool {
            true
        }
    }

    let (tasks, _dir, root) = host(BackgroundLimits::default());
    let id = tasks.spawn("sleep 30".to_string(), &root).expect("spawn");
    let sources = TaskSources::new(vec![
        Arc::clone(&tasks) as Arc<dyn TaskSource>,
        Arc::new(Agents) as Arc<dyn TaskSource>,
    ]);

    assert!(sources.find(&id).is_some());
    assert!(sources.find("agent_1").is_some());
    assert!(sources.find("nothing_7").is_none());

    // Both kinds show up in one list, which is what makes one `list_tasks`
    // enough.
    let kinds: Vec<&str> = sources.snapshots().iter().map(|row| row.kind).collect();
    assert_eq!(kinds, vec!["command", "subagent"]);

    tasks.kill(&id);
    settle(&tasks, &id).await;
}
