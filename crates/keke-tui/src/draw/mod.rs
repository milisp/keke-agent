pub(crate) mod file_search;
pub(crate) mod header;
pub(crate) mod input;
pub(crate) mod markdown;
pub(crate) mod menu;
pub(crate) mod picker;
pub(crate) mod plan;
pub(crate) mod status;
pub(crate) mod subagents;
pub(crate) mod transcript;
pub(crate) mod turn_status;

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
    let frame_area = frame.area();
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            // The slash-command menu and the `@`-completion dropdown never
            // open together (one needs the line to start with `/`, the other
            // needs an `@` with no preceding word character), so they share
            // one row of layout.
            Constraint::Length(menu::rows(app).max(file_search::rows(app))),
            // The turn-status row appears above the composer only while a
            // turn runs, and collapses to nothing when idle.
            Constraint::Length(turn_status::rows(app)),
            Constraint::Length(subagents::rows(app)),
            Constraint::Length(input::rows(app, frame.area().width)),
            Constraint::Length(picker::rows(app, frame.area().height)),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let (header, body, menu, turn, agents, composer, picker_area, footer) = (
        areas[0], areas[1], areas[2], areas[3], areas[4], areas[5], areas[6], areas[7],
    );

    let rendered = transcript::render(app.transcript.cells(), body.width, app.expanded());
    app.scroll
        .measure(rendered.lines.len(), usize::from(body.height));
    let offset = app.scroll.offset();

    // A header only answers a click while it is on screen, so the map is of
    // this frame and is rebuilt whole every frame.
    let toggles = rendered
        .toggles
        .iter()
        .filter(|(line, _)| *line >= offset && *line < offset + usize::from(body.height))
        .filter_map(|(line, key)| {
            u16::try_from(line - offset)
                .ok()
                .map(|row| (body.y + row, *key))
        })
        .collect();
    app.set_toggles(toggles);

    let visible: Vec<_> = rendered
        .lines
        .into_iter()
        .skip(offset)
        .take(usize::from(body.height))
        .collect();
    // The drag is answered against what was drawn, so the frame hands the
    // selection its own rows before asking it to mark them.
    app.selection
        .set_rows(body.y, visible.iter().map(ToString::to_string).collect());
    let visible: Vec<_> = visible
        .into_iter()
        .enumerate()
        .map(|(row, line)| {
            let row = u16::try_from(row)
                .unwrap_or(u16::MAX)
                .saturating_add(body.y);
            app.selection.highlight(row, line)
        })
        .collect();
    frame.render_widget(ratatui::widgets::Paragraph::new(visible), body);
    below(frame, body, app);

    menu::draw(frame, menu, app);
    file_search::draw(frame, menu, app);
    input::draw(frame, composer, app);
    turn_status::draw(frame, turn, app);
    subagents::draw(frame, agents, app);
    header::draw(frame, header, app);
    picker::draw(frame, picker_area, app);
    status::draw(frame, footer, app);
    // Last: the remaining overlay holds the keyboard, so nothing may be drawn over it.
    subagents::detail(frame, app);
    plan::draw(frame, frame_area, app);
    // After the plan, and only then: the policy overlay is opened *by* the
    // plan review and holds the keyboard over it, so it must not be painted
    // under the panel it is answering for.
    if app.policy_picker().is_some() {
        picker::draw(frame, picker_area, app);
    }
}
