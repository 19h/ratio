//! Custom widget components for the TUI.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::orchestrator::{LogLevel, Phase};
use crate::protocol::{ToolCallState, ToolKind};

use super::app::{
    AgentSource, FocusedPane, LogEntry, StreamEntry, TodoItem, TodoPriority, TodoStatus,
};

// ---------------------------------------------------------------------------
// Phase badge
// ---------------------------------------------------------------------------

/// Render a colored phase indicator.
pub fn phase_span(phase: &Phase) -> Span<'static> {
    let (text, style) = match phase {
        Phase::Idle => ("IDLE", Style::default().fg(Color::DarkGray)),
        Phase::Initializing => ("INIT", Style::default().fg(Color::Yellow)),
        Phase::Planning => ("PLANNING", Style::default().fg(Color::Blue).bold()),
        Phase::Working => ("WORKING", Style::default().fg(Color::Cyan).bold()),
        Phase::Reviewing => ("REVIEWING", Style::default().fg(Color::Magenta).bold()),
        Phase::Revising => ("REVISING", Style::default().fg(Color::Yellow).bold()),
        Phase::Approved => ("APPROVED", Style::default().fg(Color::Green).bold()),
        Phase::Failed(_) => ("FAILED", Style::default().fg(Color::Red).bold()),
        Phase::Aborted => ("ABORTED", Style::default().fg(Color::Red).bold()),
    };
    Span::styled(format!(" {text} "), style)
}

/// Style for the focused-pane border.
pub fn focused_border_style(focused: FocusedPane, this_pane: FocusedPane) -> Style {
    if focused == this_pane {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

// ---------------------------------------------------------------------------
// Unified agent stream pane
// ---------------------------------------------------------------------------

/// Build the main agent output pane — a unified chronological stream of
/// text, thoughts, and tool calls as they arrive.
pub fn agent_stream_paragraph<'a>(
    stream: &'a std::collections::VecDeque<StreamEntry>,
    agent: AgentSource,
    scroll: u16,
    focused: FocusedPane,
) -> Paragraph<'a> {
    let (title, title_color) = match agent {
        AgentSource::Worker => ("Worker", Color::Cyan),
        AgentSource::Reviewer => ("Reviewer", Color::Magenta),
    };

    let block = Block::default()
        .title(format!(" {title} "))
        .title_style(Style::default().fg(title_color).bold())
        .borders(Borders::ALL)
        .border_style(focused_border_style(focused, FocusedPane::Agent));

    let mut lines: Vec<Line<'a>> = Vec::new();

    for entry in stream {
        match entry {
            StreamEntry::Text(text) => {
                for line in text.lines() {
                    lines.push(Line::from(line.to_string()));
                }
                // If text ends with newline, lines() won't produce trailing empty.
                // That's fine for streaming.
            }
            StreamEntry::Thought(text) => {
                let style = Style::default().fg(Color::DarkGray).italic();
                for line in text.lines() {
                    lines.push(Line::from(Span::styled(line.to_string(), style)));
                }
            }
            StreamEntry::ToolStart {
                title,
                kind,
                detail,
                ..
            } => {
                let kind_label = tool_kind_label(kind);
                let kind_color = tool_kind_color(kind);
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  [{kind_label}] "),
                        Style::default().fg(kind_color).bold(),
                    ),
                    Span::styled(detail.clone(), Style::default().fg(Color::White)),
                    Span::styled(format!("  {title}"), Style::default().fg(Color::DarkGray)),
                ]));
            }
            StreamEntry::ToolEnd { title, status, .. } => {
                let (marker, color) = match status {
                    ToolCallState::Completed => ("ok", Color::Green),
                    ToolCallState::Failed => ("FAIL", Color::Red),
                    _ => ("?", Color::DarkGray),
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("  [{marker}] "), Style::default().fg(color)),
                    Span::styled(title.clone(), Style::default().fg(Color::DarkGray)),
                ]));
            }
            StreamEntry::Separator(label) => {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    label.clone(),
                    Style::default().fg(Color::Yellow).bold(),
                )));
                lines.push(Line::from(""));
            }
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "Waiting...",
            Style::default().fg(Color::DarkGray).italic(),
        )));
    }

    Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0))
}

// ---------------------------------------------------------------------------
// Todo list pane
// ---------------------------------------------------------------------------

/// Build the todo list pane.
pub fn todo_paragraph<'a>(
    todos: &'a [TodoItem],
    scroll: u16,
    focused: FocusedPane,
) -> Paragraph<'a> {
    let block = Block::default()
        .title(" Todo ")
        .title_style(Style::default().fg(Color::Yellow).bold())
        .borders(Borders::ALL)
        .border_style(focused_border_style(focused, FocusedPane::Todo));

    let mut lines: Vec<Line<'a>> = Vec::new();

    if todos.is_empty() {
        lines.push(Line::from(Span::styled(
            "No todos yet",
            Style::default().fg(Color::DarkGray).italic(),
        )));
    } else {
        for item in todos {
            let (icon, icon_style) = match item.status {
                TodoStatus::Pending => ("  ", Style::default().fg(Color::DarkGray)),
                TodoStatus::InProgress => ("  ", Style::default().fg(Color::Yellow)),
                TodoStatus::Completed => ("  ", Style::default().fg(Color::Green)),
                TodoStatus::Cancelled => ("  ", Style::default().fg(Color::Red)),
            };

            let priority_tag = match item.priority {
                TodoPriority::High => Span::styled(" [H]", Style::default().fg(Color::Red).bold()),
                TodoPriority::Medium => Span::styled("", Style::default()),
                TodoPriority::Low => Span::styled(" [L]", Style::default().fg(Color::DarkGray)),
            };

            let content_style = match item.status {
                TodoStatus::Completed => Style::default().fg(Color::Green),
                TodoStatus::InProgress => Style::default().fg(Color::White),
                TodoStatus::Cancelled => Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::CROSSED_OUT),
                TodoStatus::Pending => Style::default().fg(Color::DarkGray),
            };

            lines.push(Line::from(vec![
                Span::styled(icon.to_string(), icon_style),
                Span::styled(item.content.clone(), content_style),
                priority_tag,
            ]));
        }

        // Summary line.
        let total = todos.len();
        let completed = todos
            .iter()
            .filter(|t| t.status == TodoStatus::Completed)
            .count();
        let in_progress = todos
            .iter()
            .filter(|t| t.status == TodoStatus::InProgress)
            .count();
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("{completed}/{total} done, {in_progress} in progress"),
            Style::default().fg(Color::DarkGray).italic(),
        )));
    }

    Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0))
}

