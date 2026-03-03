//! Application state for the TUI.

use std::collections::VecDeque;

use chrono::Local;

use crate::orchestrator::{LogLevel, OrchestratorEvent, Phase, ReviewVerdict};
use crate::protocol::{AgentEvent, ToolCallLocation, ToolCallState, ToolKind};

/// Maximum number of log lines retained.
const MAX_LOG_LINES: usize = 2000;
/// Maximum number of stream entries retained per agent.
const MAX_STREAM_ENTRIES: usize = 5000;

// ---------------------------------------------------------------------------
// Pane focus
// ---------------------------------------------------------------------------

/// Which pane is currently focused for scrolling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusedPane {
    Agent,
    Todo,
    Log,
}

impl FocusedPane {
    pub fn next(self) -> Self {
        match self {
            Self::Agent => Self::Todo,
            Self::Todo => Self::Log,
            Self::Log => Self::Agent,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::Agent => Self::Log,
            Self::Todo => Self::Agent,
            Self::Log => Self::Todo,
        }
    }
}

// ---------------------------------------------------------------------------
// Which agent's output is shown in the main pane
// ---------------------------------------------------------------------------

/// Which agent produced this event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSource {
    Worker,
    Reviewer,
}

impl AgentSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Worker => "Worker",
            Self::Reviewer => "Reviewer",
        }
    }

    pub fn toggle(self) -> Self {
        match self {
            Self::Worker => Self::Reviewer,
            Self::Reviewer => Self::Worker,
        }
    }
}

// ---------------------------------------------------------------------------
// Unified stream entry — everything the agent emits in chronological order
// ---------------------------------------------------------------------------

/// A single entry in the unified agent output stream.
#[derive(Debug, Clone)]
pub enum StreamEntry {
    /// A chunk of assistant text output.
    Text(String),
    /// A chunk of thinking/reasoning.
    Thought(String),
    /// A tool call started.
    ToolStart {
        id: String,
        title: String,
        kind: ToolKind,
        detail: String,
    },
    /// A tool call completed or failed.
    ToolEnd {
        id: String,
        title: String,
        status: ToolCallState,
    },
    /// A separator between turns / phases.
    Separator(String),
}

// ---------------------------------------------------------------------------
// Todo item (from agent's TodoWrite tool calls)
// ---------------------------------------------------------------------------

/// A single todo item parsed from the agent's TodoWrite tool calls.
#[derive(Debug, Clone)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
    pub priority: TodoPriority,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TodoPriority {
    High,
    Medium,
    Low,
}

// ---------------------------------------------------------------------------
// Log entry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub timestamp: String,
    pub level: LogLevel,
    pub message: String,
}

// ---------------------------------------------------------------------------
// Queued user message
// ---------------------------------------------------------------------------

/// A message queued by the user to be sent to the active agent.
#[derive(Debug, Clone)]
pub struct QueuedMessage {
    pub text: String,
    pub target: AgentSource,
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

/// Complete application state driving the TUI.
pub struct App {
    /// Current orchestration phase.
    pub phase: Phase,

    /// Which agent is currently displayed in the main (left) pane.
    pub active_agent: AgentSource,

    /// Unified chronological stream for the worker.
    pub worker_stream: VecDeque<StreamEntry>,
    /// Unified chronological stream for the reviewer.
    pub reviewer_stream: VecDeque<StreamEntry>,

    /// Scroll offset for the agent pane.
    pub agent_scroll: u16,
    /// Whether to auto-scroll the agent pane.
    pub auto_scroll_agent: bool,

    /// Shared todo list (updated via TodoWrite tool calls).
    pub todos: Vec<TodoItem>,
    /// Scroll offset for the todo pane.
    pub todo_scroll: u16,
    /// Whether to auto-scroll the todo pane.
    pub auto_scroll_todo: bool,

    /// Log entries from orchestrator + protocol.
    pub log_entries: VecDeque<LogEntry>,
    /// Scroll offset for the log pane.
    pub log_scroll: u16,
    /// Whether to auto-scroll the log pane.
    pub auto_scroll_log: bool,

    /// Which pane is focused.
    pub focused: FocusedPane,

    /// Current review cycle number.
    pub current_cycle: usize,

    /// Whether the user has triggered an abort.
    pub abort_requested: bool,

    /// Whether the orchestration has finished.
    pub finished: bool,

    /// Final phase (set when finished).
    pub final_phase: Option<Phase>,

    /// Number of Ctrl+C presses (double-tap to kill).
    pub ctrl_c_count: u8,

