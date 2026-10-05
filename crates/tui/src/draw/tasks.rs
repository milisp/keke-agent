//! Read-only shell details below the composer. Inspection never consumes the
//! output that the agent still needs to collect.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;

use crate::app::App;

pub(crate) fn rows(app: &App) -> u16 {
    if app.tasks_expanded {
        u16::try_from(app.tasks().len()).unwrap_or(u16::MAX).min(6)
    } else {
        0
    }
}

fn detail(task: &keke_acp::TaskView, elapsed: std::time::Duration) -> Line<'static> {
    // Each task owns one row; control characters must not impersonate other
    // rows or terminal UI when a command comes from repository content.
    let command = task
        .description
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>();
    Line::from(vec![
        Span::styled(
            format!(" {} ", crate::ported::grok_build::format_duration(elapsed)),
            Style::new().fg(Color::DarkGray),
        ),
        Span::raw(command),
    ])
}

pub(crate) fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    app.task_rows.clear();
    if area.height == 0 {
        return;
    }
    let mut lines: Vec<_> = app
        .tasks()
        .iter()
        .take(usize::from(area.height))
        .map(|task| {
            detail(
                task,
                app.task_since
                    .get(&task.id)
                    .map(std::time::Instant::elapsed)
                    .unwrap_or_default(),
            )
        })
        .collect();
    let hidden = app.tasks().len().saturating_sub(lines.len());
    if hidden > 0
        && let Some(last) = lines.last_mut()
    {
        *last = Line::styled(
            format!(" … {} more shells", hidden + 1),
            Style::new().fg(Color::DarkGray),
        );
    }
    frame.render_widget(Paragraph::new(lines), area);
    let count = app.tasks().len().min(usize::from(area.height));
    for index in 0..count {
        if app.tasks().len() > count && index + 1 == count {
            break;
        }
        app.task_rows.push((
            Rect::new(area.x, area.y + index as u16, area.width, 1),
            app.tasks()[index].id.clone(),
        ));
    }
}

pub(crate) fn viewer(frame: &mut Frame, app: &mut App) {
    app.task_rows.clear();
    let id = app.task_viewer.as_deref().unwrap_or_default();
    let preview = app.task_preview(id);
    let text = match preview {
        Some(preview) => format!("[{} bytes omitted]\n{}", preview.dropped, preview.text),
        None => "Output unavailable (task removed or preview unsupported).".to_string(),
    };
    let text: String = text
        .chars()
        .map(|c| if c.is_control() && c != '\n' { ' ' } else { c })
        .collect();
    let area = frame.area();
    let body = Rect::new(
        area.x,
        area.y + 1,
        area.width,
        area.height.saturating_sub(1),
    );
    let max = text
        .lines()
        .count()
        .saturating_sub(usize::from(body.height));
    let offset = app.task_scroll.unwrap_or(max).min(max);
    if app.task_scroll.is_some() {
        app.task_scroll = Some(offset);
    }
    app.task_offset = offset;
    frame.render_widget(
        Paragraph::new(format!(
            " Shell {id} — Escape: close · arrows/PgUp/PgDn · End: tail"
        )),
        Rect::new(area.x, area.y, area.width, 1),
    );
    frame.render_widget(
        Paragraph::new(text.lines().skip(offset).collect::<Vec<_>>().join("\n")),
        body,
    );
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
    fn details_show_elapsed_time_and_command_not_agent_metadata() {
        let line = detail(&task("running"), std::time::Duration::from_secs(12)).to_string();
        assert!(line.contains("12s"));
        assert!(line.contains("npm run dev"));
        assert!(!line.contains("command_1"));
        assert!(!line.contains("running"));
    }
}
