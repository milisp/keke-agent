//! Prompts that fire on a timer.
//!
//! `/loop 5m run the tests` is a standing instruction: the same prompt, sent
//! again every interval, until a person stops it or the session ends. The
//! scheduler here is deliberately pure — it holds records and answers "what is
//! due at this instant" — so the timing rules are tested without waiting for
//! real time to pass, and the event loop keeps the one clock it already has.
//!
//! A loop never interrupts a turn. A due task waits for the agent to be idle,
//! because a standing check that lands mid-turn would be answered with half of
//! another question's context.

use std::time::Duration;
use std::time::Instant;

/// Below a minute a loop is a busy-wait, not a standing check: the agent would
/// spend the whole session answering itself.
pub(crate) const MIN_INTERVAL: Duration = Duration::from_secs(60);

/// A session that has been open a week has outlived whatever the loop was
/// watching for; it stops rather than firing into a context nobody is reading.
pub(crate) const MAX_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Enough for every plausible use, low enough that a mistyped loop in a loop
/// cannot fill the session with timers.
pub(crate) const MAX_TASKS: usize = 50;

/// One standing prompt.
#[derive(Clone, Debug)]
pub(crate) struct Task {
    /// What a person types to stop it. Small and stable for the life of the
    /// session — a number a person can retype, not a UUID.
    pub id: u32,
    pub interval: Duration,
    pub prompt: String,
    /// When it next becomes due. Never in the past for long: the event loop
    /// re-arms it as it fires.
    next: Instant,
    /// When it was created, for the lifetime cap.
    born: Instant,
}

impl Task {
    #[must_use]
    pub(crate) fn next_fire(&self) -> Instant {
        self.next
    }
}

/// The session's standing prompts.
#[derive(Debug, Default)]
pub(crate) struct Scheduler {
    tasks: Vec<Task>,
    next_id: u32,
}

impl Scheduler {
    /// Add a loop. `fire_immediately` makes the first occurrence due at once,
    /// which is what a person typing `/loop` expects: they asked for the check
    /// now and every interval after, not in five minutes' time.
    ///
    /// Returns the id, or an error to show the person.
    pub(crate) fn add(
        &mut self,
        interval: Duration,
        prompt: String,
        fire_immediately: bool,
        now: Instant,
    ) -> Result<u32, String> {
        if interval < MIN_INTERVAL {
            return Err(format!(
                "interval must be at least {}s",
                MIN_INTERVAL.as_secs()
            ));
        }
        if self.tasks.len() >= MAX_TASKS {
            return Err(format!(
                "{MAX_TASKS} loops are already running — /loop stop <id> first"
            ));
        }
        if prompt.trim().is_empty() {
            return Err("a loop needs a prompt — /loop <interval> <prompt>".to_string());
        }
        self.next_id += 1;
        let id = self.next_id;
        let next = if fire_immediately {
            now
        } else {
            now + interval
        };
        self.tasks.push(Task {
            id,
            interval,
            prompt,
            next,
            born: now,
        });
        Ok(id)
    }

    /// Stop one. `false` when no loop has that id.
    pub(crate) fn remove(&mut self, id: u32) -> bool {
        let before = self.tasks.len();
        self.tasks.retain(|task| task.id != id);
        self.tasks.len() != before
    }

    pub(crate) fn clear(&mut self) {
        self.tasks.clear();
    }

    #[must_use]
    pub(crate) fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// How long until something is due, for the event loop's sleep. `None`
    /// when nothing is scheduled, so an idle session with no loops still
    /// blocks rather than waking the terminal.
    #[must_use]
    pub(crate) fn until_due(&self, now: Instant) -> Option<Duration> {
        self.tasks
            .iter()
            .map(|task| task.next.saturating_duration_since(now))
            .min()
    }

    /// Drop loops that have outlived the cap, returning their ids so the
    /// person is told why the prompt stopped arriving.
    pub(crate) fn expire(&mut self, now: Instant) -> Vec<u32> {
        let expired: Vec<u32> = self
            .tasks
            .iter()
            .filter(|task| now.duration_since(task.born) >= MAX_LIFETIME)
            .map(|task| task.id)
            .collect();
        self.tasks.retain(|task| !expired.contains(&task.id));
        expired
    }

    /// Take the one loop that is due, re-arming it for the next interval.
    ///
    /// One at a time on purpose: firing two prompts at once would start two
    /// turns, and only one of them would be the one the agent answers. The
    /// other stays due and goes next.
    pub(crate) fn take_due(&mut self, now: Instant) -> Option<(u32, String)> {
        let index = self
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, task)| task.next <= now)
            .min_by_key(|(_, task)| task.next)
            .map(|(index, _)| index)?;
        let task = &mut self.tasks[index];
        // From `now`, not from the missed deadline: a loop held up by a long
        // turn should wait a full interval before asking again, not fire the
        // occurrences it slept through back to back.
        task.next = now + task.interval;
        Some((task.id, task.prompt.clone()))
    }
}

