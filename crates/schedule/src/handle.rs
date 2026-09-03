//! The one scheduler, shared between the surface that fires loops and the tool
//! that creates them.
//!
//! Two owners with different clocks would be two sets of standing prompts, and
//! a person typing `/loop list` would be shown half of them. So the scheduler
//! is composed once by the composition root and handed to both — the surface
//! keeps the clock, the tool only writes records.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use keke_tasks::TaskOutput;
use keke_tasks::TaskSnapshot;
use keke_tasks::TaskSource;
use keke_tasks::TaskState;

use crate::scheduler::KIND;
use crate::scheduler::Origin;
use crate::scheduler::Scheduler;
use crate::scheduler::parse_id;

/// A handle on the session's standing prompts.
///
/// Cheap to clone; every clone is the same scheduler. The lock is only ever
/// held across a few field reads — nothing awaits under it — so a `std` mutex
/// is the right one and a poisoned lock cannot happen from code that panics
/// while holding it.
#[derive(Clone, Debug, Default)]
pub struct Schedules(Arc<Mutex<Scheduler>>);

impl Schedules {
    /// Run `f` against the scheduler.
    ///
    /// The escape hatch for the surface, which needs several operations
    /// against one consistent view — expire, then take what is due — and would
    /// otherwise take the lock twice with a gap in between.
    pub fn with<T>(&self, f: impl FnOnce(&mut Scheduler) -> T) -> T {
        f(&mut self.lock())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Scheduler> {
        // A panic under the lock leaves the scheduler as it was — every write
        // is a whole operation on a `Vec` — so the loops are worth keeping
        // rather than propagating someone else's panic into this call.
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Add a loop, returning its task id or the reason it was refused.
    pub fn add(
        &self,
        interval: Duration,
        prompt: String,
        origin: Origin,
        fire_immediately: bool,
    ) -> Result<String, String> {
        self.lock()
            .add(interval, prompt, origin, fire_immediately, Instant::now())
            .map(|id| format!("{KIND}_{id}"))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    pub fn clear(&self) {
        self.lock().clear();
    }
}

impl TaskSource for Schedules {
    fn owns(&self, id: &str) -> bool {
        id.starts_with(KIND)
    }

    fn snapshots(&self) -> Vec<TaskSnapshot> {
        self.lock().tasks().iter().map(snapshot).collect()
    }

    fn snapshot(&self, id: &str) -> Option<TaskSnapshot> {
        self.lock().get(parse_id(id)?).map(snapshot)
    }

    fn take_output(&self, id: &str) -> Option<TaskOutput> {
        let text = self.lock().take_report(parse_id(id)?, Instant::now())?;
        // Nothing is ever dropped: a report is generated at read time from a
        // counter, so there is no buffer to overflow.
        Some(TaskOutput { text, dropped: 0 })
    }

    fn kill(&self, id: &str) -> bool {
        parse_id(id).is_some_and(|id| self.lock().remove(id))
    }
}

/// A loop is always `Running`: unlike a command it has no exit, and a stopped
/// one is removed rather than kept as a finished row, because there is no last
/// output left to collect from it.
fn snapshot(task: &crate::scheduler::Task) -> TaskSnapshot {
    TaskSnapshot {
        id: task.task_id(),
        kind: KIND,
        description: task.description(),
        state: TaskState::Running,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schedules() -> Schedules {
        let schedules = Schedules::default();
        schedules
            .add(
                Duration::from_secs(300),
                "run the tests".into(),
                Origin::Model,
                false,
            )
            .unwrap();
        schedules
    }

    #[test]
    fn a_loop_is_listed_under_the_shared_task_verbs() {
        let rows = schedules().snapshots();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "loop_1");
        assert_eq!(rows[0].kind, "loop");
        assert!(rows[0].description.contains("every 5m"), "{rows:?}");
        assert!(rows[0].state.is_running());
    }

    #[test]
    fn loop_ids_are_claimed_by_nothing_else() {
        let schedules = schedules();
        assert!(schedules.owns("loop_1"));
        assert!(!schedules.owns("command_1"));
        assert!(!schedules.owns("subagent_1"));
        assert!(schedules.snapshot("command_1").is_none());
    }

    #[test]
    fn killing_a_loop_stops_it_and_saying_so_twice_is_not_an_error() {
        let schedules = schedules();
        assert!(schedules.kill("loop_1"));
        assert!(!schedules.kill("loop_1"));
        assert!(schedules.is_empty());
    }

    #[test]
    fn a_clone_is_the_same_scheduler() {
        let schedules = schedules();
        let other = schedules.clone();
        assert!(other.kill("loop_1"));
        assert!(schedules.is_empty());
    }
}
