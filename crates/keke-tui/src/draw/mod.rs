pub(crate) mod input;
pub(crate) mod menu;
pub(crate) mod status;
pub(crate) mod transcript;

use ratatui::Frame;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;

use crate::app::App;

/// How much is still below, centred on the last row of the transcript, and
/// clickable to get back to it.
///
/// Only while the reader has scrolled away from the tail. Output arriving
/// under a person who is reading something else must announce itself, and must
/// not do it by moving what they are reading — so it announces itself here,
/// where the pointer already is when they decide to go back.
fn below(frame: &mut Frame, body: ratatui::layout::Rect, app: &mut App) {
    let hidden = app.scroll.below();
    if app.scroll.is_following() || hidden == 0 || body.height == 0 {
        app.set_follow_button(None);
        return;
    }
    let label = format!(" ↓ {hidden} more lines ");
    let width = u16::try_from(label.chars().count()).unwrap_or(body.width);
    if width > body.width {
        app.set_follow_button(None);
        return;
    }
    let area = ratatui::layout::Rect {
        x: body.x + (body.width - width) / 2,
        y: body.bottom() - 1,
        width,
        height: 1,
    };
    let style = ratatui::style::Style::new()
        .fg(ratatui::style::Color::Black)
        .bg(ratatui::style::Color::Cyan);
    frame.render_widget(
        ratatui::widgets::Paragraph::new(ratatui::text::Line::styled(label, style)),
        area,
    );
    app.set_follow_button(Some((area.x, area.y, area.width)));
}

/// Draw one frame.
///
/// The transcript is rendered first so its wrapped height is known before the
/// viewport decides what to show; scrolling anchors to wrapped lines rather
/// than to cells, which is the only way a long tool result scrolls smoothly.
pub(crate) fn draw(frame: &mut Frame, app: &mut App) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(menu::rows(app)),
            Constraint::Length(input::rows(app)),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let (body, menu, composer, footer) = (areas[0], areas[1], areas[2], areas[3]);

    let lines = transcript::render(app.transcript.cells(), body.width, app.show_thinking());
    app.scroll.measure(lines.len(), usize::from(body.height));
    let offset = app.scroll.offset();

    let visible: Vec<_> = lines
        .into_iter()
        .skip(offset)
        .take(usize::from(body.height))
        .collect();
    frame.render_widget(ratatui::widgets::Paragraph::new(visible), body);
    below(frame, body, app);

    menu::draw(frame, menu, app);
    input::draw(frame, composer, app);
    status::draw(frame, footer, app);
}
