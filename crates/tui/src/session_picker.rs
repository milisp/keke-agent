//! The small, standalone picker `keke resume` opens before it has a chat.
//!
//! It intentionally has no [`keke_acp::Conversation`]: choosing a rollout log
//! must not mint another session merely to ask which old session to restore.

use crossterm::event::Event;
use crossterm::event::EventStream;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use futures::StreamExt;
use ratatui::Frame;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Paragraph;

/// One resumable conversation as the host describes it to the picker.
///
/// The surface receives display data rather than a `keke-core` type so it stays
/// an ACP surface rather than acquiring an engine dependency.
#[derive(Clone, Debug)]
pub struct ResumeChoice {
    pub id: keke_protocol::SessionId,
    pub updated_at: String,
    pub turns: usize,
    pub cwd: Option<String>,
    pub summary: String,
}

struct Picker {
    sessions: Vec<ResumeChoice>,
    cwd: String,
    show_all: bool,
    query: String,
    selected: usize,
}

impl Picker {
    fn new(sessions: Vec<ResumeChoice>, cwd: String, show_all: bool) -> Self {
        Self {
            sessions,
            cwd,
            show_all,
            query: String::new(),
            selected: 0,
        }
    }

    fn rows(&self) -> Vec<&ResumeChoice> {
        let query = self.query.trim().to_lowercase();
        self.sessions
            .iter()
            .filter(|session| {
                self.show_all
                    || (session.turns > 0 && session.cwd.as_deref() == Some(self.cwd.as_str()))
            })
            .filter(|session| {
                query.is_empty()
                    || [
                        session.updated_at.as_str(),
                        session.cwd.as_deref().unwrap_or(""),
                        session.summary.as_str(),
                    ]
                    .into_iter()
                    .any(|field| field.to_lowercase().contains(&query))
            })
            .collect()
    }

    fn selected(&self) -> Option<keke_protocol::SessionId> {
        let rows = self.rows();
        rows.get(self.selected.min(rows.len().saturating_sub(1)))
            .map(|session| session.id)
    }

    fn move_selection(&mut self, delta: isize) {
        let count = self.rows().len();
        if count != 0 {
            self.selected =
                (self.selected.min(count - 1) as isize + delta).rem_euclid(count as isize) as usize;
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<Option<keke_protocol::SessionId>> {
        if key.code == KeyCode::Char('a') && key.modifiers == KeyModifiers::CONTROL {
            self.show_all = !self.show_all;
            self.selected = 0;
            return None;
        }
        match key.code {
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down | KeyCode::Tab => self.move_selection(1),
            KeyCode::Backspace => {
                self.query.pop();
                self.selected = 0;
            }
            KeyCode::Char(ch) => {
                self.query.push(ch);
                self.selected = 0;
            }
            KeyCode::Enter => return self.selected().map(Some),
            KeyCode::Esc => return Some(None),
            _ => {}
        }
        None
    }
}

pub(super) async fn run(
    sessions: Vec<ResumeChoice>,
    cwd: String,
    show_all: bool,
) -> anyhow::Result<Option<keke_protocol::SessionId>> {
    let mut terminal = super::enter()?;
    let result = event_loop(&mut terminal, Picker::new(sessions, cwd, show_all)).await;
    super::leave(&mut terminal)?;
    result
}

async fn event_loop(
    terminal: &mut super::Tui,
    mut picker: Picker,
) -> anyhow::Result<Option<keke_protocol::SessionId>> {
    let mut input = EventStream::new();
    loop {
        terminal.draw(|frame| draw(frame, &picker))?;
        match input.next().await {
            Some(Ok(Event::Key(key))) => {
                if let Some(selection) = picker.handle_key(key) {
                    return Ok(selection);
                }
            }
            Some(Ok(_)) => {}
            Some(Err(error)) => return Err(error.into()),
            None => return Ok(None),
        }
    }
}

fn draw(frame: &mut Frame, picker: &Picker) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([Constraint::Min(3), Constraint::Length(2)])
        .split(frame.area());
    let rows = picker.rows();
    let selected = picker.selected.min(rows.len().saturating_sub(1));
    let visible = usize::from(areas[0].height.saturating_sub(3) / 2).max(1);
    let first = selected.saturating_sub(visible.saturating_sub(1));
    let mut lines = Vec::new();
    if rows.is_empty() {
        lines.push(Line::styled(
            " no sessions match — backspace to widen the filter",
            Style::new().fg(Color::DarkGray),
        ));
    }
    for (at, session) in rows.iter().enumerate().skip(first).take(visible) {
        let style = if at == selected {
            Style::new().fg(Color::Cyan)
        } else {
            Style::new()
        };
        lines.push(Line::from(vec![Span::styled(
            format!(
                "{} {}",
                if at == selected { '>' } else { ' ' },
                session.summary
            ),
            style.add_modifier(Modifier::BOLD),
        )]));
        let mut details = relative_time(&session.updated_at);
        if picker.show_all {
            details.push_str("  ·  ");
            details.push_str(session.cwd.as_deref().unwrap_or("unknown directory"));
        }
        lines.push(Line::styled(
            format!("     {details}"),
            style.fg(if at == selected {
                Color::Cyan
            } else {
                Color::DarkGray
            }),
        ));
    }
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(Color::Cyan))
                .title(if picker.show_all {
                    " resume — all projects; ctrl+a current project "
                } else {
                    " resume — current project; ctrl+a all projects "
                }),
        ),
        areas[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" filter ", Style::new().fg(Color::DarkGray)),
            Span::styled(&picker.query, Style::new().add_modifier(Modifier::BOLD)),
            Span::styled("▏", Style::new().fg(Color::Cyan)),
        ])),
        areas[1],
    );
}

fn relative_time(updated_at: &str) -> String {
    let Ok(updated_at) = chrono::DateTime::parse_from_rfc3339(updated_at) else {
        return "unknown time".to_string();
    };
    let seconds = chrono::Utc::now()
        .signed_duration_since(updated_at.with_timezone(&chrono::Utc))
        .num_seconds()
        .max(0);
    match seconds {
        0..60 => "now".to_string(),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        86_400..604_800 => format!("{}d", seconds / 86_400),
        _ => format!("{}w", seconds / 604_800),
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::*;

    fn choice(summary: &str) -> ResumeChoice {
        ResumeChoice {
            id: keke_protocol::SessionId::new(),
            updated_at: "2026-09-23T12:00:00Z".to_string(),
            turns: 1,
            cwd: Some("/work/test".to_string()),
            summary: summary.to_string(),
        }
    }

    #[test]
    fn typing_filters_and_enter_returns_the_highlighted_session() {
        let one = choice("fix parser");
        let two = choice("update docs");
        let mut picker = Picker::new(vec![one, two.clone()], "/work/test".to_string(), false);
        for ch in "docs".chars() {
            picker.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert_eq!(picker.rows().len(), 1);
        assert_eq!(
            picker.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Some(two.id))
        );
    }

    #[test]
    fn escape_cancels_without_selecting_a_session() {
        let mut picker = Picker::new(vec![choice("fix parser")], "/work/test".to_string(), false);
        assert_eq!(
            picker.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(None)
        );
    }

    #[test]
    fn control_a_toggles_between_the_project_and_every_project() {
        let mut elsewhere = choice("other project");
        elsewhere.cwd = Some("/work/elsewhere".to_string());
        let mut picker = Picker::new(
            vec![choice("this project"), elsewhere],
            "/work/test".to_string(),
            false,
        );
        assert_eq!(picker.rows().len(), 1);
        picker.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(picker.rows().len(), 2);
    }
}
