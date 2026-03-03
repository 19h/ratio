//! Rendering logic for the TUI layout.
//!
//! Layout:
//!
//! ```text
//! ┌─ Header (phase + goal) ─────────────────────────────────┐
//! ├─ Reviewer Output (left) ──┬─ Worker Output (right) ─────┤
//! ├─ Tool Calls (left) ───────┴─ Log (right) ───────────────┤
//! ├─ Status Bar ────────────────────────────────────────────┤
//! └─────────────────────────────────────────────────────────┘
//! ```

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::app::App;
use super::widgets;

/// Render the full application frame.
///
/// Takes `&mut App` so we can clamp scroll positions to actual content height.
pub fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // header
            Constraint::Min(8),     // agent output panes (top half)
            Constraint::Length(12), // tool calls + log (bottom half)
            Constraint::Length(1),  // status bar
        ])
        .split(area);

    render_header(frame, outer[0], app);
    render_agent_panes(frame, outer[1], app);
    render_bottom_panes(frame, outer[2], app);
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
// Agent output panes (reviewer | worker)
// ---------------------------------------------------------------------------

fn render_agent_panes(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(40), // reviewer
            Constraint::Percentage(60), // worker
        ])
        .split(area);

    // Reviewer pane
    {
        let pane_area = columns[0];
        let paragraph = widgets::reviewer_paragraph(
            &app.reviewer_thinking,
            &app.reviewer_plan,
            &app.reviewer_output,
            0,
            app.focused,
        );
        let inner = inner_area(pane_area);
        let clamped = clamp_scroll(
            paragraph.line_count(inner.width),
            inner.height,
            app.reviewer_scroll,
        );
        app.reviewer_scroll = clamped;
        let paragraph = widgets::reviewer_paragraph(
            &app.reviewer_thinking,
            &app.reviewer_plan,
            &app.reviewer_output,
            clamped,
            app.focused,
        );
        frame.render_widget(paragraph, pane_area);
    }

    // Worker pane
    {
        let pane_area = columns[1];
        let paragraph = widgets::worker_paragraph(
            &app.worker_thinking,
            &app.worker_plan,
            &app.worker_output,
            0,
            app.focused,
        );
        let inner = inner_area(pane_area);
        let clamped = clamp_scroll(
            paragraph.line_count(inner.width),
            inner.height,
            app.worker_scroll,
        );
        app.worker_scroll = clamped;
        let paragraph = widgets::worker_paragraph(
            &app.worker_thinking,
            &app.worker_plan,
            &app.worker_output,
            clamped,
            app.focused,
        );
        frame.render_widget(paragraph, pane_area);
    }
}

// ---------------------------------------------------------------------------
// Bottom panes (tool calls | log)
// ---------------------------------------------------------------------------

fn render_bottom_panes(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(40), // tools
            Constraint::Percentage(60), // log
        ])
        .split(area);

    // Tools pane
    {
        let pane_area = columns[0];
        let paragraph = widgets::tool_calls_paragraph(&app.tool_calls, 0, app.focused);
        let inner = inner_area(pane_area);
        let clamped = clamp_scroll(
            paragraph.line_count(inner.width),
            inner.height,
            app.tool_scroll,
        );
        app.tool_scroll = clamped;
        let paragraph = widgets::tool_calls_paragraph(&app.tool_calls, clamped, app.focused);
        frame.render_widget(paragraph, pane_area);
    }

    // Log pane
    {
        let pane_area = columns[1];
        let paragraph = widgets::log_paragraph(&app.log_entries, 0, app.focused);
        let inner = inner_area(pane_area);
        let clamped = clamp_scroll(
            paragraph.line_count(inner.width),
            inner.height,
            app.log_scroll,
        );
        app.log_scroll = clamped;
        let paragraph = widgets::log_paragraph(&app.log_entries, clamped, app.focused);
        frame.render_widget(paragraph, pane_area);
    }
}

// ---------------------------------------------------------------------------
// Status bar
// ---------------------------------------------------------------------------

fn render_status_bar(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let bar = widgets::status_bar(
        &app.phase,
        app.current_cycle,
        app.max_cycles,
        app.focused,
        app.abort_requested,
        app.finished,
    );
    let paragraph = Paragraph::new(bar);
    frame.render_widget(paragraph, area);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compute the inner area of a block with all borders (1 cell border on each side).
fn inner_area(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

/// Clamp a scroll value so the viewport always shows content at the bottom.
///
/// `total_lines` is the number of wrapped lines in the paragraph.
/// `viewport_height` is the inner height of the pane (excluding borders).
/// `requested` is the raw scroll offset (may be `u16::MAX` for "go to bottom").
///
/// Returns the clamped scroll value.
fn clamp_scroll(total_lines: usize, viewport_height: u16, requested: u16) -> u16 {
    let max_scroll = total_lines.saturating_sub(viewport_height as usize);
    let max_scroll = u16::try_from(max_scroll).unwrap_or(u16::MAX);
    requested.min(max_scroll)
}
