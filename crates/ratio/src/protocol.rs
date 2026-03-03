//! ACP protocol integration layer.
//!
//! Implements the [`acp::Client`] trait so that the orchestrator can act as an
//! ACP client connected to an opencode agent subprocess. Session notifications
//! (streaming text, tool calls, etc.) are forwarded to the UI via channels.

use std::cell::RefCell;
use std::rc::Rc;

use agent_client_protocol as acp;
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Events emitted by the protocol layer
// ---------------------------------------------------------------------------

/// Events forwarded from the ACP connection to the orchestrator / UI.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A chunk of text streamed from the agent.
    TextChunk(String),

    /// A chunk of thinking/reasoning streamed from the agent.
    ThoughtChunk(String),

    /// The agent started a new tool call.
    ToolCallStarted {
        id: String,
        title: String,
        kind: ToolKind,
        raw_input: Option<serde_json::Value>,
        locations: Vec<ToolCallLocation>,
    },

    /// A tool call was updated (progress, completion, etc.).
    ToolCallUpdated {
        id: String,
        title: Option<String>,
        status: ToolCallState,
        content: Option<String>,
        raw_input: Option<serde_json::Value>,
        raw_output: Option<serde_json::Value>,
        locations: Vec<ToolCallLocation>,
    },

    /// The agent's plan was updated (list of tasks with status).
    PlanUpdated(Vec<PlanEntry>),

    /// The agent updated the shared todo list (via TodoWrite tool call).
    TodoUpdated(Vec<TodoEntry>),

    /// The agent requested permission to perform an action.
    PermissionRequested {
        description: String,
    },

    /// The agent's prompt turn ended.
    TurnComplete {
        stop_reason: StopReason,
    },

    /// Raw protocol-level message (for the debug pane).
    ProtocolMessage(String),
}

/// Simplified tool kind for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    SwitchMode,
    Todo,
    Other,
}

/// A single entry from the agent's plan.
#[derive(Debug, Clone)]
pub struct PlanEntry {
    pub content: String,
    pub status: PlanEntryStatus,
    pub priority: PlanEntryPriority,
}

/// Status of a plan entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanEntryStatus {
    Pending,
    InProgress,
    Completed,
}

/// Priority of a plan entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanEntryPriority {
    High,
    Medium,
    Low,
}

/// A file location referenced by a tool call.
#[derive(Debug, Clone)]
pub struct ToolCallLocation {
    pub path: String,
    pub line: Option<u32>,
}

/// Simplified tool-call status for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCallState {
    InProgress,
    Completed,
    Failed,
    Other(String),
}

/// Why the agent stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    Cancelled,
    Other(String),
}

/// A single todo entry as parsed from a TodoWrite tool call.
#[derive(Debug, Clone)]
pub struct TodoEntry {
    pub content: String,
    pub status: String,
    pub priority: String,
}

// ---------------------------------------------------------------------------
// ACP Client implementation
// ---------------------------------------------------------------------------

/// The orchestrator's ACP client handler.
///
/// Receives notifications and requests from the opencode subprocess and
/// forwards them as [`AgentEvent`]s through an mpsc channel.
pub struct OrchestratorClient {
    /// Channel to send events to the orchestrator core / UI.
    event_tx: mpsc::UnboundedSender<AgentEvent>,

    /// Accumulated full text from the agent's current turn.
    accumulated_text: RefCell<String>,

    /// Auto-approve permissions (when running in unattended mode).
    auto_approve: bool,
}

impl std::fmt::Debug for OrchestratorClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrchestratorClient")
            .field("auto_approve", &self.auto_approve)
            .finish()
    }
}

impl OrchestratorClient {
    pub fn new(event_tx: mpsc::UnboundedSender<AgentEvent>, auto_approve: bool) -> Self {
        Self {
            event_tx,
            accumulated_text: RefCell::new(String::new()),
            auto_approve,
        }
    }

    /// Return the full text accumulated during the current turn, then clear it.
    pub fn take_accumulated_text(&self) -> String {
        self.accumulated_text.borrow_mut().split_off(0)
    }

    fn emit(&self, event: AgentEvent) {
        // Best-effort: if the receiver is gone we just drop the event.
        let _ = self.event_tx.send(event);
    }

    fn extract_content_text(content: &acp::ContentBlock) -> String {
        match content {
            acp::ContentBlock::Text(t) => t.text.clone(),
            acp::ContentBlock::Image(_) => "[image]".to_string(),
            acp::ContentBlock::Audio(_) => "[audio]".to_string(),
            acp::ContentBlock::ResourceLink(r) => format!("[link: {}]", r.uri),
            acp::ContentBlock::Resource(_) => "[resource]".to_string(),
            _ => "[unknown content]".to_string(),
        }
    }

