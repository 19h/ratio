//! Custom widget components for the TUI.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::orchestrator::{LogLevel, Phase};
use crate::protocol::{PlanEntry, PlanEntryStatus, ToolCallState, ToolKind};

use super::app::{AgentSource, FocusedPane, LogEntry, ToolCallRecord};

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
// Agent output panes — now show thinking + plan + output combined
// ---------------------------------------------------------------------------

/// Build a combined paragraph for an agent pane showing:
/// 1. Thinking text (dimmed italic)
/// 2. Plan entries (with status icons)
/// 3. Output text (normal)
#[allow(clippy::too_many_arguments)]
fn agent_paragraph<'a>(
    title: &'a str,
    title_color: Color,
    thinking: &'a str,
    plan: &'a [PlanEntry],
    output: &'a str,
    scroll: u16,
    focused: FocusedPane,
    pane: FocusedPane,
) -> Paragraph<'a> {
    let block = Block::default()
        .title(format!(" {title} "))
        .title_style(Style::default().fg(title_color).bold())
        .borders(Borders::ALL)
        .border_style(focused_border_style(focused, pane));

    let mut lines: Vec<Line<'a>> = Vec::new();

    // ── Thinking section ────────────────────────────────────────
    if !thinking.is_empty() {
        lines.push(Line::from(Span::styled(
            "--- thinking ---",
            Style::default().fg(Color::DarkGray).italic(),
        )));
        let think_style = Style::default().fg(Color::DarkGray).italic();
        for line in thinking.lines() {
            lines.push(Line::from(Span::styled(line.to_string(), think_style)));
        }
        lines.push(Line::from(Span::styled(
            "--- end thinking ---",
            Style::default().fg(Color::DarkGray).italic(),
        )));
        lines.push(Line::from(""));
    }

    // ── Plan section ────────────────────────────────────────────
    if !plan.is_empty() {
        lines.push(Line::from(Span::styled(
            "Plan:",
            Style::default().fg(Color::Yellow).bold(),
        )));
        for entry in plan {
            let (icon, icon_style) = match entry.status {
                PlanEntryStatus::Pending => ("  ", Style::default().fg(Color::DarkGray)),
                PlanEntryStatus::InProgress => ("  ", Style::default().fg(Color::Yellow)),
                PlanEntryStatus::Completed => ("  ", Style::default().fg(Color::Green)),
            };
            let content_style = match entry.status {
                PlanEntryStatus::Completed => Style::default().fg(Color::Green),
                PlanEntryStatus::InProgress => Style::default().fg(Color::White),
                PlanEntryStatus::Pending => Style::default().fg(Color::DarkGray),
            };
            lines.push(Line::from(vec![
                Span::styled(icon.to_string(), icon_style),
                Span::styled(entry.content.clone(), content_style),
            ]));
        }
        lines.push(Line::from(""));
    }

    // ── Output section ──────────────────────────────────────────
    if !output.is_empty() {
        for line in output.lines() {
            lines.push(Line::from(line.to_string()));
        }
        // If the output ends with a newline, the last `lines()` call
        // won't produce a trailing empty element, but visually we want
        // the cursor to be "at the bottom" so this is fine.
    }

    // If everything is empty, show a waiting message.
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

/// Build the reviewer pane paragraph.
pub fn reviewer_paragraph<'a>(
    thinking: &'a str,
    plan: &'a [PlanEntry],
    output: &'a str,
    scroll: u16,
    focused: FocusedPane,
) -> Paragraph<'a> {
    agent_paragraph(
        "Reviewer",
        Color::Magenta,
        thinking,
        plan,
        output,
        scroll,
        focused,
        FocusedPane::Reviewer,
    )
}

/// Build the worker pane paragraph.
pub fn worker_paragraph<'a>(
    thinking: &'a str,
    plan: &'a [PlanEntry],
    output: &'a str,
    scroll: u16,
    focused: FocusedPane,
) -> Paragraph<'a> {
    agent_paragraph(
        "Worker",
        Color::Cyan,
        thinking,
        plan,
        output,
        scroll,
        focused,
        FocusedPane::Worker,
    )
}

