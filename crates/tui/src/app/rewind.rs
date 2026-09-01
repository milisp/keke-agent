//! Double-tap Esc: go back to something said earlier and say it differently.

use std::sync::Arc;
use std::time::Instant;

use keke_acp::Update;

use super::App;
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

    /// Forget a first Esc, because something else was pressed after it.
    pub(crate) fn disarm_escape(&mut self) {
        self.esc_armed = None;
    }

    fn open_rewind(&mut self) {
        let points = self
            .transcript
            .user_prompts()
            .into_iter()
            .enumerate()
            .map(|(turn, (cell, text))| Point { cell, turn, text })
            .collect();
        self.rewind = Rewind::open(points);
    }

    /// The open rewind overlay, while there is one.
    pub fn rewind(&self) -> Option<&Rewind> {
        self.rewind.as_ref()
    }

    pub(crate) fn move_rewind_selection(&mut self, delta: isize) {
        if let Some(rewind) = self.rewind.as_mut() {
            rewind.move_selection(delta);
        }
    }

    pub(crate) fn cancel_rewind(&mut self) {
        self.rewind = None;
    }

    /// Wind the conversation back to the highlighted prompt.
    ///
    /// The transcript is cut here and the agent is told separately, because
    /// only the agent can forget: a surface that cut its own scrollback and
    /// stopped there would draw a conversation the next turn would still be
    /// answered against.
    ///
    /// Spawned for the same reason `submit` is — a slow agent must not stop
    /// the surface from redrawing — and the prompt goes back in the composer
    /// now rather than when the agent confirms, since it is already on screen
    /// and the person is waiting to type into it.
    pub(crate) fn confirm_rewind(&mut self) {
        let Some(point) = self.rewind.take().and_then(|rw| rw.selection().cloned()) else {
            return;
        };
        self.transcript.truncate(point.cell);
        self.input.set_text(&point.text);
        self.input.move_end();
        self.scroll.follow();
        self.set_flash("wound back — edit it and press enter to ask again");

        let conversation = Arc::clone(&self.conversation);
        let local = self.local.clone();
        tokio::spawn(async move {
            if let Err(error) = conversation.rewind(point.turn).await {
                let _ = local.send(Update::Failed(error.to_string()));
            }
        });
    }
}
