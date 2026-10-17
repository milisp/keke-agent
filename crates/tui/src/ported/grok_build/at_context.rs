//! Ported from
//! `../grok-build/crates/codegen/xai-grok-pager/src/views/file_search/context.rs`.
//! See `THIRD_PARTY_NOTICES.md`. Changes from upstream: `pub` -> `pub(crate)`,
//! and `detect_with_drill` is folded into `detect` without its drill-prefix
//! parameter — keke has no drill-into-directory state to supply one, so the
//! only caller ever passed `None`. keke-tui runs `@`-detection per input line
//! rather than over a whole document, but the API here is already
//! line-agnostic (plain `&str` + a byte cursor), so no other adaptation was
//! needed.
//!
//! @-context detection: parses `@query` tokens from prompt text + cursor position.
//!
//! Given the prompt text and cursor position, determines whether the cursor is
//! inside an `@`-token and extracts the query string for fuzzy matching.
//!
//! ## Rules
//!
//! - The `@` must NOT be preceded by an alphanumeric character or underscore
//!   (avoids triggering on email addresses like `user@example.com`).
//! - The token extends from `@` to the first whitespace, comma, or semicolon.
//! - The cursor must be within the token range.
//! - The query is the text between `@` (exclusive) and the cursor.
//!
//! ## Special modes
//!
//! - **Dir mode**: query ends with `/` → restrict matches to directories only.
//! - **Hidden mode**: query starts with `!` → show hidden/gitignored files.

use std::ops::Range;

/// Context for the current @-completion token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AtContext {
    /// Byte range in the input text (includes the `@` as the first character).
    pub(crate) range: Range<usize>,
    /// Cursor byte position within the input text.
    pub(crate) cursor: usize,
    /// Query string: text after `@` (and after `!` if hidden mode) up to cursor.
    pub(crate) query: String,
}

impl AtContext {
    /// Whether the query requests directory-only results (ends with `/`).
    pub(crate) fn is_dir_mode(&self) -> bool {
        self.query.ends_with('/')
    }

    /// Whether the query requests hidden/gitignored files (starts with `!`).
    pub(crate) fn is_hidden_mode(&self) -> bool {
        self.query.starts_with('!')
    }

    /// The effective query for the fuzzy matcher (strips leading `!`).
    pub(crate) fn matcher_query(&self) -> &str {
        self.query.strip_prefix('!').unwrap_or(&self.query)
    }

    /// Byte range covering only the path portion of the @-token: starts
    /// after the leading `@` and (in hidden mode) the `!` prefix, ends at
    /// the @-token end. This is the range that should be replaced when
    /// inserting a path while preserving the `@` and any hidden-mode
    /// marker (see `accept_file_search_result_no_space` and
    /// `FileSearchState::try_replace`).
    pub(crate) fn path_range(&self) -> Range<usize> {
        let prefix = 1 + if self.is_hidden_mode() { 1 } else { 0 };
        self.range.start + prefix..self.range.end
    }
}

/// Detect an @-completion context from prompt text and cursor position.
///
/// Returns `None` if the cursor is not inside an @-token, or if the `@` is
/// preceded by an alphanumeric/underscore character (e.g., `email@`).
pub(crate) fn detect(text: &str, cursor: usize) -> Option<AtContext> {
    // Cursor must be within text bounds and on a char boundary.
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }

    // Find the rightmost `@` before the cursor.
    let at_idx = text[..cursor].rfind('@')?;

    // Reject if `@` is preceded by alphanumeric or underscore (email-like).
    if let Some(ch) = text[..at_idx].chars().next_back()
        && (ch.is_alphanumeric() || ch == '_')
    {
        return None;
    }

    // Find the end of the @-token: first whitespace, comma, or semicolon after `@`.
    let token_end = text[at_idx + 1..]
        .char_indices()
        .find_map(|(offset, ch)| {
            let abs = at_idx + 1 + offset;
            if ch.is_whitespace() || matches!(ch, ',' | ';') {
                Some(abs)
            } else {
                None
            }
        })
        .unwrap_or(text.len());

    // Cursor must be within the @-token.
    if cursor > token_end {
        return None;
    }

    Some(AtContext {
        range: at_idx..token_end,
        cursor,
        query: text[at_idx + 1..cursor].to_owned(),
    })
}

