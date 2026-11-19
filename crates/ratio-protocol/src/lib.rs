use serde::{Deserialize, Serialize};

mod plan;
pub use plan::{Task, TaskGraph};

/// A durable checkpoint and observations committed by the engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEvent {
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub run_id: String,
    pub phase: String,
    pub cycle: usize,
    pub checkpoint: String,
    pub effects: Vec<Effect>,
    pub ipc: Vec<IpcEvent>,
    pub workflow: Workflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Queued,
    Delivered,
    Dropped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcEvent {
    pub from: String,
    pub to: Vec<String>,
    pub sequence: usize,
    pub reply_to: Option<String>,
    pub delivery: Delivery,
    pub body: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workflow {
    pub plan_attempt: usize,
    pub parent_run: Option<String>,
    pub plan_id: Option<String>,
    pub acknowledgements: Vec<String>,
    pub tasks: Vec<TaskView>,
    pub verification: Option<VerificationView>,
    pub agents: Vec<AgentView>,
    pub context: Vec<ContextReceipt>,
    pub verdict: Option<String>,
    pub stopped: Option<RunStop>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStop {
    Interrupted,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextReceipt {
    pub actor: String,
    pub folded_groups: usize,
    pub before_tokens: usize,
    pub after_tokens: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentView {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskView {
    pub task: Task,
    pub state: TaskState,
    pub artifact_sha256: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    AwaitingAck,
    Blocked,
    Ready,
    Working,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationView {
    pub candidate: String,
    pub passed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectState {
    Intended,
    Recorded,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Effect {
    pub actor: String,
    pub attempt: usize,
    pub round: usize,
    pub index: usize,
    pub tool: String,
    pub state: EffectState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    pub run_id: String,
    pub phase: String,
    pub cycle: usize,
    pub first_sequence: u64,
    pub last_sequence: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunMetrics {
    pub provider_attempts: u64,
    pub metered_tokens: u64,
    pub active_ms: u64,
    pub tool_intents: usize,
    pub tool_receipts: usize,
    pub unknown_tool_outcomes: usize,
    pub ipc_deliveries: usize,
    pub ipc_drops: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Evaluation {
    pub phase: String,
    pub require_verified_candidate: bool,
    pub min_ipc_deliveries: usize,
    pub max_unknown_tool_outcomes: usize,
    pub max_provider_attempts: Option<u64>,
    pub max_metered_tokens: Option<u64>,
    pub max_active_ms: Option<u64>,
}

impl Default for Evaluation {
    fn default() -> Self {
        Self {
            phase: "Approved".into(),
            require_verified_candidate: false,
            min_ipc_deliveries: 0,
            max_unknown_tool_outcomes: 0,
            max_provider_attempts: None,
            max_metered_tokens: None,
            max_active_ms: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlAction {
    Status,
    Pause,
    Resume,
    Cancel,
    Guide,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlRequest {
    pub action: ControlAction,
    pub guidance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlReply {
    pub pid: u32,
    pub run_id: String,
    pub paused: bool,
    pub accepted: bool,
    pub detail: String,
}