    /// Goal description (for header display).
    pub goal: String,

    /// Whether the user is currently typing a message.
    pub input_mode: bool,

    /// Current input buffer.
    pub input_buffer: String,

    /// Cursor position within the input buffer.
    pub input_cursor: usize,

    /// Queue of messages to send to agents.
    pub message_queue: VecDeque<QueuedMessage>,
}

impl App {
    pub fn new(goal: String) -> Self {
        Self {
            phase: Phase::Idle,
            active_agent: AgentSource::Reviewer,
            worker_stream: VecDeque::new(),
            reviewer_stream: VecDeque::new(),
            agent_scroll: 0,
            auto_scroll_agent: true,
            todos: Vec::new(),
            todo_scroll: 0,
            auto_scroll_todo: true,
            log_entries: VecDeque::new(),
            log_scroll: 0,
            auto_scroll_log: true,
            focused: FocusedPane::Agent,
            current_cycle: 0,
            abort_requested: false,
            finished: false,
            final_phase: None,
            ctrl_c_count: 0,
            goal,
            input_mode: false,
            input_buffer: String::new(),
            input_cursor: 0,
            message_queue: VecDeque::new(),
        }
    }

    /// Get the currently active agent's stream.
    pub fn active_stream(&self) -> &VecDeque<StreamEntry> {
        match self.active_agent {
            AgentSource::Worker => &self.worker_stream,
            AgentSource::Reviewer => &self.reviewer_stream,
        }
    }

    /// Toggle which agent is displayed in the main pane.
    pub fn toggle_agent(&mut self) {
        self.switch_to_agent(self.active_agent.toggle());
    }

    /// Switch the active agent view (only if actually changing).
    fn switch_to_agent(&mut self, agent: AgentSource) {
        if self.active_agent != agent {
            self.active_agent = agent;
            self.agent_scroll = u16::MAX;
            self.auto_scroll_agent = true;
        }
    }

    /// Submit the current input buffer as a queued message.
    pub fn submit_input(&mut self) {
        let text = self.input_buffer.trim().to_string();
        if !text.is_empty() {
            self.message_queue.push_back(QueuedMessage {
                text: text.clone(),
                target: self.active_agent,
            });
            self.push_log(
                LogLevel::Info,
                format!("[User -> {:?}] {}", self.active_agent, text),
            );
        }
        self.input_buffer.clear();
        self.input_cursor = 0;
        self.input_mode = false;
    }

    /// Process an orchestrator event and update the TUI state.
    pub fn handle_event(&mut self, event: OrchestratorEvent) {
        match event {
            OrchestratorEvent::PhaseChanged(ref phase) => {
                // Insert phase-change separator into the relevant agent stream
                // and auto-switch the active agent view to follow the action.
                let sep = format!("--- {} ---", phase_label(phase));
                match phase {
                    Phase::Working | Phase::Revising => {
                        self.current_cycle += 1;
                        self.push_stream(AgentSource::Worker, StreamEntry::Separator(sep));
                        self.switch_to_agent(AgentSource::Worker);
                    }
                    Phase::Planning | Phase::Reviewing => {
                        self.push_stream(AgentSource::Reviewer, StreamEntry::Separator(sep));
                        self.switch_to_agent(AgentSource::Reviewer);
                    }
                    _ => {}
                }
                self.phase = phase.clone();
            }
            OrchestratorEvent::WorkerEvent(agent_evt) => {
                self.handle_agent_event(agent_evt, AgentSource::Worker);
            }
            OrchestratorEvent::ReviewerEvent(agent_evt) => {
                self.handle_agent_event(agent_evt, AgentSource::Reviewer);
            }
            OrchestratorEvent::Log(level, msg) => {
                self.push_log(level, msg);
            }
            OrchestratorEvent::CycleCompleted(record) => {
                self.push_log(
                    LogLevel::Info,
                    format!(
                        "Cycle {} completed: {}",
                        record.cycle,
                        match record.verdict {
                            ReviewVerdict::Approved { .. } => "APPROVED".to_string(),
                            ReviewVerdict::NeedsRevision { .. } => "NEEDS REVISION".to_string(),
                            ReviewVerdict::Rejected { ref reason } => format!("REJECTED: {reason}"),
                        }
                    ),
                );
            }
            OrchestratorEvent::Finished(phase) => {
                self.finished = true;
                self.final_phase = Some(phase);
            }
        }
    }

