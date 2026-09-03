//! Prompts that fire on a timer.
//!
//! `/loop 5m run the tests` is a standing instruction: the same prompt, sent
//! again every interval, until someone stops it or the session ends. The
//! scheduler here is deliberately pure — it holds records and answers "what is
//! due at this instant" — so the timing rules are tested without waiting for
//! real time to pass, and the surface keeps the one clock it already has.
//!
//! A loop never interrupts a turn. A due task waits for the agent to be idle,
//! because a standing check that lands mid-turn would be answered with half of
//! another question's context.

use std::time::Duration;
use std::time::Instant;

/// Below a minute a loop is a busy-wait, not a standing check: the agent would
/// spend the whole session answering itself.
pub const MIN_INTERVAL: Duration = Duration::from_secs(60);

/// A session that has been open a week has outlived whatever the loop was
/// watching for; it stops rather than firing into a context nobody is reading.
pub const MAX_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Enough for every plausible use, low enough that a mistyped loop in a loop
/// cannot fill the session with timers.
pub const MAX_TASKS: usize = 50;

/// The word this kind of outstanding work is listed under, and the prefix on
/// its ids. Shared with `keke-tasks` so `list_tasks` and `kill_task` reach a
/// loop by the same name a person types after `/loop stop`.
pub const KIND: &str = "loop";

/// Who asked for the loop.
///
/// Recorded because the two are answerable to different people: a person
/// reading `/loop list` needs to see the standing prompts the model gave
/// itself, or the first they hear of one is the prompt arriving.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Typed as `/loop`.
    Person,
    /// Requested by the model through `schedule_prompt`.
    Model,
}

impl Origin {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Person => "you",
            Self::Model => "the agent",
        }
    }
}

/// One standing prompt.
#[derive(Clone, Debug)]
pub struct Task {
    /// What someone types to stop it. Small and stable for the life of the
    /// session — a number a person can retype, not a UUID.
    pub id: u32,
    pub interval: Duration,
    pub prompt: String,
    pub origin: Origin,
    /// When it next becomes due. Never in the past for long: the surface
    /// re-arms it as it fires.
    next: Instant,
    /// When it was created, for the lifetime cap.
    born: Instant,
    /// How many times it has fired since its output was last taken. The model
    /// polls a loop it started the same way it polls a background command, and
    /// what it wants to know is what has happened since it last looked.
    unread_fires: u64,
    /// How many times it has fired in all, which is what a person listing
    /// loops wants instead.
    fires: u64,
}

impl Task {
    #[must_use]
    pub fn next_fire(&self) -> Instant {
        self.next
    }

    #[must_use]
    pub fn fires(&self) -> u64 {
        self.fires
    }

    /// The id as everything outside this session's keyboard writes it.
    #[must_use]
    pub fn task_id(&self) -> String {
        format!("{KIND}_{}", self.id)
    }

    /// The one-line description of a loop, for a task list or `/loop list`.
    #[must_use]
    pub fn description(&self) -> String {
        format!("every {} · {}", format_interval(self.interval), self.prompt)
    }
}

/// The session's standing prompts.
#[derive(Debug, Default)]
pub struct Scheduler {
    tasks: Vec<Task>,
    next_id: u32,
}