// ---------------------------------------------------------------------------
// Tool calls pane — now shows kind badge + colored parameters
// ---------------------------------------------------------------------------

/// Format a tool kind as a short colored badge.
fn tool_kind_span(kind: &ToolKind) -> Span<'static> {
    let (label, color) = match kind {
        ToolKind::Read => ("RD", Color::Blue),
        ToolKind::Edit => ("ED", Color::Yellow),
        ToolKind::Delete => ("DL", Color::Red),
        ToolKind::Move => ("MV", Color::Magenta),
        ToolKind::Search => ("SR", Color::Cyan),
        ToolKind::Execute => ("EX", Color::Green),
        ToolKind::Think => ("TH", Color::DarkGray),
        ToolKind::Fetch => ("FT", Color::Blue),
        ToolKind::SwitchMode => ("SM", Color::Magenta),
        ToolKind::Other => ("??", Color::DarkGray),
    };
    Span::styled(format!("{label:>2}"), Style::default().fg(color).bold())
}

/// Render a JSON value into compact, colored spans for tool parameters.
///
/// Produces a flat list of spans representing key-value pairs.
/// Truncates long string values for readability.
fn json_param_spans(value: &serde_json::Value, max_width: usize) -> Vec<Span<'static>> {
    let key_style = Style::default().fg(Color::Cyan);
    let str_style = Style::default().fg(Color::Green);
    let num_style = Style::default().fg(Color::Yellow);
    let bool_style = Style::default().fg(Color::Magenta);
    let null_style = Style::default().fg(Color::DarkGray);
    let punct_style = Style::default().fg(Color::DarkGray);

    let mut spans = Vec::new();
    let mut used = 0;

    match value {
        serde_json::Value::Object(map) => {
            for (i, (k, v)) in map.iter().enumerate() {
                if used > max_width {
                    spans.push(Span::styled(" ...", punct_style));
                    break;
                }
                if i > 0 {
                    spans.push(Span::styled(" ", punct_style));
                    used += 1;
                }
                let key_text = format!("{k}=");
                used += key_text.len();
                spans.push(Span::styled(key_text, key_style));

                let (val_span, val_len) =
                    format_json_value_span(v, &str_style, &num_style, &bool_style, &null_style);
                used += val_len;
                spans.push(val_span);
            }
        }
        other => {
            // Not an object — just format the whole thing.
            let s = other.to_string();
            let truncated = truncate_str(&s, max_width);
            spans.push(Span::styled(truncated, str_style));
        }
    }

    spans
}

/// Format a single JSON value into a colored span, truncating long strings.
fn format_json_value_span(
    v: &serde_json::Value,
    str_style: &Style,
    num_style: &Style,
    bool_style: &Style,
    null_style: &Style,
) -> (Span<'static>, usize) {
    match v {
        serde_json::Value::String(s) => {
            let display = truncate_str(s, 60);
            let len = display.len();
            (Span::styled(format!("\"{display}\""), *str_style), len + 2)
        }
        serde_json::Value::Number(n) => {
            let s = n.to_string();
            let len = s.len();
            (Span::styled(s, *num_style), len)
        }
        serde_json::Value::Bool(b) => {
            let s = b.to_string();
            let len = s.len();
            (Span::styled(s, *bool_style), len)
        }
        serde_json::Value::Null => (Span::styled("null", *null_style), 4),
        serde_json::Value::Array(arr) => {
            let s = format!("[{} items]", arr.len());
            let len = s.len();
            (Span::styled(s, *null_style), len)
        }
        serde_json::Value::Object(map) => {
            let s = format!("{{{} keys}}", map.len());
            let len = s.len();
            (Span::styled(s, *null_style), len)
        }
    }
}

/// Truncate a string to `max` chars, appending "..." if truncated.
fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max.saturating_sub(3)])
    }
}

