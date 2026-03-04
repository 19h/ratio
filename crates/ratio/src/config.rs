//! Configuration system for the orchestrator.
//!
//! Defines user-specified constraints on which tools, approaches, and behaviors
//! agents must follow. Configuration can be loaded from TOML files or constructed
//! programmatically via CLI flags.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Top-level configuration
// ---------------------------------------------------------------------------

/// Root configuration for a ratio orchestration session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The goal to accomplish — a detailed natural-language description.
    pub goal: String,

    /// Working directory both agents operate in.
    pub cwd: PathBuf,

    /// Worker agent configuration (the agent doing the actual coding work).
    pub worker: AgentConfig,

    /// Reviewer agent configuration (the agent reviewing the worker's output).
    pub reviewer: AgentConfig,

    /// Additional stakeholder personas that participate in planning and/or review.
    #[serde(default)]
    pub stakeholders: Vec<StakeholderConfig>,

    /// Orchestration behavior.
    pub orchestration: OrchestrationConfig,

    /// Constraints enforced on the worker agent.
    pub constraints: Constraints,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            goal: String::new(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            worker: AgentConfig::default_worker(),
            reviewer: AgentConfig::default_reviewer(),
            stakeholders: Vec::new(),
            orchestration: OrchestrationConfig::default(),
            constraints: Constraints::default(),
        }
    }
}

impl Config {
    /// Load configuration from a TOML file, falling back to defaults for
    /// any missing fields.
    pub fn from_file(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&content)?;
        Ok(config)
    }

    /// Merge CLI overrides into the loaded configuration.
    pub fn apply_overrides(&mut self, overrides: &CliOverrides) {
        if let Some(ref goal) = overrides.goal {
            self.goal.clone_from(goal);
        }
        if let Some(ref cwd) = overrides.cwd {
            self.cwd.clone_from(cwd);
        }
        if let Some(ref model) = overrides.worker_model {
            self.worker.model.clone_from(model);
        }
        if let Some(ref model) = overrides.reviewer_model {
            self.reviewer.model.clone_from(model);
        }
        if let Some(max) = overrides.max_iterations {
            self.orchestration.max_review_cycles = max;
        }
    }

    /// Validate that the configuration is usable.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.goal.is_empty(),
            "A goal must be specified (--goal or config file)"
        );
        // No cycle limit validation — cycles are unlimited by default.
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Agent settings (shared between worker and reviewer)
// ---------------------------------------------------------------------------

/// Configuration for an opencode agent subprocess.
///
/// Both the worker and the reviewer are opencode instances; this struct
/// captures the per-agent settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    /// Path or command name for the opencode binary.
    pub binary: String,

    /// LLM model identifier (e.g. `anthropic/claude-sonnet-4-5`).
    pub model: String,

    /// Extra environment variables passed to the agent process.
    pub env: Vec<EnvVar>,

    /// Custom agent name within opencode (--agent flag).
    pub agent: Option<String>,

    /// Additional CLI arguments forwarded verbatim.
    pub extra_args: Vec<String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            binary: "opencode".to_string(),
            model: String::new(),
            env: Vec::new(),
            agent: None,
            extra_args: Vec::new(),
        }
    }
}

impl AgentConfig {
    /// Sensible defaults for the worker agent.
    fn default_worker() -> Self {
        Self::default()
    }

    /// Sensible defaults for the reviewer agent.
    fn default_reviewer() -> Self {
        Self::default()
    }
}

/// A key=value pair injected into an agent's environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
}

// ---------------------------------------------------------------------------
// Stakeholder configuration
// ---------------------------------------------------------------------------

/// Which orchestration phases a stakeholder participates in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StakeholderPhase {
    Planning,
    Review,
}

/// Configuration for a stakeholder persona.
///
/// Each stakeholder gets its own opencode subprocess with a clean context.
/// During the phases they participate in, they are prompted with the current
/// context and asked for input from their unique perspective.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StakeholderConfig {
    /// Human-readable name (e.g. "Reverse Engineer", "CEO").
    pub name: String,

    /// The persona prompt — describes who this stakeholder is, what they
    /// care about, and how they evaluate things.
    pub persona: String,

    /// Which phases this stakeholder participates in.
    /// Defaults to both planning and review.
    #[serde(default = "default_stakeholder_phases")]
    pub phases: Vec<StakeholderPhase>,

    /// Agent subprocess configuration (binary, model, env, etc.).
    /// Falls back to the reviewer's config if not specified.
    #[serde(default)]
    pub agent: Option<AgentConfig>,
}

