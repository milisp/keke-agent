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
    let hidden = app.visible_scroll().below();
    if app.visible_scroll().is_following() || hidden == 0 || body.height == 0 {
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
    if app.task_viewer.is_some() {
        tasks::viewer(frame, app);
        return;
    }
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
    let inspecting = app.open_subagent().is_some();
    let full_transcript = app.full_transcript();
    let transcript_only = full_transcript || inspecting;
    let composer_visible =
        !transcript_only && (!planning || composing) && !managing_mcp && !blocked;
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            // The slash-command menu and the `@`-completion dropdown never
            // open together (one needs the line to start with `/`, the other
            // needs an `@` with no preceding word character), so they share
            // one row of layout.
            Constraint::Length(if transcript_only {
                0
            } else {
                menu::rows(app).max(file_search::rows(app))
            }),
            // The turn-status row appears above the composer only while a
            // turn runs, and collapses to nothing when idle.
            Constraint::Length(if transcript_only {
                0
            } else {
                turn_status::rows(app)
            }),
            Constraint::Length(u16::from(
                composer_visible && app.cache_miss_tokens().is_some(),
            )),
            Constraint::Length(if !composer_visible {
                0
            } else {
                input::rows(app, frame.area().width)
            }),
            Constraint::Length(if transcript_only { 0 } else { tasks::rows(app) }),
            Constraint::Length(if transcript_only {
                0
            } else {
                permission::rows(app)
            }),
            Constraint::Length(if transcript_only {
                0
            } else {
                picker::rows(app, frame.area().height)
            }),
            Constraint::Length(if transcript_only {
                0
            } else {
                rewind::rows(app, frame.area().height)
            }),
            Constraint::Length(if transcript_only { 0 } else { plan::rows(app) }),
            Constraint::Length(u16::from(
                transcript_only
                    || subagents::rows(app) > 0
                    || (!planning && !managing_mcp && !blocked),
            )),
            // Live child titles always follow the bottom status line.
            Constraint::Length(if inspecting { 0 } else { subagents::rows(app) }),
        ])
        .split(frame.area());

    let (
        header,
        body,
        menu,
        turn,
        cache_notice,
        composer,
        background,
        approval,
        picker_area,
        rewind_area,
        policies,
        footer,
        agents,
    ) = (
        areas[0], areas[1], areas[2], areas[3], areas[4], areas[5], areas[6], areas[7], areas[8],
        areas[9], areas[10], areas[11], areas[12],
    );

    let mut rendered = transcript::render(
        app.visible_transcript().cells(),
        body.width,
        app.expanded(),
        full_transcript,
    );
    if inspecting {
        if let Some(error) = &app.subagent_recording.error {
            rendered.lines.push(ratatui::text::Line::raw(error.clone()));
        } else if rendered.lines.is_empty() {
            rendered
                .lines
                .push(ratatui::text::Line::raw("Loading recorded transcript…"));
        }
    }
    app.visible_scroll_mut()
        .measure(rendered.lines.len(), usize::from(body.height));
    // `/view-plan` scrolls the last plan's first line into view; the plan is
    // in the scrollback now, so this is a transcript scroll like any other.
    if !inspecting && let Some(line) = app.wanted_plan_line(&rendered.plan_lines) {
        app.reveal_plan_line(line);
    }
    let offset = app.visible_scroll().offset();

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
    for (row, range) in rendered.copy_ranges.range(offset..offset + visible.len()) {
        if let Ok(screen_row) = u16::try_from(row - offset) {
            app.selection
                .set_copy_range(body.y + screen_row, range.0, range.1);
        }
    }
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
    if cache_notice.height > 0
        && let Some(cached_tokens) = app.cache_miss_tokens()
    {
        frame.render_widget(
            ratatui::widgets::Paragraph::new(ratatui::text::Line::styled(
                format!(
                    "new session to save {} tokens",
                    status::tokens(cached_tokens)
                ),
                ratatui::style::Style::new().fg(ratatui::style::Color::Yellow),
            )),
            cache_notice,
        );
    }
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
    if inspecting {
        subagents::navigation(frame, header, footer, app);
    }
    subagents::history(frame, app);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use keke_acp::{ScriptedConversation, SubagentView, Update};
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn cache_miss_is_yellow_directly_above_the_composer_and_disappears_on_a_hit() {
        let (conversation, _) = ScriptedConversation::new(Vec::new());
        let (mut app, _) = super::App::new(Arc::new(conversation));
        app.input.set_text("composer marker");
        let usage = |cached| {
            Update::TokensUsed(keke_protocol::Usage {
                input_tokens: 200_000,
                cached_input_tokens: cached,
                ..Default::default()
            })
        };
        app.apply(usage(180_000));
        app.apply(usage(0));
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| super::draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..24)
            .map(|y| (0..100).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let notice = rows
            .iter()
            .position(|row| row.contains("new session to save"))
            .unwrap();
        assert!(rows[notice].contains("new session to save 180.0k tokens"));
        assert!(rows[notice + 1].contains("message"));
        assert!(rows[notice + 2].contains("composer marker"));
        assert_eq!(buffer[(0, notice as u16)].fg, ratatui::style::Color::Yellow);
        app.apply(usage(190_000));
        terminal.draw(|frame| super::draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        assert!(!(0..24).any(|y| {
            let row: String = (0..100).map(|x| buffer[(x, y)].symbol()).collect();
            row.contains("new session to save")
        }));
    }

    #[test]
    fn shell_row_opens_read_only_view_and_escape_restores_draft() {
        use crossterm::event::{
            KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };
        let (conversation, _) = ScriptedConversation::new(Vec::new());
        let (mut app, _) = super::App::new(Arc::new(conversation));
        app.apply(Update::Tasks(vec![keke_acp::TaskView {
            id: "command_1".into(),
            kind: "command".into(),
            description: "sleep 10".into(),
            status: "running".into(),
        }]));
        app.input.set_text("draft");
        app.tasks_expanded = true;
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| super::draw(frame, &mut app)).unwrap();
        let row = app.task_rows[0].0;
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: row.x,
            row: row.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.task_viewer.as_deref(), Some("command_1"));
        assert!(
            app.next_wakeup(std::time::Duration::from_millis(100))
                .is_some()
        );
        terminal.draw(|frame| super::draw(frame, &mut app)).unwrap();
        app.handle_paste("not inserted");
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        app.task_offset = 20;
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.task_scroll, Some(19));
        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(app.task_scroll, None);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.task_viewer.is_none());
        assert_eq!(app.input.text(), "draft");
    }

    #[test]
    fn shell_chip_toggles_details_below_composer_without_consuming_tasks() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let (conversation, _updates) = ScriptedConversation::new(Vec::new());
        let (mut app, _local) = super::App::new(Arc::new(conversation));
        app.apply(Update::Tasks(vec![keke_acp::TaskView {
            id: "command_1".into(),
            kind: "command".into(),
            description: "cargo test".into(),
            status: "running".into(),
        }]));
        app.input.set_text("composer marker");
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| super::draw(frame, &mut app)).unwrap();
        assert_eq!(super::tasks::rows(&app), 0);
        let chip = app.shell_button.expect("shell hit target");
        let spans = super::status::spans(&app);
        let shell = spans
            .iter()
            .find(|span| span.content.contains("1 shell"))
            .unwrap();
        assert_eq!(shell.style.fg, Some(ratatui::style::Color::Blue));
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: chip.x,
            row: chip.y,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_mouse(click);
        terminal.draw(|frame| super::draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..24)
            .map(|y| (0..100).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let composer = rows
            .iter()
            .position(|row| row.contains("composer marker"))
            .unwrap();
        let detail = rows
            .iter()
            .position(|row| row.contains("cargo test"))
            .unwrap();
        assert!(detail > composer);
        assert_eq!(app.tasks().len(), 1);
        app.handle_mouse(click);
        assert_eq!(super::tasks::rows(&app), 0);
        app.apply(Update::Tasks(vec![keke_acp::TaskView {
            id: "command_1".into(),
            kind: "command".into(),
            description: "cargo test".into(),
            status: "exited".into(),
        }]));
        terminal.draw(|frame| super::draw(frame, &mut app)).unwrap();
        assert!(app.shell_button.is_none());
        assert!(app.tasks().is_empty());
        assert!(app.task_since.is_empty());
    }

    #[test]
    fn subagent_rows_are_below_the_status_bar_and_composer() {
        let (conversation, _updates) = ScriptedConversation::new(Vec::new());
        let (mut app, _local) = super::App::new(Arc::new(conversation));
        app.apply(Update::Subagents(vec![SubagentView {
            title: Some("Inspect parser".to_string()),
            id: "parser".to_string(),
            task: "Inspect parser".to_string(),
            status: None,
            input_tokens: 0,
        }]));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
        terminal
            .draw(|frame| super::draw(frame, &mut app))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let last: String = (0..80).map(|x| buffer[(x, 23)].symbol()).collect();
        assert!(last.contains("Inspect parser"), "{last}");
        let status: String = (0..80).map(|x| buffer[(x, 22)].symbol()).collect();
        let expected: String = super::status::spans(&app)
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(status.trim_end(), expected.trim_end());
        assert!(
            app.open_subagent_at(23),
            "click targets follow the bottom rows"
        );
    }
    #[test]
    fn running_titles_stay_directly_below_status_in_busy_full_and_approval_views() {
        use keke_acp::PermissionId;
        use keke_protocol::{ToolCall, ToolCallId};
        for mode in ["busy", "full", "approval"] {
            let (conversation, _updates) = ScriptedConversation::new(Vec::new());
            let (mut app, _local) = super::App::new(Arc::new(conversation));
            app.apply(Update::TurnStarted);
            app.apply(Update::Subagents(vec![SubagentView {
                title: Some("Inspect parser".to_string()),
                id: "agent_1".to_string(),
                task: "Long instructions must remain only in the child transcript".to_string(),
                status: None,
                input_tokens: 0,
            }]));
            if mode == "full" {
                app.toggle_full_transcript();
            }
            if mode == "approval" {
                app.apply(Update::PermissionRequested {
                    id: PermissionId("permission".to_string()),
                    call: ToolCall {
                        id: ToolCallId::new("call"),
                        name: "bash".to_string(),
                        arguments: serde_json::json!({"command":"pwd"}),
                    },
                    reason: "approval test".to_string(),
                });
            }
            let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
            terminal.draw(|frame| super::draw(frame, &mut app)).unwrap();
            let buffer = terminal.backend().buffer();
            let title: String = (0..100).map(|x| buffer[(x, 29)].symbol()).collect();
            assert!(title.contains("Inspect parser"), "{mode}: {title}");
            assert!(!title.contains("Long instructions"));
            let status: String = (0..100).map(|x| buffer[(x, 28)].symbol()).collect();
            let expected: String = super::status::spans(&app)
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            assert_eq!(status.trim_end(), expected.trim_end(), "{mode}");
        }
    }
}
