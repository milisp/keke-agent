//! The one-line header bar: where the session lives on the left, what it is
//! spending the model's attention on the right.
//!
//! The context-window figure answers "how much of this conversation can still
//! fit" — the question a person asks just before a long session starts
//! truncating. It uses input tokens because that is the axis the window
//! constrains; output is billed separately and does not crowd it.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::draw::status::tokens;

/// Columns kept clear at the right edge so the usage figure never sits flush
/// against the border.
const RIGHT_MARGIN: usize = 1;

pub(crate) fn draw(frame: &mut Frame, area: Rect, app: &App) {
    // Tilde-shorten against home so the bar stays readable anywhere.
    let mut cwd = app.cwd().to_string_lossy().to_string();
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy().to_string();
        if let Some(rest) = cwd.strip_prefix(&home) {
            cwd = format!("~{rest}");
        }
    }

    // Right-aligned by padding from the left: one paragraph, no second
    // layout pass, and truncation falls out of the widget.
    // Input against the context window: the axis the window actually
    // constrains. Without a known window there is no fraction worth showing,
    // so usage alone; with neither, nothing — an empty right side says more
    // than a pair of zeros would.
    let used = app.context_input();
    let right = match (used > 0, app.context_window()) {
        (true, Some(window)) => format!("{}/{}", tokens(used), tokens(window)),
        (true, None) => format!("{}", tokens(used)),
        (false, Some(window)) => format!("0/{}", tokens(window)),
        (false, None) => String::new(),
    };

    let width = usize::from(area.width);
    // Two columns wide at most each so cwd and usage cannot collide; whatever
    // does not fit is dropped from the middle of the path, whose tail is the
    // part a person recognises.
    let left_budget = width
        .saturating_sub(right.chars().count() + RIGHT_MARGIN + 1)
        .min(width / 2);
    if cwd.chars().count() > left_budget {
        let suffix = format!(
            "…{}",
            cwd.chars()
                .rev()
                .take(left_budget.saturating_sub(1))
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<String>()
        );
        cwd = suffix;
    }
    let pad = width.saturating_sub(cwd.chars().count() + right.chars().count() + RIGHT_MARGIN);
    let line = Line::from(vec![
        Span::styled(format!(" {cwd}"), Style::new().fg(Color::DarkGray)),
        Span::raw(" ".repeat(pad)),
        Span::styled(right, Style::new().fg(Color::Cyan)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod tests {

    use crate::draw::header::RIGHT_MARGIN;

    fn truncate(cwd: &str, width: u16, right: &str) -> String {
        // The same rule draw() applies, extracted for testing.
        let width = usize::from(width);
        let left_budget = width
            .saturating_sub(right.chars().count() + RIGHT_MARGIN + 1)
            .min(width / 2);
        if cwd.chars().count() > left_budget {
            format!(
                "…{}",
                cwd.chars()
                    .rev()
                    .take(left_budget.saturating_sub(1))
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<String>()
            )
        } else {
            cwd.to_string()
        }
    }

    #[test]
    fn a_long_path_loses_its_head_but_keeps_the_project_name() {
        assert_eq!(
            truncate("/very/long/prefix/keke", 20, "gpt-5"),
            "…efix/keke"
        );
    }

    #[test]
    fn a_narrow_bar_with_a_wide_right_side_truncates_to_nothing_without_panicking() {
        // left_budget hits zero; the ellipsis-only result must not underflow.
        assert_eq!(truncate("/keke", 10, "12345678"), "…");
    }

    #[test]
    fn a_short_path_is_left_alone() {
        assert_eq!(truncate("/home/g/keke", 40, "gpt-5"), "/home/g/keke");
    }
}