fn default_stakeholder_phases() -> Vec<StakeholderPhase> {
    vec![StakeholderPhase::Planning, StakeholderPhase::Review]
}

// ---------------------------------------------------------------------------
// Orchestration behavior
// ---------------------------------------------------------------------------

/// Controls the orchestration loop behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OrchestrationConfig {
    /// Maximum number of review-revise cycles before the orchestrator gives up.
    pub max_review_cycles: usize,

    /// Custom system-level instructions prepended to the reviewer's prompts.
    pub reviewer_system_prompt: Option<String>,

    /// Custom system-level instructions prepended to the worker's prompts.
    pub worker_system_prompt: Option<String>,
}

impl Default for OrchestrationConfig {
    fn default() -> Self {
        Self {
            max_review_cycles: 5,
            reviewer_system_prompt: None,
            worker_system_prompt: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Constraints (enforced approaches / tools)
// ---------------------------------------------------------------------------

/// Hard constraints the user places on the worker agent's behavior.
///
/// These are compiled into the task prompt and validated during review.
/// The reviewer agent receives these as part of its review context so it
/// can verify compliance.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Constraints {
    /// Tools the agent is *required* to use (e.g. `["cargo clippy", "cargo test"]`).
    pub required_tools: Vec<String>,

    /// Tools the agent is explicitly *forbidden* from using.
    pub forbidden_tools: Vec<String>,

    /// Coding approaches or patterns the agent must follow.
    /// Free-form text entries that get injected into the prompt.
    pub required_approaches: Vec<String>,

    /// Patterns or approaches the agent must avoid.
    pub forbidden_approaches: Vec<String>,

    /// File paths the agent is allowed to modify (empty = unrestricted).
    pub allowed_paths: Vec<String>,

    /// File paths the agent must not touch.
    pub forbidden_paths: Vec<String>,

    /// Custom rules expressed as free-form sentences.
    pub custom_rules: Vec<String>,
}

impl Constraints {
    /// Render all constraints into a structured text block suitable for
    /// inclusion in a prompt.
    pub fn render_prompt_section(&self) -> String {
        let mut sections = Vec::new();

        if !self.required_tools.is_empty() {
            sections.push(format!(
                "REQUIRED TOOLS — you MUST use each of these:\n{}",
                self.required_tools
                    .iter()
                    .map(|t| format!("  - {t}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }

        if !self.forbidden_tools.is_empty() {
            sections.push(format!(
                "FORBIDDEN TOOLS — you MUST NOT use any of these:\n{}",
                self.forbidden_tools
                    .iter()
                    .map(|t| format!("  - {t}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }

        if !self.required_approaches.is_empty() {
            sections.push(format!(
                "REQUIRED APPROACHES:\n{}",
                self.required_approaches
                    .iter()
                    .map(|a| format!("  - {a}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }

        if !self.forbidden_approaches.is_empty() {
            sections.push(format!(
                "FORBIDDEN APPROACHES:\n{}",
                self.forbidden_approaches
                    .iter()
                    .map(|a| format!("  - {a}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }

        if !self.allowed_paths.is_empty() {
            sections.push(format!(
                "ALLOWED PATHS (only these may be modified):\n{}",
                self.allowed_paths
                    .iter()
                    .map(|p| format!("  - {p}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }

        if !self.forbidden_paths.is_empty() {
            sections.push(format!(
                "FORBIDDEN PATHS (must NOT be modified):\n{}",
                self.forbidden_paths
                    .iter()
                    .map(|p| format!("  - {p}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }

        if !self.custom_rules.is_empty() {
            sections.push(format!(
                "CUSTOM RULES:\n{}",
                self.custom_rules
                    .iter()
                    .map(|r| format!("  - {r}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }

        if sections.is_empty() {
            return String::new();
        }

        format!(
            "═══ ENFORCED CONSTRAINTS ═══\n\n{}\n\n═══ END CONSTRAINTS ═══",
            sections.join("\n\n")
        )
    }
}

// ---------------------------------------------------------------------------
// CLI overrides (merged on top of file config)
// ---------------------------------------------------------------------------

/// Values that can be overridden from the command line.
#[derive(Debug, Default)]
pub struct CliOverrides {
    pub goal: Option<String>,
    pub cwd: Option<PathBuf>,
    pub worker_model: Option<String>,
    pub reviewer_model: Option<String>,
    pub max_iterations: Option<usize>,
}
