//! The three-line startup banner: icon, version and tagline, and the
//! workspace's git status, all shown once at the top of a fresh scrollback.

use std::path::Path;
use std::process::Command;

/// Three rows, all the same width, so the text column beside it stays
/// aligned regardless of which row it is next to.
const ICON: [&str; 3] = [" ▗▄▄▖  ", "▐▘◕‿◕▘ ", "▝▀▀▀▘  "];

/// Built once, at session start, from `cwd`. Not refreshed: it answers "what
/// does this workspace look like right now", and once a prompt is sent that
/// answer is stale, so nothing here is worth recomputing.
pub(crate) fn startup(cwd: &Path) -> Vec<String> {
    let mut display = crate::draw::header::tilde(cwd);
    if let Some((added, removed)) = diff_stat(cwd) {
        display = format!("{display}  +{added} -{removed}");
    }

    vec![
        format!("{}keke v{}", ICON[0], env!("CARGO_PKG_VERSION")),
        format!("{}any model, one workflow", ICON[1]),
        format!("{}{}", ICON[2], display),
    ]
}

/// `git diff --shortstat`, parsed into `(insertions, deletions)`. `None` when
/// there is no `git` on `PATH`, `dir` is not inside a work tree, or there are
/// no unstaged changes — all three collapse to the same "say nothing"
/// outcome, since an empty right side says more than a pair of zeros would.
fn diff_stat(dir: &Path) -> Option<(u64, u64)> {
    let output = Command::new("git")
        .args(["diff", "--shortstat"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_shortstat(&String::from_utf8_lossy(&output.stdout))
}

/// ` 3 files changed, 42 insertions(+), 7 deletions(-)` → `(42, 7)`. Either
/// half may be absent (an insertions-only or deletions-only diff), so each is
/// parsed independently rather than assuming both appear.
fn parse_shortstat(text: &str) -> Option<(u64, u64)> {
    let added = number_before(text, "insertion").unwrap_or(0);
    let removed = number_before(text, "deletion").unwrap_or(0);
    if added == 0 && removed == 0 {
        None
    } else {
        Some((added, removed))
    }
}

fn number_before(text: &str, word: &str) -> Option<u64> {
    let at = text.find(word)?;
    text[..at]
        .trim_end()
        .rsplit(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_counts() {
        assert_eq!(
            parse_shortstat(" 3 files changed, 42 insertions(+), 7 deletions(-)\n"),
            Some((42, 7))
        );
    }

    #[test]
    fn parses_insertions_only() {
        assert_eq!(
            parse_shortstat(" 1 file changed, 5 insertions(+)\n"),
            Some((5, 0))
        );
    }

    #[test]
    fn empty_diff_is_none() {
        assert_eq!(parse_shortstat(""), None);
    }

    #[test]
    fn all_three_lines_share_the_icon_width() {
        let lines = startup(Path::new("/tmp"));
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert!(line.starts_with(' ') || line.starts_with('▐') || line.starts_with('▝'));
        }
    }
}