    fn map_tool_kind(kind: &acp::ToolKind) -> ToolKind {
        match kind {
            acp::ToolKind::Read => ToolKind::Read,
            acp::ToolKind::Edit => ToolKind::Edit,
            acp::ToolKind::Delete => ToolKind::Delete,
            acp::ToolKind::Move => ToolKind::Move,
            acp::ToolKind::Search => ToolKind::Search,
            acp::ToolKind::Execute => ToolKind::Execute,
            acp::ToolKind::Think => ToolKind::Think,
            acp::ToolKind::Fetch => ToolKind::Fetch,
            acp::ToolKind::SwitchMode => ToolKind::SwitchMode,
            _ => ToolKind::Other,
        }
    }

    fn map_plan_status(status: &acp::PlanEntryStatus) -> PlanEntryStatus {
        match status {
            acp::PlanEntryStatus::Pending => PlanEntryStatus::Pending,
            acp::PlanEntryStatus::InProgress => PlanEntryStatus::InProgress,
            acp::PlanEntryStatus::Completed => PlanEntryStatus::Completed,
            _ => PlanEntryStatus::Pending,
        }
    }

    fn map_plan_priority(priority: &acp::PlanEntryPriority) -> PlanEntryPriority {
        match priority {
            acp::PlanEntryPriority::High => PlanEntryPriority::High,
            acp::PlanEntryPriority::Medium => PlanEntryPriority::Medium,
            acp::PlanEntryPriority::Low => PlanEntryPriority::Low,
            _ => PlanEntryPriority::Medium,
        }
    }

    fn map_locations(locations: &[acp::ToolCallLocation]) -> Vec<ToolCallLocation> {
        locations
            .iter()
            .map(|loc| ToolCallLocation {
                path: loc.path.to_string_lossy().to_string(),
                line: loc.line,
            })
            .collect()
    }

    /// Check if a tool call looks like a TodoWrite and try to extract todo items.
    ///
    /// The agent's TodoWrite calls have `raw_input` as a JSON object with a
    /// `todos` key containing an array of `{ content, status, priority }`.
    fn try_extract_todos(raw_input: &Option<serde_json::Value>) -> Option<Vec<TodoEntry>> {
        let input = raw_input.as_ref()?;
        let obj = input.as_object()?;
        let todos_val = obj.get("todos")?;
        let todos_arr = todos_val.as_array()?;

        let mut entries = Vec::new();
        for item in todos_arr {
            let obj = item.as_object()?;
            let content = obj.get("content")?.as_str()?.to_string();
            let status = obj
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("pending")
                .to_string();
            let priority = obj
                .get("priority")
                .and_then(|v| v.as_str())
                .unwrap_or("medium")
                .to_string();
            entries.push(TodoEntry {
                content,
                status,
                priority,
            });
        }
        Some(entries)
    }

