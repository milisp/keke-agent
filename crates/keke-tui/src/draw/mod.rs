pub(crate) mod input;
pub(crate) mod status;
pub(crate) mod transcript;

use ratatui::Frame;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;

use crate::app::App;

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
            Constraint::Length(input::rows(app)),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let (body, composer, footer) = (areas[0], areas[1], areas[2]);

    let lines = transcript::render(app.transcript.cells(), body.width, app.show_thinking());
    app.scroll.measure(lines.len(), usize::from(body.height));
    let offset = app.scroll.offset();

    let visible: Vec<_> = lines
        .into_iter()
        .skip(offset)
        .take(usize::from(body.height))
        .collect();
    frame.render_widget(ratatui::widgets::Paragraph::new(visible), body);

    input::draw(frame, composer, app);
    status::draw(frame, footer, app);
}