/// Build the tool calls list as styled lines with kind badges and parameters.
pub fn tool_calls_paragraph<'a>(
    tool_calls: &'a std::collections::VecDeque<ToolCallRecord>,
    scroll: u16,
    focused: FocusedPane,
) -> Paragraph<'a> {
    let block = Block::default()
        .title(" Tool Calls ")
        .borders(Borders::ALL)
        .border_style(focused_border_style(focused, FocusedPane::Tools));

    let mut lines: Vec<Line<'a>> = Vec::new();

    for tc in tool_calls {
        let status_style = match tc.status {
            ToolCallState::InProgress => Style::default().fg(Color::Yellow),
            ToolCallState::Completed => Style::default().fg(Color::Green),
            ToolCallState::Failed => Style::default().fg(Color::Red),
            ToolCallState::Other(_) => Style::default().fg(Color::DarkGray),
        };
        let status_char = match tc.status {
            ToolCallState::InProgress => "...",
            ToolCallState::Completed => " ok",
            ToolCallState::Failed => "ERR",
            ToolCallState::Other(_) => " ? ",
        };
        let source_tag = match tc.source {
            AgentSource::Worker => Span::styled("W", Style::default().fg(Color::Cyan)),
            AgentSource::Reviewer => Span::styled("R", Style::default().fg(Color::Magenta)),
        };

        // First line: timestamp source [status] kind title
        let mut header_spans = vec![
            Span::styled(
                format!("{} ", tc.timestamp),
                Style::default().fg(Color::DarkGray),
            ),
            source_tag,
            Span::raw(" "),
            Span::styled(format!("[{status_char}]"), status_style),
            Span::raw(" "),
            tool_kind_span(&tc.kind),
            Span::raw(format!(" {}", tc.title)),
        ];

        // Show file locations if available.
        if !tc.locations.is_empty() {
            header_spans.push(Span::raw("  "));
            for (i, loc) in tc.locations.iter().enumerate() {
                if i > 0 {
                    header_spans.push(Span::styled(", ", Style::default().fg(Color::DarkGray)));
                }
                let line_suffix = loc.line.map(|l| format!(":{l}")).unwrap_or_default();
                header_spans.push(Span::styled(
                    format!("{}{line_suffix}", loc.path),
                    Style::default().fg(Color::White),
                ));
            }
        }

        // If there are raw_input parameters and it's an object, show inline
        // for small payloads (only when no locations shown).
        if tc.locations.is_empty() {
            if let Some(ref input) = tc.raw_input {
                if let serde_json::Value::Object(map) = input {
                    if map.len() <= 3 {
                        header_spans.push(Span::styled("  ", Style::default().fg(Color::DarkGray)));
                        header_spans.extend(json_param_spans(input, 80));
                    }
                }
            }
        }

        lines.push(Line::from(header_spans));

        // For larger parameter sets, show them on subsequent indented lines.
        if let Some(serde_json::Value::Object(map)) = tc.raw_input.as_ref() {
            if map.len() > 3 {
                for (k, v) in map {
                    let mut param_spans = vec![
                        Span::styled("      ", Style::default()),
                        Span::styled(k.to_string(), Style::default().fg(Color::Cyan)),
                        Span::styled("=", Style::default().fg(Color::DarkGray)),
                    ];
                    let (val_span, _) = format_json_value_span(
                        v,
                        &Style::default().fg(Color::Green),
                        &Style::default().fg(Color::Yellow),
                        &Style::default().fg(Color::Magenta),
                        &Style::default().fg(Color::DarkGray),
                    );
                    param_spans.push(val_span);
                    lines.push(Line::from(param_spans));
                }
            }
        }
    }

    Paragraph::new(lines).block(block).scroll((scroll, 0))
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
    max_cycles: usize,
    focused: FocusedPane,
    abort_requested: bool,
    finished: bool,
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
            format!("cycle {cycle}/{max_cycles}"),
            Style::default().fg(Color::White),
        ));
        spans.push(Span::raw("  "));
    }

    let pane_label = match focused {
        FocusedPane::Reviewer => "Reviewer",
        FocusedPane::Worker => "Worker",
        FocusedPane::Tools => "Tools",
        FocusedPane::Log => "Log",
    };
    spans.push(Span::styled(
        format!("[{pane_label}]"),
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
    } else {
        spans.push(Span::styled(
            "Ctrl+K: kill  Ctrl+C x2: abort  Tab/Shift+Tab: pane  j/k: scroll",
            Style::default().fg(Color::DarkGray),
        ));
    }

    Line::from(spans)
}
