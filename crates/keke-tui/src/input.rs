//! The multi-line prompt editor.
//!
//! Lines are held as a `Vec<String>` with a `(row, column)` cursor measured in
//! characters. Bytes would put the cursor inside a codepoint the first time
//! someone pastes a path with an accent in it.

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

    /// Take the text and reset, so a submitted prompt cannot be sent twice.
    pub fn take(&mut self) -> String {
        let text = self.text();
        self.clear();
        text
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
