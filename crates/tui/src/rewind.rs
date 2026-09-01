//! Going back to something the person said earlier.
//!
//! A conversation is a record of what was asked, and asking the wrong thing is
//! ordinary. Rewinding takes a prompt back and hands the words over to be
//! edited — and, because a turn is not only words, it can take back what that
//! turn did to the files as well.
//!
//! Those two are offered separately because they are separately useful. Fixing
//! a typo in a prompt means the conversation; an agent that made a mess of the
//! tree may be worth undoing *while keeping* the discussion of how it happened.
//! keke cannot infer which one somebody means, so it asks — once, on a list of
//! three, with the ones that would do nothing marked as such.

use std::time::Duration;

use keke_protocol::RewindScope;

/// How long the first Esc stays armed, waiting for the second.
///
/// Long enough for a deliberate double-tap on a slow keyboard, short enough
/// that an Esc pressed now and another a minute later are two separate
/// intentions rather than one gesture.
pub const ARM: Duration = Duration::from_millis(1_000);

/// How long after a rewind hands the words back that Enter still belongs to
/// the rewind rather than to the composer.
///
/// Short enough to be invisible to someone who read the prompt before sending
/// it again, long enough to swallow a repeat of the very keystroke that closed
/// the overlay.
pub const HANDBACK: Duration = Duration::from_millis(400);

/// One prompt the conversation can be wound back to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Point {
    /// Which user turn it is, counting from zero. What the seam is told.
    pub turn: usize,
    pub text: String,
    /// Whether the agent holds a snapshot of the tree from before this turn
    /// wrote. False for a turn that only talked, and for every turn of a
    /// session running with checkpoints off.
    pub has_snapshot: bool,
}

/// What the overlay is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Waiting for the agent to say where it can go back to. Its own phase
    /// because the answer crosses the seam: the list is drawn before it
    /// arrives, and a person must not be shown an empty one that then fills.
    Loading,
    /// Choosing which prompt to go back to.
    Picking { selected: usize },
    /// Choosing what to put back.
    Confirming {
        /// Index into `points`, not a turn number.
        at: usize,
        selected: usize,
        /// What a restore would put back. `None` while the agent is still
        /// being asked.
        changed: Option<Vec<String>>,
    },
}

/// One line of the confirm step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub scope: RewindScope,
    pub label: &'static str,
    /// Why this one would do nothing, when it would. A disabled row is still
    /// drawn: a person looking for "restore the files" needs to see that keke
    /// knows about it and cannot do it here, rather than not see it at all.
    pub unavailable: Option<&'static str>,
}

/// The open overlay.
#[derive(Debug)]
pub struct Rewind {
    /// Newest first, which is where a person almost always means to go: the
    /// thing just said that came out wrong.
    points: Vec<Point>,
    phase: Phase,
}

impl Rewind {
    /// Open, with nothing to show yet.
    #[must_use]
    pub fn loading() -> Self {
        Self {
            points: Vec::new(),
            phase: Phase::Loading,
        }
    }

    /// Fill in what the agent answered, newest first. `false` when there is
    /// nothing to go back to — an overlay listing no destination is a dead
    /// end, not a choice, and the caller closes it.
    pub fn offer(&mut self, mut points: Vec<Point>) -> bool {
        if points.is_empty() {
            return false;
        }
        points.reverse();
        self.points = points;
        self.phase = Phase::Picking { selected: 0 };
        true
    }

    #[must_use]
    pub fn points(&self) -> &[Point] {
        &self.points
    }

    #[must_use]
    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    /// The row the keyboard is on, in whichever phase is open.
    #[must_use]
    pub fn selected(&self) -> usize {
        match self.phase {
            Phase::Loading => 0,
            Phase::Picking { selected } | Phase::Confirming { selected, .. } => selected,
        }
    }

    /// The prompt being gone back to, once one has been chosen.
    #[must_use]
    pub fn point(&self) -> Option<&Point> {
        match self.phase {
            Phase::Picking { selected } => self.points.get(selected),
            Phase::Confirming { at, .. } => self.points.get(at),
            Phase::Loading => None,
        }
    }

