//! The multi-line prompt editor.
//!
//! Lines are held as a `Vec<String>` with a `(row, column)` cursor measured in
//! characters. Bytes would put the cursor inside a codepoint the first time
//! someone pastes a path with an accent in it. The terminal is told where the
//! cursor is in cells instead: see [`InputBox::cursor_display`].

use unicode_width::UnicodeWidthStr as _;

/// A multi-line text buffer with a cursor.
#[derive(Debug, Default)]
pub struct InputBox {
    lines: Vec<String>,
    row: usize,
    column: usize,
}

impl InputBox {
    pub fn lines(&self) -> &[String] {
        if self.lines.is_empty() {
            std::slice::from_ref(&EMPTY)
        } else {
            &self.lines
        }
    }

    /// Cursor as `(row, column)` in characters.
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.column)
    }

    /// Cursor as `(row, column)` in terminal cells.
    ///
    /// A cell is not a character: a CJK character is one character and two
    /// cells wide, so a cursor placed by character index lands inside the text
    /// rather than after it. The terminal draws in cells, so the position
    /// handed to it is measured in cells.
    pub fn cursor_display(&self) -> (usize, usize) {
        let line = self.line();
        let at = byte_index(line, self.column);
        (self.row, line[..at].width())
    }

    /// Insert pasted text, honouring the newlines in it.
    ///
    /// A paste is one edit, not a key per character: "\r\n" and a lone "\r"
    /// both mean a line break here, never a submit.
    pub fn insert_str(&mut self, text: &str) {
        let mut rest = text;
        while let Some(at) = rest.find(['\n', '\r']) {
            self.insert_text(&rest[..at]);
            self.insert_newline();
            let skipped = if rest[at..].starts_with("\r\n") {
                at + 2
            } else {
                at + 1
            };
            rest = &rest[skipped..];
        }
        self.insert_text(rest);
    }

    /// Insert a run with no line breaks in it at the cursor.
    fn insert_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let column = self.column;
        let line = self.line_mut();
        let at = byte_index(line, column);
        line.insert_str(at, text);
        self.column += text.chars().count();
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(String::is_empty)
    }

    pub fn insert_char(&mut self, ch: char) {
        let column = self.column;
        let line = self.line_mut();
        let at = byte_index(line, column);
        line.insert(at, ch);
        self.column += 1;
    }

    pub fn insert_newline(&mut self) {
        let column = self.column;
        let line = self.line_mut();
        let at = byte_index(line, column);
        let tail = line.split_off(at);
        self.lines.insert(self.row + 1, tail);
        self.row += 1;
        self.column = 0;
    }

    pub fn backspace(&mut self) {
        if self.column > 0 {
            let column = self.column - 1;
            let line = self.line_mut();
            let at = byte_index(line, column);
            line.remove(at);
            self.column -= 1;
        } else if self.row > 0 {
            let tail = self.lines.remove(self.row);
            self.row -= 1;
            self.column = self.lines[self.row].chars().count();
            self.lines[self.row].push_str(&tail);
        }
    }

    pub fn delete(&mut self) {
        let width = self.line().chars().count();
        if self.column < width {
            let column = self.column;
            let line = self.line_mut();
            let at = byte_index(line, column);
            line.remove(at);
        } else if self.row + 1 < self.lines.len() {
            let tail = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&tail);
        }
    }

    pub fn move_left(&mut self) {
        if self.column > 0 {
            self.column -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.column = self.line().chars().count();
        }
    }

    pub fn move_right(&mut self) {
        if self.column < self.line().chars().count() {
            self.column += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.column = 0;
        }
    }

    pub fn move_up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.column = self.column.min(self.line().chars().count());
        }
    }

    pub fn move_down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.column = self.column.min(self.line().chars().count());
        }
    }

    pub fn move_home(&mut self) {
        self.column = 0;
    }

    pub fn move_end(&mut self) {
        self.column = self.line().chars().count();
    }

    /// Delete from the cursor back to the start of the line.
    pub fn kill_to_start(&mut self) {
        let column = self.column;
        let line = self.line_mut();
        let at = byte_index(line, column);
        line.replace_range(..at, "");
        self.column = 0;
    }

    /// Delete from the cursor to the end of the line.
    pub fn kill_to_end(&mut self) {
        let column = self.column;
        let line = self.line_mut();
        let at = byte_index(line, column);
        line.truncate(at);
    }

    /// Delete the word before the cursor, along with the spaces leading to it.
    ///
    /// Whitespace first, then the word: Ctrl-W at the end of `git commit ` has
    /// to eat `commit`, not just the trailing space.
    pub fn delete_word_before(&mut self) {
        while self.column > 0 && self.char_before().is_some_and(char::is_whitespace) {
            self.backspace();
        }
        while self.column > 0 && self.char_before().is_some_and(|ch| !ch.is_whitespace()) {
            self.backspace();
        }
    }

    fn char_before(&self) -> Option<char> {
        self.line().chars().nth(self.column.checked_sub(1)?)
    }

    /// Take the text and reset, so a submitted prompt cannot be sent twice.
    pub fn take(&mut self) -> String {
        let text = self.text();
        self.clear();
        text
    }

    /// Replace the whole buffer and put the cursor at the end, where somebody
    /// recalling a past prompt expects to carry on typing from.
    pub fn set_text(&mut self, text: &str) {
        self.lines = text.lines().map(str::to_string).collect();
        if text.ends_with('\n') || self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.row = self.lines.len() - 1;
        self.column = self.lines[self.row].chars().count();
    }

    /// How many lines the buffer holds, counting an empty buffer as one.
    pub fn rows(&self) -> usize {
        self.lines.len().max(1)
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.row = 0;
        self.column = 0;
    }

    fn line(&self) -> &str {
        self.lines.get(self.row).map_or("", String::as_str)
    }

    fn line_mut(&mut self) -> &mut String {
        if self.lines.is_empty() {
            self.lines.push(String::new());
            self.row = 0;
        }
        &mut self.lines[self.row]
    }
}

static EMPTY: String = String::new();

fn byte_index(line: &str, column: usize) -> usize {
    line.char_indices()
        .nth(column)
        .map_or(line.len(), |(at, _)| at)
}