    /// Check if this is a TodoWrite tool call by its title.
    fn is_todo_tool(title: &str) -> bool {
        let lower = title.to_lowercase();
        lower.contains("todowrite") || lower.contains("todo_write") || lower.contains("todo write")
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Client for OrchestratorClient {
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        // Extract a description from the tool_call's fields.
        let description = args
            .tool_call
            .fields
            .title
            .as_deref()
            .unwrap_or("<unknown action>")
            .to_string();

        self.emit(AgentEvent::PermissionRequested {
            description: description.clone(),
        });

        if self.auto_approve {
            // Find the first "allow" option, or just pick the first option.
            let option = args
                .options
                .iter()
                .find(|o| {
                    matches!(
                        o.kind,
                        acp::PermissionOptionKind::AllowOnce
                            | acp::PermissionOptionKind::AllowAlways
                    )
                })
                .or_else(|| args.options.first());

            if let Some(opt) = option {
                Ok(acp::RequestPermissionResponse::new(
                    acp::RequestPermissionOutcome::Selected(
                        acp::SelectedPermissionOutcome::new(opt.option_id.clone()),
                    ),
                ))
            } else {
                Ok(acp::RequestPermissionResponse::new(
                    acp::RequestPermissionOutcome::Cancelled,
                ))
            }
        } else {
            // In interactive mode we currently auto-approve.
            let option = args.options.first();
            if let Some(opt) = option {
                Ok(acp::RequestPermissionResponse::new(
                    acp::RequestPermissionOutcome::Selected(
                        acp::SelectedPermissionOutcome::new(opt.option_id.clone()),
                    ),
                ))
            } else {
                Ok(acp::RequestPermissionResponse::new(
                    acp::RequestPermissionOutcome::Cancelled,
                ))
            }
        }
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
        match args.update {
            acp::SessionUpdate::AgentMessageChunk(chunk) => {
                let text = Self::extract_content_text(&chunk.content);
                self.accumulated_text.borrow_mut().push_str(&text);
                self.emit(AgentEvent::TextChunk(text));
            }
            acp::SessionUpdate::AgentThoughtChunk(chunk) => {
                let text = Self::extract_content_text(&chunk.content);
                self.emit(AgentEvent::ThoughtChunk(text));
            }
            acp::SessionUpdate::ToolCall(tc) => {
                let id = tc.tool_call_id.0.to_string();
                let title = tc.title.clone();
                let kind = if Self::is_todo_tool(&title) {
                    ToolKind::Todo
                } else {
                    Self::map_tool_kind(&tc.kind)
                };
                let raw_input = tc.raw_input.clone();
                let locations = Self::map_locations(&tc.locations);

                // If this is a TodoWrite call, extract the todo items.
                if kind == ToolKind::Todo {
                    if let Some(entries) = Self::try_extract_todos(&raw_input) {
                        self.emit(AgentEvent::TodoUpdated(entries));
                    }
                }

                self.emit(AgentEvent::ToolCallStarted {
                    id,
                    title,
                    kind,
                    raw_input,
                    locations,
                });
            }
            acp::SessionUpdate::ToolCallUpdate(update) => {
                let id = update.tool_call_id.0.to_string();
                let status = match update.fields.status {
                    Some(acp::ToolCallStatus::InProgress) | None => ToolCallState::InProgress,
                    Some(acp::ToolCallStatus::Completed) => ToolCallState::Completed,
                    Some(acp::ToolCallStatus::Failed) => ToolCallState::Failed,
                    Some(other) => ToolCallState::Other(format!("{other:?}")),
                };

                // Extract text content from tool call content blocks.
                let content = update.fields.content.as_ref().and_then(|blocks| {
                    blocks.iter().find_map(|b| {
                        if let acp::ToolCallContent::Content(c) = b {
                            if let acp::ContentBlock::Text(t) = &c.content {
                                return Some(t.text.clone());
                            }
                        }
                        None
                    })
                });

                let title = update.fields.title.clone();
                let raw_input = update.fields.raw_input.clone();
                let raw_output = update.fields.raw_output.clone();
                let locations = update
                    .fields
                    .locations
                    .as_deref()
                    .map_or_else(Vec::new, Self::map_locations);

                // Check if this update includes new todo data.
                if let Some(ref t) = title {
                    if Self::is_todo_tool(t) {
                        if let Some(entries) = Self::try_extract_todos(&raw_input) {
                            self.emit(AgentEvent::TodoUpdated(entries));
                        }
                    }
                }

                self.emit(AgentEvent::ToolCallUpdated {
                    id,
                    title,
                    status,
                    content,
                    raw_input,
                    raw_output,
                    locations,
                });
            }
            acp::SessionUpdate::Plan(plan) => {
                let entries = plan
                    .entries
                    .iter()
                    .map(|e| PlanEntry {
                        content: e.content.clone(),
                        status: Self::map_plan_status(&e.status),
                        priority: Self::map_plan_priority(&e.priority),
                    })
                    .collect();
                self.emit(AgentEvent::PlanUpdated(entries));
            }
            _ => {
                // Forward as a debug message.
                self.emit(AgentEvent::ProtocolMessage(format!(
                    "session update: {:?}",
                    args.session_id
                )));
            }
        }
        Ok(())
    }

    async fn read_text_file(
        &self,
        args: acp::ReadTextFileRequest,
    ) -> acp::Result<acp::ReadTextFileResponse> {
        // Serve file reads from the local filesystem.
        let path = args.path;
        match std::fs::read_to_string(&path) {
            Ok(content) => Ok(acp::ReadTextFileResponse::new(content)),
            Err(e) => Err(acp::Error::internal_error().data(format!("read {path:?}: {e}"))),
        }
    }

    async fn write_text_file(
        &self,
        args: acp::WriteTextFileRequest,
    ) -> acp::Result<acp::WriteTextFileResponse> {
        let path = &args.path;
        // Ensure parent directory exists.
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(path, &args.content) {
            Ok(()) => Ok(acp::WriteTextFileResponse::new()),
            Err(e) => Err(acp::Error::internal_error().data(format!("write {path:?}: {e}"))),
        }
    }
}

// ---------------------------------------------------------------------------
// Connection handle
// ---------------------------------------------------------------------------

/// A live ACP connection to an agent subprocess.
///
/// Wraps [`acp::ClientSideConnection`] and provides ergonomic methods
/// for the orchestrator to drive the agent through the review loop.
pub struct WorkerConnection {
    conn: acp::ClientSideConnection,
    session_id: Option<acp::SessionId>,
    client: Rc<OrchestratorClient>,
}

impl WorkerConnection {
    pub fn new(conn: acp::ClientSideConnection, client: Rc<OrchestratorClient>) -> Self {
        Self {
            conn,
            session_id: None,
            client,
        }
    }

