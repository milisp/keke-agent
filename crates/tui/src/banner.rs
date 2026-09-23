//! The three-line startup banner: icon, version and tagline, and the
//! workspace's git status, all shown once at the top of a fresh scrollback.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

const ICON: [&str; 3] = [" ▗▄▄▖  ", "▐▘◕‿◕▘ ", "▝▀▀▀▘  "];
const BRIGHT_GREEN: &str = "\u{1b}[92m";
const CYAN: &str = "\u{1b}[96m";
const MAGENTA: &str = "\u{1b}[95m";
const RESET: &str = "\u{1b}[0m";

fn colored_face() -> String {
    format!("{CYAN}▐▘{MAGENTA}◕‿◕{CYAN}▘{RESET} ")
}

/// Built once, at session start, from `cwd`. Not refreshed: it answers "what
/// does this workspace look like right now", and once a prompt is sent that
/// answer is stale, so nothing here is worth recomputing.
///
/// `startup` is the elapsed time from process start to this banner being
/// built, when the caller measured one — shown next to the version so a
/// person can see how long the launch took without setting
/// `KEKE_STARTUP_TRACE`.
///
/// `diff` is the workspace's unstaged `(insertions, deletions)`, which the
/// caller measures with [`diff_stat`] off the startup path: `git diff` costs
/// milliseconds in a small repository and far more in a large one, and the
/// first frame must not wait on it.
///
/// `tools` and `skills`, when either is non-empty, sit as counts right after
/// the workspace line — `<cwd>  Tools (15)  Skills (71)` — rather than the
/// names themselves. What is available to a session is worth a glance at
/// launch; what each one does is what `/tools` and `/skills` are for, so the
/// banner only says how many.
pub(crate) fn startup(
    cwd: &Path,
    startup: Option<Duration>,
    diff: Option<(u64, u64)>,
    tools: &[String],
    skills: &[String],
) -> Vec<String> {
    let mut display = crate::draw::header::tilde(cwd);
    if let Some((added, removed)) = diff {
        display = format!("{display}  +{added} -{removed}");
    }

    let version_line = match startup {
        Some(elapsed) => format!(
            "{}keke v{} {BRIGHT_GREEN}{:.0?}{RESET}",
            ICON[0],
            env!("CARGO_PKG_VERSION"),
            elapsed
        ),
        None => format!("{}keke v{}", ICON[0], env!("CARGO_PKG_VERSION")),
    };

    let mut lines = vec![
        version_line,
        format!("{}{}", colored_face(), "any model, one workflow"),
        format!("{}{}", ICON[2], display),
    ];

    let counts: Vec<String> = [
        (!tools.is_empty()).then(|| format!("Tools ({})", tools.len())),
        (!skills.is_empty()).then(|| format!("Skills ({})", skills.len())),
    ]
    .into_iter()
    .flatten()
    .collect();

    if !counts.is_empty() {
        lines[2].push_str("  ");
        lines[2].push_str(&counts.join("  "));
    }

    lines
}

/// `git diff --shortstat`, parsed into `(insertions, deletions)`. `None` when
/// there is no `git` on `PATH`, `dir` is not inside a work tree, or there are
/// no unstaged changes — all three collapse to the same "say nothing"
/// outcome, since an empty right side says more than a pair of zeros would.
pub(crate) fn diff_stat(dir: &Path) -> Option<(u64, u64)> {
    let output = Command::new("git")
        .args(["diff", "--shortstat"])
        // Runs beside the session, whose own git work (checkpoints) must not
        // lose a race for `index.lock` to a banner decoration.
        .env("GIT_OPTIONAL_LOCKS", "0")
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
        let lines = startup(Path::new("/tmp"), None, None, &[], &[]);
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert!(
                line.starts_with('\x1b')
                    || line.starts_with(' ')
                    || line.starts_with('▐')
                    || line.starts_with('▝')
            );
        }
    }

    #[test]
    fn tools_and_skills_show_as_counts_after_the_workspace_line() {
        let tools: Vec<String> = (0..15).map(|n| format!("tool-{n}")).collect();
        let skills: Vec<String> = (0..71).map(|n| format!("skill-{n}")).collect();
        let lines = startup(Path::new("/tmp"), None, None, &tools, &skills);
        assert_eq!(lines.len(), 3);
        assert!(lines[2].contains("Tools (15)"));
        assert!(lines[2].contains("Skills (71)"));
        assert!(!lines[0].contains("Tools") && !lines[0].contains("Skills"));
        assert!(!lines[1].contains("Tools") && !lines[1].contains("Skills"));
        assert!(!lines[2].contains("tool-0"));
        assert!(!lines[2].contains("skill-0"));
    }
}
