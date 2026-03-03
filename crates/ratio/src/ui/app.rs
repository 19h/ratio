//! Application state for the TUI.

use std::collections::VecDeque;

use chrono::Local;

use crate::orchestrator::{LogLevel, OrchestratorEvent, Phase, ReviewVerdict};
use crate::protocol::{AgentEvent, PlanEntry, ToolCallState, ToolKind};

/// Maximum number of log lines retained.
const MAX_LOG_LINES: usize = 2000;
/// Maximum number of tool call records retained.
const MAX_TOOL_CALLS: usize = 500;

// ---------------------------------------------------------------------------
// Pane focus
// ---------------------------------------------------------------------------

/// Which pane is currently focused for scrolling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusedPane {
    Reviewer,
    Worker,
    Tools,
    Log,
}

impl FocusedPane {
    pub fn next(self) -> Self {
        match self {
            Self::Reviewer => Self::Worker,
            Self::Worker => Self::Tools,
            Self::Tools => Self::Log,
            Self::Log => Self::Reviewer,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::Reviewer => Self::Log,
            Self::Worker => Self::Reviewer,
            Self::Tools => Self::Worker,
            Self::Log => Self::Tools,
        }
    }
}

// ---------------------------------------------------------------------------
// Tool call display record
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    pub id: String,
    pub title: String,
    pub kind: ToolKind,
    pub status: ToolCallState,
    pub source: AgentSource,
    pub content: Option<String>,
    pub raw_input: Option<serde_json::Value>,
    pub raw_output: Option<serde_json::Value>,
    pub timestamp: String,
}

/// Which agent produced this event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSource {
    Worker,
    Reviewer,
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
// App state
// ---------------------------------------------------------------------------

/// Complete application state driving the TUI.
pub struct App {
    /// Current orchestration phase.
    pub phase: Phase,

    /// Streaming text from the reviewer (current turn).
    pub reviewer_output: String,
    /// Streaming thinking text from the reviewer (current turn).
    pub reviewer_thinking: String,
    /// Scroll offset for the reviewer pane.
    pub reviewer_scroll: u16,

    /// Streaming text from the worker (current turn).
    pub worker_output: String,
    /// Streaming thinking text from the worker (current turn).
    pub worker_thinking: String,
    /// Scroll offset for the worker pane.
    pub worker_scroll: u16,

    /// Current plan entries from the worker agent.
    pub worker_plan: Vec<PlanEntry>,
    /// Current plan entries from the reviewer agent.
    pub reviewer_plan: Vec<PlanEntry>,

    /// Tool call records from both agents.
    pub tool_calls: VecDeque<ToolCallRecord>,
    /// Scroll offset for the tool pane.
    pub tool_scroll: u16,

    /// Log entries from orchestrator + protocol.
    pub log_entries: VecDeque<LogEntry>,
    /// Scroll offset for the log pane.
    pub log_scroll: u16,

    /// Which pane is focused.
    pub focused: FocusedPane,

    /// Current review cycle number.
    pub current_cycle: usize,

    /// Maximum configured review cycles.
    pub max_cycles: usize,

    /// Whether the user has triggered an abort.
    pub abort_requested: bool,

    /// Whether the orchestration has finished.
    pub finished: bool,

    /// Final phase (set when finished).
    pub final_phase: Option<Phase>,

    /// Whether to auto-scroll the reviewer pane.
    pub auto_scroll_reviewer: bool,

    /// Whether to auto-scroll the worker pane.
    pub auto_scroll_worker: bool,

    /// Whether to auto-scroll the tools pane.
    pub auto_scroll_tools: bool,

    /// Whether to auto-scroll the log pane.
    pub auto_scroll_log: bool,

    /// Number of Ctrl+C presses (double-tap to kill).
    pub ctrl_c_count: u8,

    /// Goal description (for header display).
    pub goal: String,
}

impl App {
    pub fn new(goal: String, max_cycles: usize) -> Self {
        Self {
            phase: Phase::Idle,
            reviewer_output: String::new(),
            reviewer_thinking: String::new(),
            reviewer_scroll: 0,
            worker_output: String::new(),
            worker_thinking: String::new(),
            worker_scroll: 0,
            worker_plan: Vec::new(),
            reviewer_plan: Vec::new(),
            tool_calls: VecDeque::new(),
            tool_scroll: 0,
            log_entries: VecDeque::new(),
            log_scroll: 0,
            focused: FocusedPane::Worker,
            current_cycle: 0,
            max_cycles,
            abort_requested: false,
            finished: false,
            final_phase: None,
            auto_scroll_reviewer: true,
            auto_scroll_worker: true,
            auto_scroll_tools: true,
            auto_scroll_log: true,
            ctrl_c_count: 0,
            goal,
        }
    }

