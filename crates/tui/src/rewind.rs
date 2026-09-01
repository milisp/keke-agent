//! Going back to something the person said earlier.
//!
//! A conversation is a record of what was asked, and asking the wrong thing is
//! ordinary. Rewinding takes a prompt back — with the turn it started, and
//! everything that turn did — and hands the words back to be edited. It is not
//! a view filter: the agent forgets too, through
//! [`keke_acp::Conversation::rewind`], or the next answer would be given
//! against the very message a person had just withdrawn.

use std::time::Duration;

/// How long the first Esc stays armed, waiting for the second.
///
/// Long enough for a deliberate double-tap on a slow keyboard, short enough
/// that an Esc pressed now and another a minute later are two separate
/// intentions rather than one gesture.
pub const ARM: Duration = Duration::from_millis(1_000);

/// One prompt the conversation can be wound back to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Point {
    /// Which transcript cell holds it. The truncation cuts here, so everything
    /// this prompt led to goes with it.
    pub cell: usize,
    /// Which user turn it is, counting from zero — what the seam is told. Not
    /// the same number as `cell`: the transcript also holds what the agent
    /// said, and the banner above all of it.
    pub turn: usize,
    pub text: String,
}

/// The open overlay: where a person can go back to, and which row they are on.
#[derive(Debug)]
pub struct Rewind {
    /// Newest first, which is where a person almost always means to go: the
    /// thing just said that came out wrong.
    points: Vec<Point>,
    selected: usize,
}

impl Rewind {
    /// Open on `points`, newest first. `None` when there is nothing to go back
    /// to — an overlay listing no destination is a dead end, not a choice.
    #[must_use]
    pub fn open(mut points: Vec<Point>) -> Option<Self> {
        if points.is_empty() {
            return None;
        }
        points.reverse();
        Some(Self {
            points,
            selected: 0,
        })
    }

    #[must_use]
    pub fn points(&self) -> &[Point] {
        &self.points
    }

    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The row the keyboard is on. Never `None` while the overlay is open,
    /// since it cannot open on an empty list.
    #[must_use]
    pub fn selection(&self) -> Option<&Point> {
        self.points.get(self.selected)
    }

    /// Move by `delta`, wrapping — the list is short, and a person who has
    /// walked to one end means to reach the other.
    pub fn move_selection(&mut self, delta: isize) {
        let count = self.points.len() as isize;
        if count == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(count) as usize;
    }
}