// ---------------------------------------------------------------------------
// Log pane
// ---------------------------------------------------------------------------

/// Build the log entries paragraph.
pub fn log_paragraph<'a>(
    entries: &'a std::collections::VecDeque<LogEntry>,
    scroll: u16,
    focused: FocusedPane,
) -> Paragraph<'a> {
    let block = Block::default()
        .title(" Log ")
        .borders(Borders::ALL)
        .border_style(focused_border_style(focused, FocusedPane::Log));

    let lines: Vec<Line<'a>> = entries
        .iter()
        .map(|entry| {
            let level_style = match entry.level {
                LogLevel::Info => Style::default().fg(Color::Cyan),
                LogLevel::Warn => Style::default().fg(Color::Yellow),
                LogLevel::Error => Style::default().fg(Color::Red),
            };
            let level_label = match entry.level {
                LogLevel::Info => "INF",
                LogLevel::Warn => "WRN",
                LogLevel::Error => "ERR",
            };

            Line::from(vec![
                Span::styled(
                    format!("{} ", entry.timestamp),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(format!("[{level_label}] "), level_style),
                Span::raw(entry.message.as_str()),
            ])
        })
        .collect();

    Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0))
}

// ---------------------------------------------------------------------------
// Status bar
// ---------------------------------------------------------------------------

/// Build the bottom status bar line.
pub fn status_bar<'a>(
    phase: &Phase,
    cycle: usize,
    active_agent: AgentSource,
    focused: FocusedPane,
    abort_requested: bool,
    finished: bool,
    input_mode: bool,
) -> Line<'a> {
    let mut spans = vec![
        Span::styled(
            " ratio ",
            Style::default().fg(Color::Black).bg(Color::Cyan).bold(),
        ),
        Span::raw(" "),
        phase_span(phase),
        Span::raw("  "),
    ];

    if cycle > 0 {
        spans.push(Span::styled(
            format!("cycle {cycle}"),
            Style::default().fg(Color::White),
        ));
        spans.push(Span::raw("  "));
    }

    // Show active agent.
    let agent_color = match active_agent {
        AgentSource::Worker => Color::Cyan,
        AgentSource::Reviewer => Color::Magenta,
    };
    spans.push(Span::styled(
        format!("[{}]", active_agent.label()),
        Style::default().fg(agent_color),
    ));

    let pane_label = match focused {
        FocusedPane::Agent => "Agent",
        FocusedPane::Todo => "Todo",
        FocusedPane::Log => "Log",
    };
    spans.push(Span::styled(
        format!(" {pane_label}"),
        Style::default().fg(Color::DarkGray),
    ));

    spans.push(Span::raw("  "));

    if abort_requested {
        spans.push(Span::styled(
            "ABORTING...",
            Style::default().fg(Color::Red).bold(),
        ));
    } else if finished {
        spans.push(Span::styled(
            "Press q to quit",
            Style::default().fg(Color::Green),
        ));
    } else if input_mode {
        spans.push(Span::styled(
            "Type message, Enter to send, Esc to cancel",
            Style::default().fg(Color::Yellow),
        ));
    } else {
        spans.push(Span::styled(
            "i:input r:switch Tab:pane j/k:scroll Ctrl+K:kill",
            Style::default().fg(Color::DarkGray),
        ));
    }

    Line::from(spans)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn tool_kind_label(kind: &ToolKind) -> &'static str {
    match kind {
        ToolKind::Read => "read",
        ToolKind::Edit => "edit",
        ToolKind::Delete => "del",
        ToolKind::Move => "move",
        ToolKind::Search => "search",
        ToolKind::Execute => "exec",
        ToolKind::Think => "think",
        ToolKind::Fetch => "fetch",
        ToolKind::SwitchMode => "mode",
        ToolKind::Todo => "todo",
        ToolKind::Other => "tool",
    }
}

fn tool_kind_color(kind: &ToolKind) -> Color {
    match kind {
        ToolKind::Read => Color::Blue,
        ToolKind::Edit => Color::Yellow,
        ToolKind::Delete => Color::Red,
        ToolKind::Move => Color::Magenta,
        ToolKind::Search => Color::Cyan,
        ToolKind::Execute => Color::Green,
        ToolKind::Think => Color::DarkGray,
        ToolKind::Fetch => Color::Blue,
        ToolKind::SwitchMode => Color::Magenta,
        ToolKind::Todo => Color::Yellow,
        ToolKind::Other => Color::DarkGray,
    }
}
