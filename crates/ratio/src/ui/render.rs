//! Rendering logic for the TUI layout.
//!
//! Layout:
//!
//! ```text
//! ┌─ Header (phase + goal) ──────────────────────────────────────┐
//! ├─ Agent Stream (left, full height) ─┬─ Todo List (top-right) ─┤
//! │                                    ├─ Log (bottom-right) ────┤
//! │                                    │                         │
//! ├─ [Input bar, when active] ─────────┴─────────────────────────┤
//! ├─ Status Bar ─────────────────────────────────────────────────┤
//! └──────────────────────────────────────────────────────────────┘
//! ```

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::app::App;
use super::widgets;

/// Render the full application frame.
pub fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();

    // Determine how many rows the input bar needs.
    let input_height: u16 = if app.input_mode { 3 } else { 0 };

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),            // header
            Constraint::Min(8),               // main content
            Constraint::Length(input_height), // input bar (0 or 3)
            Constraint::Length(1),            // status bar
        ])
        .split(area);

    render_header(frame, outer[0], app);
    render_main(frame, outer[1], app);
    if app.input_mode {
        render_input_bar(frame, outer[2], app);
    }
    render_status_bar(frame, outer[3], app);
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

fn render_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let max_goal_len = (area.width as usize).saturating_sub(30);
    let goal_display = if app.goal.len() > max_goal_len {
        format!(
            "{}...",
            &app.goal[..app.goal.len().min(max_goal_len.saturating_sub(3))]
        )
    } else {
        app.goal.clone()
    };

    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " RATIO ",
            Style::default().fg(Color::Black).bg(Color::Cyan).bold(),
        ),
        Span::raw(" "),
        widgets::phase_span(&app.phase),
        Span::raw("  "),
        Span::styled(goal_display, Style::default().fg(Color::White).italic()),
    ]))
    .block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(Color::DarkGray)),
    )
    .wrap(Wrap { trim: true });

    frame.render_widget(header, area);
}

// ---------------------------------------------------------------------------
// Main content: agent pane (left) + todo/log (right)
// ---------------------------------------------------------------------------

fn render_main(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(60), // agent stream
            Constraint::Percentage(40), // todo + log
        ])
        .split(area);

    // Left: unified agent stream.
    render_agent_pane(frame, columns[0], app);

    // Right: todo (top) + log (bottom).
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(50), // todo
            Constraint::Percentage(50), // log
        ])
        .split(columns[1]);

    render_todo_pane(frame, right[0], app);
    render_log_pane(frame, right[1], app);
}

fn render_agent_pane(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let stream = app.active_stream();
    let paragraph = widgets::agent_stream_paragraph(stream, &app.active_agent, 0, app.focused);
    let inner = inner_area(area);
    let clamped = clamp_scroll(
        paragraph.line_count(inner.width),
        inner.height,
        app.agent_scroll,
    );
    app.agent_scroll = clamped;

    let paragraph = widgets::agent_stream_paragraph(
        app.active_stream(),
        &app.active_agent,
        clamped,
        app.focused,
    );
    frame.render_widget(paragraph, area);
}

fn render_todo_pane(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let paragraph = widgets::todo_paragraph(&app.todos, 0, app.focused);
    let inner = inner_area(area);
    let clamped = clamp_scroll(
        paragraph.line_count(inner.width),
        inner.height,
        app.todo_scroll,
    );
    app.todo_scroll = clamped;
    let paragraph = widgets::todo_paragraph(&app.todos, clamped, app.focused);
    frame.render_widget(paragraph, area);
}

fn render_log_pane(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let paragraph = widgets::log_paragraph(&app.log_entries, 0, app.focused);
    let inner = inner_area(area);
    let clamped = clamp_scroll(
        paragraph.line_count(inner.width),
        inner.height,
        app.log_scroll,
    );
    app.log_scroll = clamped;
    let paragraph = widgets::log_paragraph(&app.log_entries, clamped, app.focused);
    frame.render_widget(paragraph, area);
}

// ---------------------------------------------------------------------------
// Input bar
// ---------------------------------------------------------------------------

fn render_input_bar(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let target_label = app.active_agent.label();
    let block = Block::default()
        .title(format!(" Message to {target_label} "))
        .title_style(Style::default().fg(Color::Yellow).bold())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));

    let input_text = Paragraph::new(app.input_buffer.as_str()).block(block);

    frame.render_widget(input_text, area);

    // Place cursor.
    let inner = inner_area(area);
    let cursor_x = inner.x + app.input_cursor as u16;
    let cursor_y = inner.y;
    if cursor_x < inner.x + inner.width {
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

// ---------------------------------------------------------------------------
// Status bar
// ---------------------------------------------------------------------------

fn render_status_bar(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let bar = widgets::status_bar(
        &app.phase,
        app.current_cycle,
        &app.active_agent,
        app.focused,
        app.abort_requested,
        app.finished,
        app.input_mode,
        app.stakeholder_streams.len(),
    );
    let paragraph = Paragraph::new(bar);
    frame.render_widget(paragraph, area);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compute the inner area of a block with all borders.
fn inner_area(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

/// Clamp a scroll value so the viewport always shows content at the bottom.
fn clamp_scroll(total_lines: usize, viewport_height: u16, requested: u16) -> u16 {
    let max_scroll = total_lines.saturating_sub(viewport_height as usize);
    let max_scroll = u16::try_from(max_scroll).unwrap_or(u16::MAX);
    requested.min(max_scroll)
}
