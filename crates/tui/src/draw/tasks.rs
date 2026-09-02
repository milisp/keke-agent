//! One line for the background commands, under the subagent rows.
//!
//! A background command is invisible for the same reason a subagent is: the
//! tool call that started it returned immediately, and nothing else is said
//! until someone reads it. But there is far less to say about a command than
//! about a delegated turn — it is running or it is not — so this is a count
//! rather than a pane, and it disappears the moment nothing is left.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;

use crate::app::App;

pub(crate) fn rows(app: &App) -> u16 {
    u16::from(!app.tasks().is_empty())
}

/// `2 commands · 1 finished`, or nothing at all.
///
/// Finished-but-unread is worth its own number: that is the state a person can
/// act on, and the one a model has forgotten to read.
#[must_use]
pub(crate) fn summary(tasks: &[keke_acp::TaskView]) -> Option<String> {
    if tasks.is_empty() {
        return None;
    }
    let running = tasks.iter().filter(|task| task.is_running()).count();
    let finished = tasks.len() - running;
    let mut parts = Vec::new();
    if running > 0 {
        parts.push(format!(
            "{running} background {}",
            if running == 1 { "command" } else { "commands" }
        ));
    }
    if finished > 0 {
        parts.push(format!("{finished} finished, unread"));
    }
    Some(parts.join(" · "))
}

pub(crate) fn draw(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    let Some(text) = summary(app.tasks()) else {
        return;
    };
    let line = Line::from(vec![
        Span::styled(" ◎ ", Style::default().fg(Color::Cyan)),
        Span::styled(text, Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(status: &str) -> keke_acp::TaskView {
        keke_acp::TaskView {
            id: "command_1".to_string(),
            kind: "command".to_string(),
            description: "npm run dev".to_string(),
            status: status.to_string(),
        }
    }

    #[test]
    fn nothing_running_draws_no_line() {
        assert_eq!(summary(&[]), None);
    }

    #[test]
    fn one_command_is_not_pluralised() {
        assert_eq!(
            summary(&[task("running")]).as_deref(),
            Some("1 background command")
        );
    }

    /// The unread count is the number a person can act on, so it is never
    /// folded into the running total.
    #[test]
    fn finished_but_unread_is_counted_separately() {
        let rows = vec![task("running"), task("exited"), task("killed")];
        assert_eq!(
            summary(&rows).as_deref(),
            Some("1 background command · 2 finished, unread")
        );
    }
}