impl Scheduler {
    /// Add a loop. `fire_immediately` makes the first occurrence due at once,
    /// which is what a person typing `/loop` expects: they asked for the check
    /// now and every interval after, not in five minutes' time. The model gets
    /// the opposite default — it is already mid-turn, and a loop due the
    /// instant it is created would re-ask the question it just asked.
    ///
    /// Returns the id, or an error to show whoever asked.
    pub fn add(
        &mut self,
        interval: Duration,
        prompt: String,
        origin: Origin,
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
                "{MAX_TASKS} loops are already running — stop one first"
            ));
        }
        if prompt.trim().is_empty() {
            return Err("a loop needs a prompt".to_string());
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
            origin,
            next,
            born: now,
            unread_fires: 0,
            fires: 0,
        });
        Ok(id)
    }

    /// Stop one. `false` when no loop has that id.
    pub fn remove(&mut self, id: u32) -> bool {
        let before = self.tasks.len();
        self.tasks.retain(|task| task.id != id);
        self.tasks.len() != before
    }

    pub fn clear(&mut self) {
        self.tasks.clear();
    }

    #[must_use]
    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    #[must_use]
    pub fn get(&self, id: u32) -> Option<&Task> {
        self.tasks.iter().find(|task| task.id == id)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// How long until something is due, for the surface's sleep. `None`
    /// when nothing is scheduled, so an idle session with no loops still
    /// blocks rather than waking the terminal.
    #[must_use]
    pub fn until_due(&self, now: Instant) -> Option<Duration> {
        self.tasks
            .iter()
            .map(|task| task.next.saturating_duration_since(now))
            .min()
    }

    /// Drop loops that have outlived the cap, returning their ids so whoever
    /// started them is told why the prompt stopped arriving.
    pub fn expire(&mut self, now: Instant) -> Vec<u32> {
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
    pub fn take_due(&mut self, now: Instant) -> Option<(u32, String)> {
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
        task.fires += 1;
        task.unread_fires += 1;
        Some((task.id, task.prompt.clone()))
    }

    /// What has happened to one loop since this was last called, and reset the
    /// count. `None` when no loop has that id.
    ///
    /// Consuming rather than replaying, for the same reason a background
    /// command's output is: a model polling its own standing prompt wants what
    /// is new, and re-reporting every past firing spends the context window on
    /// what it already read.
    pub fn take_report(&mut self, id: u32, now: Instant) -> Option<String> {
        let task = self.tasks.iter_mut().find(|task| task.id == id)?;
        let fired = std::mem::take(&mut task.unread_fires);
        let due = task.next.saturating_duration_since(now).as_secs();
        Some(match fired {
            0 => format!("has not fired yet; next in {due}s"),
            1 => format!("fired once since the last read; next in {due}s"),
            _ => format!("fired {fired} times since the last read; next in {due}s"),
        })
    }
}

/// Parse `60s`, `5m`, `2h`, `1d` into a duration.
///
/// Only these four, and only whole numbers: an interval is a rough cadence,
/// and accepting `1.5h` invites precision the scheduler does not have.
#[must_use]
pub fn parse_interval(text: &str) -> Option<Duration> {
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

/// How an interval is written back, in the unit it was given in.
#[must_use]
pub fn format_interval(interval: Duration) -> String {
    let seconds = interval.as_secs();
    for (unit, size) in [("d", 86_400), ("h", 3_600), ("m", 60)] {
        if seconds.is_multiple_of(size) && seconds >= size {
            return format!("{}{unit}", seconds / size);
        }
    }
    format!("{seconds}s")
}

/// Read `loop_3`, or the bare `3` a person types after `/loop stop`.
///
/// Both spellings on purpose: the id in a task list is the prefixed one, and
/// the id in `/loop list` is short enough to retype. Refusing either would be
/// refusing an id keke itself printed.
#[must_use]
pub fn parse_id(text: &str) -> Option<u32> {
    let text = text.trim();
    text.strip_prefix(KIND)
        .and_then(|rest| rest.strip_prefix('_'))
        .unwrap_or(text)
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scheduler() -> (Scheduler, Instant) {
        (Scheduler::default(), Instant::now())
    }

    fn add(scheduler: &mut Scheduler, seconds: u64, prompt: &str, now: Instant) -> u32 {
        scheduler
            .add(
                Duration::from_secs(seconds),
                prompt.into(),
                Origin::Person,
                true,
                now,
            )
            .unwrap()
    }

    #[test]
    fn an_interval_under_a_minute_is_refused() {
        let (mut scheduler, now) = scheduler();
        let error = scheduler
            .add(
                Duration::from_secs(30),
                "check".into(),
                Origin::Person,
                true,
                now,
            )
            .unwrap_err();
        assert!(error.contains("60s"), "{error}");
        assert!(scheduler.is_empty());
    }

    #[test]
    fn a_loop_fires_immediately_then_after_its_interval() {
        let (mut scheduler, now) = scheduler();
        add(&mut scheduler, 300, "check", now);
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
            .add(
                Duration::from_secs(60),
                "check".into(),
                Origin::Model,
                false,
                now,
            )
            .unwrap();
        assert!(scheduler.take_due(now).is_none());
        assert!(scheduler.take_due(now + Duration::from_secs(60)).is_some());
    }

    #[test]
    fn a_missed_occurrence_is_not_fired_twice_to_catch_up() {
        let (mut scheduler, now) = scheduler();
        add(&mut scheduler, 60, "check", now);
        // Three intervals pass while a turn holds the loop up.
        let late = now + Duration::from_secs(180);
        assert!(scheduler.take_due(late).is_some());
        assert!(scheduler.take_due(late).is_none());
    }

    #[test]
    fn only_one_loop_fires_at_a_time_and_the_other_stays_due() {
        let (mut scheduler, now) = scheduler();
        add(&mut scheduler, 60, "first", now);
        add(&mut scheduler, 60, "second", now);
        assert_eq!(scheduler.take_due(now).unwrap().1, "first");
        assert_eq!(scheduler.take_due(now).unwrap().1, "second");
    }

    #[test]
    fn a_stopped_loop_stops_firing() {
        let (mut scheduler, now) = scheduler();
        let id = add(&mut scheduler, 60, "check", now);
        assert!(scheduler.remove(id));
        assert!(!scheduler.remove(id));
        assert!(scheduler.take_due(now).is_none());
    }

    #[test]
    fn a_loop_expires_after_a_week() {
        let (mut scheduler, now) = scheduler();
        let id = add(&mut scheduler, 60, "check", now);
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
            add(&mut scheduler, 60, "check", now);
        }
        assert!(
            scheduler
                .add(
                    Duration::from_secs(60),
                    "check".into(),
                    Origin::Person,
                    false,
                    now
                )
                .is_err()
        );
    }

    #[test]
    fn nothing_scheduled_means_nothing_to_wake_up_for() {
        let (mut scheduler, now) = scheduler();
        assert!(scheduler.until_due(now).is_none());
        scheduler
            .add(
                Duration::from_secs(60),
                "check".into(),
                Origin::Person,
                false,
                now,
            )
            .unwrap();
        assert_eq!(scheduler.until_due(now), Some(Duration::from_secs(60)));
    }

    #[test]
    fn a_report_covers_only_the_firings_since_the_last_one() {
        let (mut scheduler, now) = scheduler();
        let id = add(&mut scheduler, 60, "check", now);
        assert!(
            scheduler
                .take_report(id, now)
                .unwrap()
                .contains("not fired")
        );
        scheduler.take_due(now);
        scheduler.take_due(now + Duration::from_secs(60));
        let report = scheduler.take_report(id, now).unwrap();
        assert!(report.contains("fired 2 times"), "{report}");
        assert!(
            scheduler
                .take_report(id, now)
                .unwrap()
                .contains("not fired"),
            "a read is consuming"
        );
        // The total is not consumed with it: it is what a person listing loops
        // is shown.
        assert_eq!(scheduler.get(id).unwrap().fires(), 2);
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

    #[test]
    fn an_id_reads_both_the_way_it_is_listed_and_the_way_it_is_typed() {
        assert_eq!(parse_id("loop_3"), Some(3));
        assert_eq!(parse_id(" 3 "), Some(3));
        assert_eq!(parse_id("command_3"), None);
        assert_eq!(parse_id("loop_"), None);
    }
}
