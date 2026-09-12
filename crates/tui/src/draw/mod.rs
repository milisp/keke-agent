pub(crate) mod diff;
pub(crate) mod file_search;
pub(crate) mod header;
pub(crate) mod input;
pub(crate) mod markdown;
pub(crate) mod menu;
pub(crate) mod permission;
pub(crate) mod picker;
pub(crate) mod plan;
pub(crate) mod rewind;
pub(crate) mod status;
pub(crate) mod subagents;
pub(crate) mod tasks;
pub(crate) mod transcript;
pub(crate) mod turn_status;

use ratatui::Frame;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::widgets::Clear;

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
    // While a plan waits, the screen belongs to it: the composer has nothing
    // to say until a comment is being written, and the status bar's policy is
    // exactly what the panel below the plan is asking about.
    let planning = app.plan_review().is_some();
    let composing = planning && app.plan_focus() == crate::app::plan::PlanFocus::Composer;
    // The MCP overlay is a management pane, not a box over the composer: while
    // it is open there is nothing to type into the composer and nothing the
    // status bar says that the overlay does not already say better, so both
    // collapse and the pane reads as owning the bottom of the screen.
    let managing_mcp = app.mcp_picker().is_some();
    // While a tool call is blocked on approval, the approval panel owns the
    // composer's row and the keyboard: there is nothing to type, and the
    // status bar's turn state is exactly what the panel is already saying.
    let blocked = app.turn() == crate::app::Turn::AwaitingPermission;
    let full_transcript = app.full_transcript();
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            // The slash-command menu and the `@`-completion dropdown never
            // open together (one needs the line to start with `/`, the other
            // needs an `@` with no preceding word character), so they share
            // one row of layout.
            Constraint::Length(if full_transcript {
                0
            } else {
                menu::rows(app).max(file_search::rows(app))
            }),
            // The turn-status row appears above the composer only while a
            // turn runs, and collapses to nothing when idle.
            Constraint::Length(if full_transcript {
                0
            } else {
                turn_status::rows(app)
            }),
            Constraint::Length(if full_transcript {
                0
            } else {
                subagents::rows(app)
            }),
            Constraint::Length(if full_transcript { 0 } else { tasks::rows(app) }),
            Constraint::Length(
                if full_transcript || (planning && !composing) || managing_mcp || blocked {
                    0
                } else {
                    input::rows(app, frame.area().width)
                },
            ),
            Constraint::Length(if full_transcript {
                0
            } else {
                permission::rows(app)
            }),
            Constraint::Length(if full_transcript {
                0
            } else {
                picker::rows(app, frame.area().height)
            }),
            Constraint::Length(if full_transcript {
                0
            } else {
                rewind::rows(app, frame.area().height)
            }),
            Constraint::Length(if full_transcript { 0 } else { plan::rows(app) }),
            Constraint::Length(u16::from(
                full_transcript || (!planning && !managing_mcp && !blocked),
            )),
        ])
        .split(frame.area());

    let (
        header,
        body,
        menu,
        turn,
        agents,
        background,
        composer,
        approval,
        picker_area,
        rewind_area,
        policies,
        footer,
    ) = (
        areas[0], areas[1], areas[2], areas[3], areas[4], areas[5], areas[6], areas[7], areas[8],
        areas[9], areas[10], areas[11],
    );

    let rendered = transcript::render(
        app.transcript.cells(),
        body.width,
        app.expanded(),
        full_transcript,
    );
    app.scroll
        .measure(rendered.lines.len(), usize::from(body.height));
    // `/view-plan` scrolls the last plan's first line into view; the plan is
    // in the scrollback now, so this is a transcript scroll like any other.
    if let Some(line) = app.wanted_plan_line(&rendered.plan_lines) {
        app.reveal_plan_line(line);
    }
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
    // Paragraph only paints the rows it receives. Clearing first matters when
    // a long expanded tool run is collapsed: otherwise the terminal keeps
    // the old rows (or their trailing characters) below the shorter header.
    frame.render_widget(Clear, body);
    frame.render_widget(ratatui::widgets::Paragraph::new(visible), body);
    below(frame, body, app);

    menu::draw(frame, menu, app);
    file_search::draw(frame, menu, app);
    input::draw(frame, composer, app);
    permission::draw(frame, approval, app);
    turn_status::draw(frame, turn, app);
    subagents::draw(frame, agents, app);
    tasks::draw(frame, background, app);
    header::draw(frame, header, app);
    picker::draw(frame, picker_area, app);
    rewind::draw(frame, rewind_area, app);
    plan::draw(frame, policies, app);
    status::draw(frame, footer, app);
    // Last: the remaining overlay holds the keyboard, so nothing may be drawn over it.
    subagents::detail(frame, app);
}