    fn handle_agent_event(&mut self, event: AgentEvent, source: AgentSource) {
        match event {
            AgentEvent::TextChunk(text) => {
                // Coalesce consecutive text chunks into a single entry so the
                // renderer doesn't treat each streamed fragment as a new line.
                let stream = match source {
                    AgentSource::Worker => &mut self.worker_stream,
                    AgentSource::Reviewer => &mut self.reviewer_stream,
                };
                if let Some(StreamEntry::Text(existing)) = stream.back_mut() {
                    existing.push_str(&text);
                    // Auto-scroll if viewing this agent.
                    if source == self.active_agent && self.auto_scroll_agent {
                        self.agent_scroll = u16::MAX;
                    }
                } else {
                    self.push_stream(source, StreamEntry::Text(text));
                }
            }
            AgentEvent::ThoughtChunk(text) => {
                let stream = match source {
                    AgentSource::Worker => &mut self.worker_stream,
                    AgentSource::Reviewer => &mut self.reviewer_stream,
                };
                if let Some(StreamEntry::Thought(existing)) = stream.back_mut() {
                    existing.push_str(&text);
                    if source == self.active_agent && self.auto_scroll_agent {
                        self.agent_scroll = u16::MAX;
                    }
                } else {
                    self.push_stream(source, StreamEntry::Thought(text));
                }
            }
            AgentEvent::PlanUpdated(_entries) => {
                // Plans are shown in the stream as info, not as a separate panel.
                // We no longer use them since todo list replaces this.
            }
            AgentEvent::ToolCallStarted {
                id,
                title,
                kind,
                raw_input,
                locations,
            } => {
                let detail = Self::extract_tool_detail(&kind, &title, &locations, &raw_input);
                self.push_stream(
                    source,
                    StreamEntry::ToolStart {
                        id,
                        title,
                        kind,
                        detail,
                    },
                );
            }
            AgentEvent::ToolCallUpdated {
                id, title, status, ..
            } => {
                if status == ToolCallState::Completed || status == ToolCallState::Failed {
                    self.push_stream(
                        source,
                        StreamEntry::ToolEnd {
                            id,
                            title: title.unwrap_or_default(),
                            status,
                        },
                    );
                }
            }
            AgentEvent::TodoUpdated(items) => {
                // Replace the shared todo list.
                self.todos = items
                    .into_iter()
                    .map(|item| TodoItem {
                        content: item.content,
                        status: match item.status.as_str() {
                            "in_progress" => TodoStatus::InProgress,
                            "completed" => TodoStatus::Completed,
                            "cancelled" => TodoStatus::Cancelled,
                            _ => TodoStatus::Pending,
                        },
                        priority: match item.priority.as_str() {
                            "high" => TodoPriority::High,
                            "low" => TodoPriority::Low,
                            _ => TodoPriority::Medium,
                        },
                    })
                    .collect();
                if self.auto_scroll_todo {
                    self.todo_scroll = u16::MAX;
                }
            }
            AgentEvent::PermissionRequested { description } => {
                self.push_log(
                    LogLevel::Info,
                    format!("[{source:?}] Permission auto-approved: {description}"),
                );
            }
            AgentEvent::TurnComplete { stop_reason } => {
                self.push_log(
                    LogLevel::Info,
                    format!("[{source:?}] Turn complete: {stop_reason:?}"),
                );
            }
            AgentEvent::ProtocolMessage(msg) => {
                self.push_log(LogLevel::Info, format!("[{source:?}] {msg}"));
            }
        }
    }

    fn push_stream(&mut self, source: AgentSource, entry: StreamEntry) {
        let stream = match source {
            AgentSource::Worker => &mut self.worker_stream,
            AgentSource::Reviewer => &mut self.reviewer_stream,
        };
        stream.push_back(entry);
        if stream.len() > MAX_STREAM_ENTRIES {
            stream.pop_front();
        }
        // Auto-scroll if viewing this agent.
        if source == self.active_agent && self.auto_scroll_agent {
            self.agent_scroll = u16::MAX;
        }
    }

    fn push_log(&mut self, level: LogLevel, message: String) {
        let entry = LogEntry {
            timestamp: Local::now().format("%H:%M:%S%.3f").to_string(),
            level,
            message,
        };
        self.log_entries.push_back(entry);
        if self.log_entries.len() > MAX_LOG_LINES {
            self.log_entries.pop_front();
        }
        if self.auto_scroll_log {
            self.log_scroll = u16::MAX;
        }
    }