/// Parse `60s`, `5m`, `2h`, `1d` into a duration.
///
/// Only these four, and only whole numbers: an interval is a rough cadence,
/// and accepting `1.5h` invites precision the scheduler does not have.
pub(crate) fn parse_interval(text: &str) -> Option<Duration> {
    let text = text.trim();
    let (digits, unit) = text.split_at(text.len().checked_sub(1)?);
    let count: u64 = digits.parse().ok()?;
    let seconds = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        _ => return None,
    };
    Some(Duration::from_secs(count.checked_mul(seconds)?))
}

/// How an interval is written back to the person, in the unit they gave.
#[must_use]
pub(crate) fn format_interval(interval: Duration) -> String {
    let seconds = interval.as_secs();
    for (unit, size) in [("d", 86_400), ("h", 3_600), ("m", 60)] {
        if seconds.is_multiple_of(size) && seconds >= size {
            return format!("{}{unit}", seconds / size);
        }
    }
    format!("{seconds}s")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scheduler() -> (Scheduler, Instant) {
        (Scheduler::default(), Instant::now())
    }

    #[test]
    fn an_interval_under_a_minute_is_refused() {
        let (mut scheduler, now) = scheduler();
        let error = scheduler
            .add(Duration::from_secs(30), "check".into(), true, now)
            .unwrap_err();
        assert!(error.contains("60s"), "{error}");
        assert!(scheduler.is_empty());
    }

    #[test]
    fn a_loop_fires_immediately_then_after_its_interval() {
        let (mut scheduler, now) = scheduler();
        scheduler
            .add(Duration::from_secs(300), "check".into(), true, now)
            .unwrap();
        assert_eq!(scheduler.take_due(now).unwrap().1, "check");
        assert!(scheduler.take_due(now).is_none());
        assert_eq!(
            scheduler
                .take_due(now + Duration::from_secs(300))
                .unwrap()
                .1,
            "check"
        );
    }

    #[test]
    fn a_loop_that_does_not_fire_immediately_waits_out_the_first_interval() {
        let (mut scheduler, now) = scheduler();
        scheduler
            .add(Duration::from_secs(60), "check".into(), false, now)
            .unwrap();
        assert!(scheduler.take_due(now).is_none());
        assert!(scheduler.take_due(now + Duration::from_secs(60)).is_some());
    }

    #[test]
    fn a_missed_occurrence_is_not_fired_twice_to_catch_up() {
        let (mut scheduler, now) = scheduler();
        scheduler
            .add(Duration::from_secs(60), "check".into(), true, now)
            .unwrap();
        // Three intervals pass while a turn holds the loop up.
        let late = now + Duration::from_secs(180);
        assert!(scheduler.take_due(late).is_some());
        assert!(scheduler.take_due(late).is_none());
    }

    #[test]
    fn only_one_loop_fires_at_a_time_and_the_other_stays_due() {
        let (mut scheduler, now) = scheduler();
        scheduler
            .add(Duration::from_secs(60), "first".into(), true, now)
            .unwrap();
        scheduler
            .add(Duration::from_secs(60), "second".into(), true, now)
            .unwrap();
        assert_eq!(scheduler.take_due(now).unwrap().1, "first");
        assert_eq!(scheduler.take_due(now).unwrap().1, "second");
    }

    #[test]
    fn a_stopped_loop_stops_firing() {
        let (mut scheduler, now) = scheduler();
        let id = scheduler
            .add(Duration::from_secs(60), "check".into(), true, now)
            .unwrap();
        assert!(scheduler.remove(id));
        assert!(!scheduler.remove(id));
        assert!(scheduler.take_due(now).is_none());
    }

    #[test]
    fn a_loop_expires_after_a_week() {
        let (mut scheduler, now) = scheduler();
        let id = scheduler
            .add(Duration::from_secs(60), "check".into(), true, now)
            .unwrap();
        assert!(
            scheduler
                .expire(now + MAX_LIFETIME - Duration::from_secs(1))
                .is_empty()
        );
        assert_eq!(scheduler.expire(now + MAX_LIFETIME), vec![id]);
        assert!(scheduler.is_empty());
    }

    #[test]
    fn the_fifty_first_loop_is_refused() {
        let (mut scheduler, now) = scheduler();
        for _ in 0..MAX_TASKS {
            scheduler
                .add(Duration::from_secs(60), "check".into(), false, now)
                .unwrap();
        }
        assert!(
            scheduler
                .add(Duration::from_secs(60), "check".into(), false, now)
                .is_err()
        );
    }

    #[test]
    fn nothing_scheduled_means_nothing_to_wake_up_for() {
        let (mut scheduler, now) = scheduler();
        assert!(scheduler.until_due(now).is_none());
        scheduler
            .add(Duration::from_secs(60), "check".into(), false, now)
            .unwrap();
        assert_eq!(scheduler.until_due(now), Some(Duration::from_secs(60)));
    }

    #[test]
    fn intervals_round_trip_through_their_unit() {
        for text in ["90s", "5m", "2h", "1d"] {
            let interval = parse_interval(text).unwrap();
            assert_eq!(format_interval(interval), text);
        }
        assert_eq!(parse_interval("5"), None);
        assert_eq!(parse_interval("m"), None);
        assert_eq!(parse_interval("1.5h"), None);
        assert_eq!(parse_interval(""), None);
    }
}
