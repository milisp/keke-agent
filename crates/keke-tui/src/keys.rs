//! Key and mouse handling.
//!
//! Kept apart from [`crate::app`] so the bindings can be read as a table, and
//! apart from `draw` so a test can press a key without a terminal.

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;
use keke_acp::PermissionAnswer;

use crate::app::App;

/// The keys the completion menu takes over while it is open.
fn menu_key(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::Enter | KeyCode::Esc
    )
}

/// How many transcript lines one wheel notch moves.
const WHEEL_LINES: usize = 3;

impl App {
    pub fn handle_key(&mut self, key: KeyEvent) {
        // Windows reports press and release; acting on both double-types.
        if key.kind == KeyEventKind::Release {
            return;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        match key.code {
            KeyCode::Char('c') if control => self.interrupt(),
            KeyCode::Char('d') if control => self.quit(),
            KeyCode::Char('t') if control => self.toggle_thinking(),
            KeyCode::Char('l') if control => self.scroll.follow(),
            KeyCode::PageUp => self.scroll.page_up(),
            KeyCode::PageDown => self.scroll.page_down(),
            // Shift+Enter is invisible to a terminal without the Kitty keyboard
            // protocol, so Alt+Enter and Ctrl-J are equal citizens, not
            // fallbacks — one of the three works everywhere.
            KeyCode::Char('j') if control => self.input.insert_newline(),
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
            KeyCode::Enter => self.submit(),
            _ if self.open_permission_id().is_some() => self.handle_permission_key(key),
            KeyCode::Char(ch) if !control => self.input.insert_char(ch),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.move_left(),
            KeyCode::Right => self.input.move_right(),
            KeyCode::Up => self.input.move_up(),
            KeyCode::Down => self.input.move_down(),
            KeyCode::Home => self.input.move_home(),
            KeyCode::End => self.input.move_end(),
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

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll.scroll_up(WHEEL_LINES),
            MouseEventKind::ScrollDown => self.scroll.scroll_down(WHEEL_LINES),
            _ => {}
        }
    }
}

/// The one-line binding reminder in the status bar.
pub(crate) fn hints(awaiting_permission: bool) -> &'static str {
    if awaiting_permission {
        "y allow · a always · n deny · ^C cancel"
    } else {
        "enter send · / commands · shift-tab mode · ^T thinking · ^C cancel · ^D quit"
    }
}