    /// Move by `delta`, wrapping — the lists are short, and a person who has
    /// walked to one end means to reach the other. Disabled choices are
    /// stepped over rather than landed on.
    pub fn move_selection(&mut self, delta: isize) {
        let count = match self.phase {
            Phase::Loading => return,
            Phase::Picking { .. } => self.points.len(),
            Phase::Confirming { .. } => self.choices().len(),
        };
        if count == 0 {
            return;
        }
        let choices = self.choices();
        let step = if delta >= 0 { 1 } else { -1 };
        let mut selected = self.selected() as isize;
        // At most one lap: if every row is disabled the highlight stays put
        // rather than spinning.
        for _ in 0..count {
            selected = (selected + step).rem_euclid(count as isize);
            let at = selected as usize;
            let usable = match self.phase {
                Phase::Confirming { .. } => {
                    choices.get(at).is_some_and(|row| row.unavailable.is_none())
                }
                _ => true,
            };
            if usable {
                break;
            }
        }
        let selected = selected as usize;
        match &mut self.phase {
            Phase::Picking { selected: at } | Phase::Confirming { selected: at, .. } => {
                *at = selected;
            }
            Phase::Loading => {}
        }
    }

    /// Move from choosing a prompt to choosing what to put back.
    ///
    /// The highlight lands on the first choice that would actually do
    /// something, so Esc Esc Enter Enter is the whole gesture for the common
    /// case and never silently picks a row that does nothing.
    pub fn confirm_point(&mut self) {
        let Phase::Picking { selected } = self.phase else {
            return;
        };
        if self.points.get(selected).is_none() {
            return;
        }
        self.phase = Phase::Confirming {
            at: selected,
            selected: 0,
            changed: None,
        };
        let first = self
            .choices()
            .iter()
            .position(|row| row.unavailable.is_none())
            .unwrap_or(0);
        if let Phase::Confirming { selected, .. } = &mut self.phase {
            *selected = first;
        }
    }

    /// Back out of the confirm step to the list of prompts.
    ///
    /// Returns whether there was one to back out of, so Esc can close the
    /// whole overlay when there was not.
    pub fn back_to_picking(&mut self) -> bool {
        let Phase::Confirming { at, .. } = self.phase else {
            return false;
        };
        self.phase = Phase::Picking { selected: at };
        true
    }

    /// Record what the agent said a restore would put back.
    ///
    /// Ignored unless it is about the point being confirmed: an answer for a
    /// row somebody has already moved off would describe a different restore
    /// than the one they are reading about.
    pub fn preview(&mut self, turn: usize, files: Vec<String>) {
        let Phase::Confirming { at, .. } = self.phase else {
            return;
        };
        if self.points.get(at).map(|point| point.turn) != Some(turn) {
            return;
        }
        if let Phase::Confirming { changed, .. } = &mut self.phase {
            *changed = Some(files);
        }
        // What is available changed with it, so the highlight may now be on a
        // row that does nothing.
        let first = self
            .choices()
            .iter()
            .position(|row| row.unavailable.is_none())
            .unwrap_or(0);
        if let Phase::Confirming {
            selected, changed, ..
        } = &mut self.phase
        {
            let disabled = changed.as_ref().is_some_and(Vec::is_empty);
            if disabled && *selected != 0 {
                *selected = first;
            }
        }
    }

    /// How many files a restore would put back, once the agent has said.
    #[must_use]
    pub fn changed(&self) -> Option<&[String]> {
        match &self.phase {
            Phase::Confirming { changed, .. } => changed.as_deref(),
            _ => None,
        }
    }

    /// The three things a person can ask for, and which of them would do
    /// nothing here.
    ///
    /// Always three rows. A restore that keke cannot perform is shown with the
    /// reason rather than hidden: a missing option reads as keke not having
    /// the feature, which is a worse answer than "nothing to put back".
    #[must_use]
    pub fn choices(&self) -> Vec<Choice> {
        let point = self.point();
        let no_snapshot = point.is_some_and(|point| !point.has_snapshot);
        let nothing_changed = matches!(self.changed(), Some(files) if files.is_empty());
        let files_do_nothing = if no_snapshot {
            Some("no snapshot — this turn changed no files")
        } else if nothing_changed {
            Some("nothing to put back")
        } else {
            None
        };
        vec![
            Choice {
                scope: RewindScope::Conversation,
                label: "conversation only",
                unavailable: None,
            },
            Choice {
                scope: RewindScope::Files,
                label: "files only",
                unavailable: files_do_nothing,
            },
            Choice {
                scope: RewindScope::Both,
                label: "conversation and files",
                unavailable: files_do_nothing,
            },
        ]
    }

    /// What Enter would do right now: the point, and what it would put back.
    /// `None` while the highlight is on a choice that would do nothing.
    #[must_use]
    pub fn decision(&self) -> Option<(Point, RewindScope)> {
        let Phase::Confirming { selected, .. } = self.phase else {
            return None;
        };
        let choice = self.choices().into_iter().nth(selected)?;
        if choice.unavailable.is_some() {
            return None;
        }
        Some((self.point()?.clone(), choice.scope))
    }
}