    /// Perform ACP handshake: initialize + new_session.
    pub async fn handshake(&mut self, cwd: &std::path::Path) -> anyhow::Result<()> {
        use acp::Agent as _;

        self.conn
            .initialize(
                acp::InitializeRequest::new(acp::ProtocolVersion::LATEST)
                    .client_info(
                        acp::Implementation::new(
                            "ratio-orchestrator",
                            env!("CARGO_PKG_VERSION"),
                        )
                        .title("Ratio Orchestrator"),
                    ),
            )
            .await
            .map_err(|e| anyhow::anyhow!("ACP initialize failed: {e}"))?;

        let response = self
            .conn
            .new_session(acp::NewSessionRequest::new(cwd))
            .await
            .map_err(|e| anyhow::anyhow!("ACP new_session failed: {e}"))?;

        self.session_id = Some(response.session_id);
        Ok(())
    }

    /// Return the current session ID, if established.
    pub fn session_id(&self) -> Option<&acp::SessionId> {
        self.session_id.as_ref()
    }

    /// Load an existing session by ID (for `--resume`).
    ///
    /// Performs ACP `initialize` + `load_session` instead of `new_session`.
    /// The agent will replay conversation history as session notifications.
    pub async fn load_existing_session(
        &mut self,
        session_id: String,
        cwd: &std::path::Path,
    ) -> anyhow::Result<()> {
        use acp::Agent as _;

        self.conn
            .initialize(
                acp::InitializeRequest::new(acp::ProtocolVersion::LATEST)
                    .client_info(
                        acp::Implementation::new(
                            "ratio-orchestrator",
                            env!("CARGO_PKG_VERSION"),
                        )
                        .title("Ratio Orchestrator"),
                    ),
            )
            .await
            .map_err(|e| anyhow::anyhow!("ACP initialize failed: {e}"))?;

        let sid: acp::SessionId = session_id.into();

        self.conn
            .load_session(acp::LoadSessionRequest::new(sid.clone(), cwd))
            .await
            .map_err(|e| anyhow::anyhow!("ACP load_session failed: {e}"))?;

        self.session_id = Some(sid);
        Ok(())
    }

    /// Send a prompt to the agent and wait for the turn to complete.
    ///
    /// Returns the stop reason and the full accumulated text from the turn.
    pub async fn prompt(&self, text: &str) -> anyhow::Result<(StopReason, String)> {
        use acp::Agent as _;

        let session_id = self
            .session_id
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no active session — call handshake() first"))?
            .clone();

        // Clear any leftover accumulated text.
        let _ = self.client.take_accumulated_text();

        let response = self
            .conn
            .prompt(acp::PromptRequest::new(
                session_id,
                vec![text.into()],
            ))
            .await
            .map_err(|e| anyhow::anyhow!("ACP prompt failed: {e}"))?;

        let stop = match response.stop_reason {
            acp::StopReason::EndTurn => StopReason::EndTurn,
            acp::StopReason::Cancelled => StopReason::Cancelled,
            _ => StopReason::Other(format!("{:?}", response.stop_reason)),
        };

        let full_text = self.client.take_accumulated_text();
        Ok((stop, full_text))
    }

    /// Send a cancel notification to abort the current turn.
    pub async fn cancel(&self) -> anyhow::Result<()> {
        use acp::Agent as _;

        if let Some(ref sid) = self.session_id {
            self.conn
                .cancel(acp::CancelNotification::new(sid.clone()))
                .await
                .map_err(|e| anyhow::anyhow!("ACP cancel failed: {e}"))?;
        }
        Ok(())
    }

    /// Subscribe to the raw message stream (for debug display).
    pub fn subscribe(&self) -> acp::StreamReceiver {
        self.conn.subscribe()
    }
}
