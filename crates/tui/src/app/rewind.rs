//! Double-tap Esc: go back to something said earlier, and choose what to put
//! back with it.

use std::sync::Arc;
use std::time::Instant;

use keke_acp::Update;
use keke_protocol::RewindScope;

use super::App;
use crate::rewind::Phase;
use crate::rewind::Point;
use crate::rewind::Rewind;

impl App {
    /// Esc with nothing to interrupt: arm, or open the overlay if the previous
    /// Esc armed it recently enough.
    ///
    /// Two taps rather than one because Esc is the key people press to mean
    /// "never mind", and a conversation that wound itself back on a stray
    /// press would be one nobody dared press it in.
    pub(crate) fn tap_escape(&mut self) {
        let armed = self
            .esc_armed
            .take()
            .is_some_and(|at| at.elapsed() < crate::rewind::ARM);
        if armed {
            self.open_rewind();
            return;
        }
        if !self.transcript.has_user_message() {
            return;
        }
        self.esc_armed = Some(Instant::now());
        self.set_flash("press esc again to go back to an earlier message");
    }

    /// Ask the agent where this conversation can be wound back to.
    ///
    /// Asked rather than read off the transcript: whether keke still holds a
    /// snapshot of the working tree from before a turn wrote is something only
    /// the agent knows, and a surface that guessed would offer to put files
    /// back that it cannot.
    fn open_rewind(&mut self) {
        self.rewind = Some(Rewind::loading());
        let conversation = Arc::clone(&self.conversation);
        let local = self.local.clone();
        tokio::spawn(async move {
            let update = match conversation.rewind_points().await {
                Ok(points) => Update::RewindPoints(points),
                Err(error) => Update::Failed(error.to_string()),
            };
            let _ = local.send(update);
        });
    }

    /// The open rewind overlay, while there is one.
    pub fn rewind(&self) -> Option<&Rewind> {
        self.rewind.as_ref()
    }

    /// Fill the overlay in with what the agent answered.
    pub(crate) fn offer_rewind_points(&mut self, points: Vec<keke_acp::RewindPoint>) {
        let Some(rewind) = self.rewind.as_mut() else {
            return;
        };
        let points = points
            .into_iter()
            .map(|point| Point {
                turn: point.turn,
                text: point.prompt,
                has_snapshot: point.has_snapshot,
            })
            .collect();
        if !rewind.offer(points) {
            self.rewind = None;
            self.set_flash("nothing to go back to yet");
        }
    }

    /// What the agent says a restore to the point being confirmed would touch.
    pub(crate) fn preview_rewind(&mut self, turn: usize, files: Vec<String>) {
        if let Some(rewind) = self.rewind.as_mut() {
            rewind.preview(turn, files);
        }
    }

    pub(crate) fn move_rewind_selection(&mut self, delta: isize) {
        if let Some(rewind) = self.rewind.as_mut() {
            rewind.move_selection(delta);
        }
    }

    /// Esc: back out of the confirm step, or close the overlay if that is
    /// where it already was.
    pub(crate) fn cancel_rewind(&mut self) {
        let backed_out = self
            .rewind
            .as_mut()
            .is_some_and(crate::rewind::Rewind::back_to_picking);
        if !backed_out {
            self.rewind = None;
        }
    }

    /// Enter: choose this prompt, then choose what to put back.
    pub(crate) fn advance_rewind(&mut self) {
        let Some(rewind) = self.rewind.as_mut() else {
            return;
        };
        match rewind.phase() {
            Phase::Loading => {}
            Phase::Picking { .. } => {
                rewind.confirm_point();
                let Some(turn) = rewind.point().map(|point| point.turn) else {
                    return;
                };
                // Asked now rather than when the list was drawn: it is a diff
                // against the working tree, and running one per row would
                // spend the cost on every point nobody chose.
                let conversation = Arc::clone(&self.conversation);
                let local = self.local.clone();
                tokio::spawn(async move {
                    if let Ok(files) = conversation.changed_since(turn).await {
                        let _ = local.send(Update::RewindPreview { turn, files });
                    }
                });
            }
            Phase::Confirming { .. } => self.confirm_rewind(),
        }
    }

    /// Carry out the rewind the overlay is on.
    ///
    /// The transcript is cut here and the agent is told separately, because
    /// only the agent can forget — and only the agent holds the snapshots, so
    /// what happens to the files is reported back rather than assumed.
    ///
    /// Spawned for the same reason `submit` is: a slow agent must not stop the
    /// surface from redrawing. The prompt goes back in the composer now rather
    /// than when the agent answers, since it is already on screen and the
    /// person is waiting to type into it.
    fn confirm_rewind(&mut self) {
        let Some((point, scope)) = self
            .rewind
            .as_ref()
            .and_then(crate::rewind::Rewind::decision)
        else {
            return;
        };
        self.rewind = None;

        if scope.touches_conversation() {
            if let Some((cell, _)) = self.transcript.user_prompts().get(point.turn) {
                self.transcript.truncate(*cell);
            }
            self.input.set_text(&point.text);
            self.input.move_end();
            self.scroll.follow();
            self.rewound_at = Some(Instant::now());
        }
        self.set_flash(match scope {
            RewindScope::Conversation => "wound back — edit it and press enter to ask again",
            RewindScope::Files => "putting the files back\u{2026}",
            RewindScope::Both => "wound back — putting the files back too\u{2026}",
        });

        let conversation = Arc::clone(&self.conversation);
        let local = self.local.clone();
        tokio::spawn(async move {
            let update = match conversation.rewind(point.turn, scope).await {
                Ok(Some(rewound)) => Update::Rewound(rewound),
                // The turn is gone from under the overlay — nothing was
                // changed, and nothing to report.
                Ok(None) => return,
                Err(error) => Update::Failed(error.to_string()),
            };
            let _ = local.send(update);
        });
    }

    /// Say what the rewind did to the files, now that the agent has.
    pub(crate) fn report_rewind(&mut self, rewound: &keke_acp::Rewound) {
        let files = rewound.restored_files.len();
        if files == 0 {
            return;
        }
        self.set_flash(format!(
            "put back {files} file{}",
            if files == 1 { "" } else { "s" }
        ));
    }

    /// Whether Enter is still the key that just carried out a rewind rather
    /// than the key that sends the composer.
    pub(crate) fn just_rewound(&mut self) -> bool {
        let fresh = self
            .rewound_at
            .is_some_and(|at| at.elapsed() < crate::rewind::HANDBACK);
        if !fresh {
            self.rewound_at = None;
        }
        fresh
    }

    /// Forget a first Esc, because something else was pressed after it.
    pub(crate) fn disarm_escape(&mut self) {
        self.esc_armed = None;
    }
}