/// Normalize a display path (strip leading `./`).
pub(crate) fn normalize_display_path(path: &str) -> &str {
    path.strip_prefix("./").unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_at_token() {
        let ctx = detect("@foo", 4).unwrap();
        assert_eq!(ctx.range, 0..4);
        assert_eq!(ctx.query, "foo");
        assert!(!ctx.is_dir_mode());
        assert!(!ctx.is_hidden_mode());
    }

    #[test]
    fn at_with_prefix_text() {
        let ctx = detect("hello @bar/baz", 14).unwrap();
        assert_eq!(ctx.range, 6..14);
        assert_eq!(ctx.query, "bar/baz");
    }

    #[test]
    fn cursor_mid_token() {
        let ctx = detect("@foo/bar", 5).unwrap();
        assert_eq!(ctx.range, 0..8);
        assert_eq!(ctx.query, "foo/");
        assert!(ctx.is_dir_mode());
    }

    #[test]
    fn cursor_at_sign_only() {
        let ctx = detect("@", 1).unwrap();
        assert_eq!(ctx.range, 0..1);
        assert_eq!(ctx.query, "");
    }

    #[test]
    fn rejected_email_like() {
        // @ preceded by alphanumeric — should not trigger.
        assert!(detect("user@example", 12).is_none());
        assert!(detect("test_@foo", 9).is_none());
    }

    #[test]
    fn cursor_past_token() {
        // Cursor is after the space following the token — no match.
        assert!(detect("@foo bar", 5).is_none());
        assert!(detect("@foo bar", 8).is_none());
    }

    #[test]
    fn hidden_mode() {
        let ctx = detect("@!foo", 5).unwrap();
        assert!(ctx.is_hidden_mode());
        assert_eq!(ctx.matcher_query(), "foo");
    }

    #[test]
    fn dir_mode() {
        let ctx = detect("@src/", 5).unwrap();
        assert!(ctx.is_dir_mode());
        assert_eq!(ctx.query, "src/");
        assert_eq!(ctx.matcher_query(), "src/");
    }

    #[test]
    fn hidden_dir_mode() {
        let ctx = detect("@!.config/", 10).unwrap();
        assert!(ctx.is_hidden_mode());
        assert!(ctx.is_dir_mode());
        assert_eq!(ctx.matcher_query(), ".config/");
    }

    #[test]
    fn multiple_at_picks_rightmost() {
        let ctx = detect("@first @second", 14).unwrap();
        assert_eq!(ctx.query, "second");
        assert_eq!(ctx.range, 7..14);
    }

    #[test]
    fn at_after_special_chars() {
        // @ preceded by space, parens, etc. — should trigger.
        assert!(detect("(@foo", 5).is_some());
        assert!(detect(" @foo", 5).is_some());
        assert!(detect(",@foo", 5).is_some());
    }

    #[test]
    fn empty_text() {
        assert!(detect("", 0).is_none());
    }

    #[test]
    fn cursor_at_zero() {
        assert!(detect("@foo", 0).is_none());
    }

    #[test]
    fn normalize_path() {
        assert_eq!(normalize_display_path("./foo/bar"), "foo/bar");
        assert_eq!(normalize_display_path("foo/bar"), "foo/bar");
        assert_eq!(normalize_display_path("./"), "");
    }

    #[test]
    fn token_delimited_by_comma() {
        let ctx = detect("@foo,@bar", 4).unwrap();
        assert_eq!(ctx.range, 0..4);
        assert_eq!(ctx.query, "foo");
    }

    #[test]
    fn token_delimited_by_semicolon() {
        let ctx = detect("@foo;rest", 4).unwrap();
        assert_eq!(ctx.range, 0..4);
        assert_eq!(ctx.query, "foo");
    }

    #[test]
    fn path_range_skips_at_only() {
        // Plain @-token: path_range starts after `@`, ends at token end.
        let ctx = detect("@src/foo", 8).unwrap();
        assert_eq!(ctx.range, 0..8);
        assert_eq!(ctx.path_range(), 1..8);
    }

    #[test]
    fn path_range_skips_at_and_bang_in_hidden_mode() {
        // Hidden mode: path_range skips both `@` and `!`.
        let ctx = detect("@!src/foo", 9).unwrap();
        assert!(ctx.is_hidden_mode());
        assert_eq!(ctx.range, 0..9);
        assert_eq!(ctx.path_range(), 2..9);
    }

    #[test]
    fn path_range_with_prefix_text_offset() {
        // @-token preceded by other text: path_range respects the
        // absolute offset of the @ in the input.
        let ctx = detect("hello @bar", 10).unwrap();
        assert_eq!(ctx.range, 6..10);
        assert_eq!(ctx.path_range(), 7..10);
    }
}
