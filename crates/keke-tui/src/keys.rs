//! Key and mouse handling.
//!
//! Kept apart from [`crate::app`] so the bindings can be read as a table, and
//! apart from `draw` so a test can press a key without a terminal.

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use crossterm::event::MouseButton;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;
use keke_acp::PermissionAnswer;

use crate::app::App;

/// How many transcript lines one wheel notch moves.
const WHEEL_LINES: usize = 3;

/// The keys the completion menu takes over while it is open.
fn menu_key(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::Enter | KeyCode::Esc
    )
}

impl App {
    /// A bracketed paste arrives as one event, so pasted text lands as text.
    ///
    /// Without this a terminal delivers a paste as a key per character: the
    /// newlines in it submit the prompt half-typed, and an input method's
    /// characters race the redraw. Ignored while a permission prompt owns the
    /// keyboard, where the composer is not taking input anyway.
    pub fn handle_paste(&mut self, text: &str) {
        if self.open_permission_id().is_some() {
            return;
        }
        self.input.insert_str(text);
        self.sync_file_search();
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        // Windows reports press and release; acting on both double-types.
        if key.kind == KeyEventKind::Release {
            return;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        // The model overlay owns the keyboard while it is up, letters
        // included: it filters as you type, and a keystroke that went into the
        // composer behind it would be invisible until the overlay closed.
        if self.model_picker().is_some() && !control {
            self.handle_picker_key(key);
            return;
        }

        match key.code {
            KeyCode::Char('c') if control => self.interrupt(),
            // Before the interrupt: a visible overlay owns escape, the way the
            // model picker above does. A subagent popup is open exactly while
            // the turn is busy, so without this it could never be closed.
            KeyCode::Esc if self.close_subagent() => {}
            KeyCode::Esc if self.turn().is_busy() => self.interrupt(),
            KeyCode::Char('d') if control => self.quit(),
            KeyCode::Char('t') if control => self.toggle_thinking(),
            KeyCode::Char('l') if control => self.scroll.follow(),
            KeyCode::Char('o') if control => self.toggle_last_expandable(),
            KeyCode::PageUp => self.scroll.page_up(),
            KeyCode::PageDown => self.scroll.page_down(),
            // Shift+Enter is invisible to a terminal without the Kitty keyboard
            // protocol, so Alt+Enter and Ctrl-J are equal citizens, not
            // fallbacks — one of the three works everywhere.
            KeyCode::Char('j') if control => self.input.insert_newline(),
            // The readline bindings a terminal person has in every other
            // prompt. Ctrl-A/E/B/F move, Ctrl-U/K/W delete.
            KeyCode::Char('a') if control => self.input.move_home(),
            KeyCode::Char('e') if control => self.input.move_end(),
            KeyCode::Char('b') if control => self.input.move_left(),
            KeyCode::Char('f') if control => self.input.move_right(),
            KeyCode::Char('u') if control => self.input.kill_to_start(),
            KeyCode::Char('k') if control => self.input.kill_to_end(),
            KeyCode::Char('w') if control => self.input.delete_word_before(),
            // History, always, whatever the composer holds — the arrows cannot
            // be relied on for it now that the wheel arrives as arrow keys.
            KeyCode::Char('p') if control => self.recall_older(),
            KeyCode::Char('n') if control => self.recall_newer(),
            KeyCode::Enter if shift || alt => self.input.insert_newline(),
            // Shift-Tab reaches a terminal as `BackTab`, except where it does
            // not; both spellings mean the same gesture.
            KeyCode::BackTab => self.cycle_approval_policy(),
            KeyCode::Tab if shift => self.cycle_approval_policy(),
            // The completion menu owns the keys only while it is open, so
            // nothing a person can press changes meaning without them seeing
            // the list it applies to.
            _ if !self.completions().is_empty() && menu_key(key.code) => {
                self.handle_completion_key(key);
            }
            // Same rule for the `@`-file dropdown. Esc closes it even before
            // any results have arrived, which is why this checks `is_open`
            // rather than whether there is something on screen yet.
            _ if self.file_search.is_open() && menu_key(key.code) => {
                self.handle_file_search_key(key);
            }
            KeyCode::Enter => self.submit(),
            _ if self.open_permission_id().is_some() => self.handle_permission_key(key),
            KeyCode::Char(ch) if !control => self.input.insert_char(ch),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.move_left(),
            KeyCode::Right => self.input.move_right(),
            KeyCode::Up => self.move_up(),
            KeyCode::Down => self.move_down(),
            KeyCode::Home => self.input.move_home(),
            KeyCode::End => self.input.move_end(),
            _ => {}
        }
        self.sync_file_search();
    }

    /// Up moves within a multi-line prompt first and recalls a past one only
    /// from the top line, so the arrow keys never yank away text somebody is
    /// still editing further down.
    ///
    /// With nothing typed, an empty composer recalls history too — that is
    /// what most people reach for first. It only gives the arrows to the
    /// transcript instead while the mouse has been handed back with `/mouse`:
    /// a terminal not in mouse-reporting mode turns the wheel into arrow keys,
    /// and with mouse capture on (the default) that fallback never fires, so
    /// there is no wheel event for recall to steal. Ctrl-P and Ctrl-N still
    /// recall regardless, for a terminal person who already expects them.
    fn move_up(&mut self) {
        if self.input.is_empty() {
            if self.mouse_capture() {
                self.recall_older();
            } else {
                self.scroll.scroll_up(1);
            }
            return;
        }
        if self.input.cursor().0 > 0 {
            self.input.move_up();
            return;
        }
        self.recall_older();
    }

    /// Down is Up's mirror: it walks back toward the newest prompt and then to
    /// whatever draft was interrupted, but only from the last line.
    fn move_down(&mut self) {
        if self.input.is_empty() {
            if self.mouse_capture() {
                self.recall_newer();
            } else {
                self.scroll.scroll_down(1);
            }
            return;
        }
        if self.input.cursor().0 + 1 < self.input.rows() {
            self.input.move_down();
            return;
        }
        self.recall_newer();
    }

    fn recall_older(&mut self) {
        let current = self.input.text();
        if let Some(prompt) = self.history.older(&current) {
            self.input.set_text(&prompt);
        }
    }

    fn recall_newer(&mut self) {
        if let Some(prompt) = self.history.newer() {
            self.input.set_text(&prompt);
        }
    }

    /// The wheel scrolls the transcript, and the count of what is below it is
    /// a button back to the bottom: the pointer is already there when a reader
    /// decides they are done looking back.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            // Scrolling moves what the rows hold out from under a selection
            // pinned to them, so it drops it rather than keeping a highlight
            // that now marks the wrong text.
            MouseEventKind::ScrollUp => {
                self.selection.clear();
                self.scroll.scroll_up(WHEEL_LINES);
            }
            MouseEventKind::ScrollDown => {
                self.selection.clear();
                self.scroll.scroll_down(WHEEL_LINES);
            }
            // Answered on press rather than release: a subagent row is one
            // line and is not selectable text, so there is no second meaning
            // the gesture could turn out to have.
            MouseEventKind::Down(MouseButton::Left) if self.open_subagent_at(mouse.row) => {}
            MouseEventKind::Down(MouseButton::Left)
                if self.hit_follow_button(mouse.column, mouse.row) =>
            {
                self.scroll.follow();
            }
            // A press is not yet a click: what it becomes is decided on
            // release, so the same gesture can open a tool call or select the
            // line it is drawn on.
            MouseEventKind::Down(MouseButton::Left) => {
                self.selection.press((mouse.row, mouse.column));
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.selection.drag_to((mouse.row, mouse.column));
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(text) = self.selection.release() {
                    self.copy_selection(text);
                } else {
                    self.toggle_at(mouse.row);
                }
            }
            _ => {}
        }
    }

    /// While the model overlay is up, these keys drive it.
    fn handle_picker_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => self.move_picker_selection(-1),
            KeyCode::Down | KeyCode::Tab => self.move_picker_selection(1),
            KeyCode::Enter => self.accept_picker(),
            KeyCode::Esc => self.close_picker(),
            KeyCode::Backspace => self.backspace_in_picker(),
            KeyCode::Char(ch) => self.type_into_picker(ch),
            _ => {}
        }
    }

    /// While the completion menu is up, these keys drive it.
    fn handle_completion_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => self.select_previous_completion(),
            KeyCode::Down => self.select_next_completion(),
            KeyCode::Tab => self.complete(),
            // Enter runs the highlighted command rather than whatever prefix is
            // in the box: the highlight is what the person is looking at.
            KeyCode::Enter => {
                self.complete();
                self.submit();
            }
            KeyCode::Esc => self.input.clear(),
            _ => {}
        }
    }

    /// While the `@`-file dropdown is open, these keys drive it instead of
    /// editing the composer.
    fn handle_file_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => self.file_search.move_selection(-1),
            KeyCode::Down => self.file_search.move_selection(1),
            KeyCode::Tab | KeyCode::Enter => {
                if let Some(replacement) = self.file_search.accept() {
                    self.input
                        .replace_line_range(replacement.range, &replacement.text);
                }
                self.file_search.clear();
            }
            KeyCode::Esc => self.file_search.clear(),
            _ => {}
        }
    }

    /// While a prompt is open the letter keys answer it instead of typing.
    ///
    /// The turn is blocked either way, so a keystroke that silently went into
    /// the input box would look like the interface had stopped responding.
    fn handle_permission_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') => self.answer_permission(PermissionAnswer::Allow),
            KeyCode::Char('a') => self.answer_permission(PermissionAnswer::AllowAlways),
            KeyCode::Char('n') | KeyCode::Char('d') | KeyCode::Esc => {
                self.answer_permission(PermissionAnswer::Deny);
            }
            _ => {}
        }
    }
}