    /// Process an orchestrator event and update the TUI state.
    pub fn handle_event(&mut self, event: OrchestratorEvent) {
        match event {
            OrchestratorEvent::PhaseChanged(ref phase) => {
                match phase {
                    Phase::Working | Phase::Revising => {
                        self.current_cycle += 1;
                        // Clear worker output for new cycle.
                        self.worker_output.clear();
                        self.worker_thinking.clear();
                        self.worker_scroll = 0;
                    }
                    Phase::Planning => {
                        self.reviewer_output.clear();
                        self.reviewer_thinking.clear();
                        self.reviewer_scroll = 0;
                    }
                    Phase::Reviewing => {
                        // Clear reviewer output for new review cycle.
                        self.reviewer_output.clear();
                        self.reviewer_thinking.clear();
                        self.reviewer_scroll = 0;
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
                        "Cycle {} completed — verdict: {}",
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
            AgentEvent::TextChunk(text) => match source {
                AgentSource::Worker => {
                    self.worker_output.push_str(&text);
                    if self.auto_scroll_worker {
                        self.worker_scroll = u16::MAX;
                    }
                }
                AgentSource::Reviewer => {
                    self.reviewer_output.push_str(&text);
                    if self.auto_scroll_reviewer {
                        self.reviewer_scroll = u16::MAX;
                    }
                }
            },
            AgentEvent::ThoughtChunk(text) => match source {
                AgentSource::Worker => {
                    self.worker_thinking.push_str(&text);
                    if self.auto_scroll_worker {
                        self.worker_scroll = u16::MAX;
                    }
                }
                AgentSource::Reviewer => {
                    self.reviewer_thinking.push_str(&text);
                    if self.auto_scroll_reviewer {
                        self.reviewer_scroll = u16::MAX;
                    }
                }
            },
            AgentEvent::PlanUpdated(entries) => {
                match source {
                    AgentSource::Worker => self.worker_plan = entries,
                    AgentSource::Reviewer => self.reviewer_plan = entries,
                }
                if self.auto_scroll_worker && source == AgentSource::Worker {
                    self.worker_scroll = u16::MAX;
                }
                if self.auto_scroll_reviewer && source == AgentSource::Reviewer {
                    self.reviewer_scroll = u16::MAX;
                }
            }
            AgentEvent::ToolCallStarted {
                id,
                title,
                kind,
                raw_input,
            } => {
                let record = ToolCallRecord {
                    id,
                    title,
                    kind,
                    status: ToolCallState::InProgress,
                    source,
                    content: None,
                    raw_input,
                    raw_output: None,
                    timestamp: Local::now().format("%H:%M:%S").to_string(),
                };
                self.tool_calls.push_back(record);
                if self.tool_calls.len() > MAX_TOOL_CALLS {
                    self.tool_calls.pop_front();
                }
                if self.auto_scroll_tools {
                    self.tool_scroll = u16::MAX;
                }
            }
            AgentEvent::ToolCallUpdated {
                id,
                status,
                content,
                raw_output,
            } => {
                if let Some(tc) = self.tool_calls.iter_mut().rev().find(|tc| tc.id == id) {
                    tc.status = status;
                    if content.is_some() {
                        tc.content = content;
                    }
                    if raw_output.is_some() {
                        tc.raw_output = raw_output;
                    }
                }
                if self.auto_scroll_tools {
                    self.tool_scroll = u16::MAX;
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

    /// Scroll the focused pane up by `n` lines.
    pub fn scroll_up(&mut self, n: u16) {
        match self.focused {
            FocusedPane::Reviewer => {
                self.reviewer_scroll = self.reviewer_scroll.saturating_sub(n);
                self.auto_scroll_reviewer = false;
            }
            FocusedPane::Worker => {
                self.worker_scroll = self.worker_scroll.saturating_sub(n);
                self.auto_scroll_worker = false;
            }
            FocusedPane::Tools => {
                self.tool_scroll = self.tool_scroll.saturating_sub(n);
                self.auto_scroll_tools = false;
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
            FocusedPane::Reviewer => {
                self.reviewer_scroll = self.reviewer_scroll.saturating_add(n);
            }
            FocusedPane::Worker => {
                self.worker_scroll = self.worker_scroll.saturating_add(n);
            }
            FocusedPane::Tools => {
                self.tool_scroll = self.tool_scroll.saturating_add(n);
            }
            FocusedPane::Log => {
                self.log_scroll = self.log_scroll.saturating_add(n);
            }
        }
    }

    /// Jump to the bottom of the focused pane and re-enable auto-scroll.
    pub fn scroll_to_bottom(&mut self) {
        match self.focused {
            FocusedPane::Reviewer => {
                self.reviewer_scroll = u16::MAX;
                self.auto_scroll_reviewer = true;
            }
            FocusedPane::Worker => {
                self.worker_scroll = u16::MAX;
                self.auto_scroll_worker = true;
            }
            FocusedPane::Tools => {
                self.tool_scroll = u16::MAX;
                self.auto_scroll_tools = true;
            }
            FocusedPane::Log => {
                self.log_scroll = u16::MAX;
                self.auto_scroll_log = true;
            }
        }
    }
}
