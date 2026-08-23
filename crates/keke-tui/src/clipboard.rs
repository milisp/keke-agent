//! Putting text on the system clipboard.
//!
//! OSC 52 rather than a clipboard crate: the terminal that owns the selection
//! is not always on the machine keke runs on, and over ssh or inside tmux a
//! native clipboard call would copy on the wrong side of the connection.
//! Every terminal keke cares about either honours the escape or ignores it.

use std::io::Write;

use base64::Engine;

/// The escape that carries `text` to the terminal's clipboard.
pub(crate) fn osc52(text: &str) -> String {
    let payload = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{payload}\x07")
}

/// Hand `text` to the terminal. Failure is silent by design: the notice the
/// person already saw is about what keke did, and a terminal that drops the
/// escape gives no way to know it did.
pub(crate) fn copy(text: &str) {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(osc52(text).as_bytes());
    let _ = stdout.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_escape_carries_the_text_base64_encoded() {
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }
}