    // -----------------------------------------------------------------------
    // Scrolling
    // -----------------------------------------------------------------------

    /// Scroll the focused pane up by `n` lines.
    pub fn scroll_up(&mut self, n: u16) {
        match self.focused {
            FocusedPane::Agent => {
                self.agent_scroll = self.agent_scroll.saturating_sub(n);
                self.auto_scroll_agent = false;
            }
            FocusedPane::Todo => {
                self.todo_scroll = self.todo_scroll.saturating_sub(n);
                self.auto_scroll_todo = false;
            }
            FocusedPane::Log => {
                self.log_scroll = self.log_scroll.saturating_sub(n);
                self.auto_scroll_log = false;
            }
        }
    }

    /// Scroll the focused pane down by `n` lines.
    pub fn scroll_down(&mut self, n: u16) {
        match self.focused {
            FocusedPane::Agent => {
                self.agent_scroll = self.agent_scroll.saturating_add(n);
            }
            FocusedPane::Todo => {
                self.todo_scroll = self.todo_scroll.saturating_add(n);
            }
            FocusedPane::Log => {
                self.log_scroll = self.log_scroll.saturating_add(n);
            }
        }
    }

    /// Jump to the bottom of the focused pane and re-enable auto-scroll.
    pub fn scroll_to_bottom(&mut self) {
        match self.focused {
            FocusedPane::Agent => {
                self.agent_scroll = u16::MAX;
                self.auto_scroll_agent = true;
            }
            FocusedPane::Todo => {
                self.todo_scroll = u16::MAX;
                self.auto_scroll_todo = true;
            }
            FocusedPane::Log => {
                self.log_scroll = u16::MAX;
                self.auto_scroll_log = true;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Helpers for tool call detail extraction
    // -----------------------------------------------------------------------

    /// Extract detail from a tool call for inline display.
    ///
    /// Priority: file location > known key shortcuts > all params as key=value.
    /// Never falls back to just the title — always shows parameters if available.
    fn extract_tool_detail(
        kind: &ToolKind,
        title: &str,
        locations: &[ToolCallLocation],
        raw_input: &Option<serde_json::Value>,
    ) -> String {
        // For file-oriented operations, show the path.
        if let Some(loc) = locations.first() {
            let line_suffix = loc.line.map(|l| format!(":{l}")).unwrap_or_default();
            return format!("{}{line_suffix}", loc.path);
        }

        if let Some(serde_json::Value::Object(map)) = raw_input {
            // For execute, try to find a "command" key.
            if matches!(kind, ToolKind::Execute) {
                if let Some(serde_json::Value::String(cmd)) = map.get("command") {
                    return truncate(cmd, 120);
                }
            }

            // For read/edit, try "path" or "file" keys — show as primary detail.
            for key in &["path", "file", "filePath", "file_path"] {
                if let Some(serde_json::Value::String(p)) = map.get(*key) {
                    return p.clone();
                }
            }

            // Fallback: show ALL parameters as compact key=value pairs.
            if !map.is_empty() {
                return Self::format_params_inline(map);
            }
        }

        title.to_string()
    }

    /// Format a JSON object's key-value pairs as a compact inline string.
    /// e.g. `query="foo" include="*.rs" path="/src"`
    fn format_params_inline(map: &serde_json::Map<String, serde_json::Value>) -> String {
        let mut parts = Vec::new();
        let mut total_len = 0;
        let max_len = 200;

        for (k, v) in map {
            if total_len > max_len {
                parts.push("...".to_string());
                break;
            }
            let val_str = match v {
                serde_json::Value::String(s) => {
                    let t = truncate(s, 60);
                    format!("\"{t}\"")
                }
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::Bool(b) => b.to_string(),
                serde_json::Value::Null => "null".to_string(),
                serde_json::Value::Array(a) => format!("[{} items]", a.len()),
                serde_json::Value::Object(m) => format!("{{{} keys}}", m.len()),
            };
            let part = format!("{k}={val_str}");
            total_len += part.len() + 1;
            parts.push(part);
        }

        parts.join(" ")
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max.saturating_sub(3)])
    }
}

fn phase_label(phase: &Phase) -> &str {
    match phase {
        Phase::Idle => "idle",
        Phase::Initializing => "init",
        Phase::Planning => "planning",
        Phase::Working => "working",
        Phase::Reviewing => "reviewing",
        Phase::Revising => "revising",
        Phase::Approved => "approved",
        Phase::Failed(_) => "failed",
        Phase::Aborted => "aborted",
    }
}
