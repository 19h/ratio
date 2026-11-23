use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    path::PathBuf,
};

use anyhow::{Context, Result, ensure};
use clap::{CommandFactory, FromArgMatches, Parser, ValueEnum};
use serde::{Deserialize, Serialize};

#[derive(Parser)]
#[command(
    version,
    about = "Plan, implement and review a coding goal with local tools"
)]
pub struct Cli {
    /// Serve the workspace's live and historical runs in a local browser.
    #[arg(long, conflicts_with_all = ["request", "goal", "goal_stdin", "attach", "resume", "fork", "detached", "control", "plan_only", "review_plan", "check_mcp", "check_provider", "list_runs", "events", "metrics", "otlp_trace", "evaluate"])]
    pub web: bool,
    /// Train dashboard compression dictionaries from the given .ratio directories and exit.
    #[arg(long, num_args = 1.., value_name = "DIRECTORY", conflicts_with_all = ["web", "request", "goal", "goal_stdin", "attach", "resume", "fork", "detached", "control", "list_runs", "events", "metrics", "evaluate"])]
    pub train_web_dictionaries: Vec<PathBuf>,
    /// Loopback port for --web. Zero selects a free port.
    #[arg(long, requires = "web", default_value_t = 0)]
    pub web_port: u16,
    /// Enable a separate, read-only loopback listener for shared runs. Zero selects a free port.
    #[arg(long, requires = "web")]
    pub web_share_port: Option<u16>,
    /// Public HTTPS origin used in share links, such as https://ratio.example.ts.net.
    #[arg(long, requires = "web_share_port")]
    pub web_share_origin: Option<String>,
    /// Request to plan and implement.
    #[arg(short, long, conflicts_with_all = ["request", "goal_stdin"])]
    pub goal: Option<String>,
    #[arg(value_name = "REQUEST", conflicts_with_all = ["goal", "goal_stdin", "resume", "attach", "control", "events", "list_runs"])]
    pub request: Option<String>,
    /// Read the request from standard input.
    #[arg(long, conflicts_with_all = ["goal", "request", "resume", "attach", "control", "events", "list_runs"])]
    pub goal_stdin: bool,
    /// Print the team proposal as JSON and exit.
    #[arg(long, conflicts_with_all = ["resume", "detached", "review_plan", "fixed_team"])]
    pub plan_only: bool,
    /// Select the output format for --plan-only.
    #[arg(long, value_enum, requires = "plan_only")]
    pub plan_format: Option<PlanFormat>,
    /// Review and refine the team proposal in the TUI before starting.
    #[arg(long, conflicts_with_all = ["headless", "detached", "resume"])]
    pub review_plan: bool,
    /// Use the configured roster or the built-in worker without team generation.
    #[arg(long)]
    pub fixed_team: bool,
    /// List permitted tools from a configured stdio server without starting a run.
    #[arg(long, value_name = "NAME", conflicts_with_all = ["request", "goal", "goal_stdin", "resume", "detached", "plan_only"])]
    pub check_mcp: Option<String>,
    /// Send one small request to each configured model and print the result as JSON.
    #[arg(long, conflicts_with_all = ["request", "goal", "goal_stdin", "resume", "detached", "plan_only", "check_mcp", "fork", "attach", "control", "list_runs", "events", "metrics", "otlp_trace", "evaluate"])]
    pub check_provider: bool,
    #[arg(short, long)]
    pub config: Option<PathBuf>,
    #[arg(short = 'C', long)]
    cwd: Option<PathBuf>,
    #[arg(long, visible_alias = "base-url", env = "RATIO_ENDPOINT")]
    endpoint: Option<String>,
    #[arg(long, env = "RATIO_API_KEY", hide_env_values = true)]
    api_key: Option<String>,
    #[arg(long, env = "RATIO_MODEL")]
    model: Option<String>,
    #[arg(long)]
    worker_model: Option<String>,
    #[arg(long)]
    reviewer_model: Option<String>,
    #[arg(long, env = "RATIO_CONTEXT_WINDOW", value_name = "TOKENS")]
    context_window: Option<usize>,
    #[arg(long, value_name = "TOKENS")]
    output_tokens: Option<usize>,
    /// Request SSE streaming (true by default).
    #[arg(long, value_name = "BOOL")]
    stream: Option<bool>,
    #[arg(long, value_enum)]
    provider_api: Option<ProviderApi>,
    #[arg(long)]
    temperature: Option<f64>,
    #[arg(long)]
    top_p: Option<f64>,
    #[arg(long, allow_hyphen_values = true)]
    seed: Option<i64>,
    #[arg(long, value_name = "BOOL")]
    parallel_tool_calls: Option<bool>,
    /// Reasoning effort to request, such as low, medium, or high.
    #[arg(long, value_name = "LEVEL")]
    reasoning_effort: Option<String>,
    /// Send max_tokens for providers that require the legacy field.
    #[arg(long)]
    legacy_max_tokens: bool,
    /// Maximum number of reviews, or 0 for unlimited.
    #[arg(long)]
    max_cycles: Option<usize>,
    /// Maximum provider attempts, or 0 for unlimited.
    #[arg(long)]
    max_requests: Option<u64>,
    /// Maximum metered tokens per run, or 0 for unlimited.
    #[arg(long)]
    max_total_tokens: Option<u64>,
    /// Maximum active runtime in seconds, or 0 for unlimited.
    #[arg(long)]
    max_run_secs: Option<u64>,
    /// Run without the TUI. Progress goes to stderr and the assessment to stdout.
    #[arg(long)]
    pub headless: bool,
    /// Report request sizes and model usage to stderr.
    #[arg(long)]
    pub debug: bool,
    /// Continue the checkpoint for this workspace.
    #[arg(long)]
    pub resume: bool,
    /// Continue a specific recorded run.
    #[arg(long, requires = "resume")]
    pub run_id: Option<String>,
    /// Print one JSON record per run in the workspace history.
    #[arg(long, conflicts_with_all = ["events", "resume", "goal"])]
    pub list_runs: bool,
    /// Print checkpoint events for a run as JSON Lines.
    #[arg(long, value_name = "RUN_ID", conflicts_with_all = ["resume", "goal"])]
    pub events: Option<String>,
    /// Start reading events after this sequence number.
    #[arg(long, requires = "events", default_value = "0")]
    pub after: u64,
    /// Follow durably committed events until the run reaches a terminal phase.
    #[arg(long, requires = "events")]
    pub follow: bool,
    /// Run headlessly in a process independent of this terminal.
    #[arg(long, conflicts_with_all = ["list_runs", "events", "control"])]
    pub detached: bool,
    #[arg(long, hide = true, conflicts_with = "detached")]
    pub managed_run: bool,
    #[arg(long, hide = true, requires = "managed_run")]
    pub dashboard_managed: bool,
    #[arg(long, hide = true, requires_all = ["managed_run", "goal_stdin", "continue_checkpoint", "continue_action"], conflicts_with_all = ["fork", "resume"])]
    pub continue_run: Option<String>,
    #[arg(long, hide = true, requires = "continue_run")]
    pub continue_checkpoint: Option<u64>,
    #[arg(long, hide = true, requires = "continue_run")]
    pub continue_action: Option<String>,
    /// Control the active foreground headless or detached run.
    #[arg(long, value_enum, conflicts_with_all = ["events", "list_runs", "resume", "goal"])]
    pub control: Option<ControlOperation>,
    /// Text to send with --control guide.
    #[arg(long, requires = "control")]
    pub guidance: Option<String>,
    /// Describe the new worker's role and proposed scope.
    #[arg(long, requires = "control")]
    pub worker_request: Option<String>,
    /// Start a fresh run from a recorded run's goal and roster.
    #[arg(long, value_name = "RUN_ID", conflicts_with_all = ["resume", "list_runs", "events", "control"])]
    pub fork: Option<String>,
    /// Open a live or historical run in the terminal.
    #[arg(long, value_name = "RUN_ID", conflicts_with_all = ["resume", "detached", "fork", "goal", "list_runs", "events", "control"])]
    pub attach: Option<String>,
    /// Print metered usage and effect counts for a recorded run.
    #[arg(long, value_name = "RUN_ID", conflicts_with_all = ["attach", "control", "detached", "events", "fork", "goal", "list_runs", "resume", "evaluate", "otlp_trace"])]
    pub metrics: Option<String>,
    /// Print a redacted OTLP/HTTP JSON trace derived from durable checkpoints.
    #[arg(long, value_name = "RUN_ID", conflicts_with_all = ["attach", "control", "detached", "events", "fork", "goal", "list_runs", "resume", "evaluate"])]
    pub otlp_trace: Option<String>,
    /// Evaluate a recorded run against reproducible acceptance thresholds.
    #[arg(long, value_name = "RUN_ID", conflicts_with_all = ["attach", "control", "detached", "events", "fork", "goal", "list_runs", "resume"])]
    pub evaluate: Option<String>,
    /// Optional JSON file containing evaluation thresholds.
    #[arg(long, requires = "evaluate")]
    pub evaluation: Option<PathBuf>,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum PlanFormat {
    Json,
    Toml,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum ControlOperation {
    Status,
    Pause,
    Resume,
    Cancel,
    Guide,
    AddWorker,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    goal: Option<String>,
    cwd: Option<PathBuf>,
    endpoint: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
    context_window: Option<usize>,
    output_tokens: Option<usize>,
    legacy_max_tokens: bool,
    provider: Provider,
    worker: RoleConfig,
    reviewer: RoleConfig,
    orchestration: Orchestration,
    constraints: Constraints,
    stakeholders: Vec<Stakeholder>,
    reviewers: Vec<Stakeholder>,
    workers: Vec<Worker>,
    mcp_servers: BTreeMap<String, McpServer>,
    skills: BTreeMap<String, Skill>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Skill {
    pub directory: PathBuf,
    pub description: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    pub command: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    pub tools: Option<BTreeSet<String>>,
    #[serde(default)]
    pub allow_all: bool,
    pub deny_tools: Option<BTreeSet<String>>,
    #[serde(default)]
    pub inspection_tools: BTreeSet<String>,
    pub instructions: Option<String>,
    #[serde(default = "default_mcp_protocol")]
    pub protocol: String,
    #[serde(default)]
    pub lifetime: McpLifetime,
}

impl McpServer {
    fn validate_policy(&self) -> Result<()> {
        let names = match (&self.tools, self.allow_all, &self.deny_tools) {
            (None, true, None) => None,
            (Some(names), false, None) => {
                ensure!(
                    !names.is_empty(),
                    "tools must name at least one permitted tool"
                );
                Some(names)
            }
            (None, false, Some(names)) => Some(names),
            _ => anyhow::bail!("choose exactly one of tools, allow_all = true, or deny_tools"),
        };
        ensure!(
            names
                .into_iter()
                .flatten()
                .chain(&self.inspection_tools)
                .all(|name| !name.trim().is_empty() && name.len() <= 128),
            "tool lists must contain names of 1 to 128 bytes"
        );
        for name in &self.inspection_tools {
            ensure!(
                self.permits(name),
                "inspection_tools contains {name:?}, blocked by the server tool policy"
            );
        }
        Ok(())
    }

    pub(crate) fn permits(&self, name: &str) -> bool {
        match (&self.tools, self.allow_all, &self.deny_tools) {
            (None, true, None) => true,
            (Some(names), false, None) => names.contains(name),
            (None, false, Some(names)) => !names.contains(name),
            _ => false,
        }
    }
}

#[cfg(test)]
mod mcp_policy_tests {
    use super::McpServer;

    #[test]
    fn tool_policy_requires_one_explicit_mode_without_limiting_list_size() {
        let load = |mode: &str| {
            let server: McpServer =
                toml::from_str(&format!("command = \"fixture\"\n{mode}")).unwrap();
            server.validate_policy()
        };
        let names = (0..161)
            .map(|index| format!("\"inspect_{index}\""))
            .collect::<Vec<_>>()
            .join(",");
        assert!(load(&format!("tools = [{names}]")).is_ok());
        assert!(load(&format!("deny_tools = [{names}]")).is_ok());
        assert!(load("deny_tools = []").is_ok());
        assert!(load("allow_all = true").is_ok());
        assert!(load("allow_all = true\ninspection_tools = [\"inspect\"]").is_ok());
        for invalid in [
            "",
            "tools = []",
            "allow_all = false",
            "allow_all = true\ntools = [\"inspect\"]",
            "allow_all = true\ndeny_tools = [\"inspect\"]",
            "tools = [\"inspect\"]\ndeny_tools = [\"change\"]",
            "deny_tools = [\"change\"]\ninspection_tools = [\"change\"]",
            "tools = [\"inspect\"]\ninspection_tools = [\"unlisted\"]",
        ] {
            assert!(load(invalid).is_err(), "accepted {invalid:?}");
        }
    }
}

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpLifetime {
    #[default]
    Call,
    Worker,
}

fn default_mcp_protocol() -> String {
    "2025-11-25".into()
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Provider {
    pub api: ProviderApi,
    pub stream: bool,
    pub stream_usage: bool,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub seed: Option<i64>,
    pub parallel_tool_calls: Option<bool>,
    pub reasoning_effort: Option<String>,
    pub headers: BTreeMap<String, String>,
}

/// A model with the reasoning effort to request from it.
#[derive(Clone, Copy)]
pub struct ModelSpec<'a> {
    pub model: &'a str,
    pub effort: Option<&'a str>,
}

impl<'a> ModelSpec<'a> {
    /// Apply an agent's own overrides over a role default.
    pub fn with(self, model: Option<&'a str>, effort: Option<&'a str>) -> Self {
        Self {
            model: model.unwrap_or(self.model),
            effort: effort.or(self.effort),
        }
    }
}

fn valid_effort(value: &str) -> bool {
    (1..=32).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte))
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ProviderApi {
    #[default]
    ChatCompletions,
    Responses,
}

impl Default for Provider {
    fn default() -> Self {
        Self {
            api: ProviderApi::default(),
            stream: true,
            stream_usage: true,
            temperature: None,
            top_p: None,
            seed: None,
            parallel_tool_calls: None,
            reasoning_effort: None,
            headers: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct RoleConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Orchestration {
    pub max_plan_attempts: usize,
    pub max_requests: u64,
    pub max_total_tokens: u64,
    pub max_run_secs: u64,
    pub max_review_cycles: usize,
    pub max_tool_rounds: usize,
    pub request_timeout_secs: u64,
    pub command_timeout_secs: u64,
    pub allow_inspection_shell: bool,
    pub parallel_stakeholders: bool,
    pub max_parallel_agents: usize,
    pub require_task_graph: bool,
    pub stage_workspace: Option<bool>,
    pub verification_commands: Option<Vec<String>>,
    pub worker_system_prompt: Option<String>,
    pub reviewer_system_prompt: Option<String>,
    pub project_instructions: bool,
    pub instruction_files: Vec<String>,
}

impl Default for Orchestration {
    fn default() -> Self {
        Self {
            max_plan_attempts: 0,
            max_requests: 0,
            max_total_tokens: 0,
            max_run_secs: 0,
            max_review_cycles: 0,
            max_tool_rounds: 0,
            request_timeout_secs: 180,
            command_timeout_secs: 120,
            allow_inspection_shell: false,
            parallel_stakeholders: true,
            max_parallel_agents: 8,
            require_task_graph: false,
            stage_workspace: None,
            verification_commands: None,
            worker_system_prompt: None,
            reviewer_system_prompt: None,
            project_instructions: true,
            instruction_files: ["AGENTS.md", "CLAUDE.md", "CONTRIBUTING.md"]
                .map(String::from)
                .to_vec(),
        }
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Constraints {
    required_tools: Vec<String>,
    forbidden_tools: Vec<String>,
    required_approaches: Vec<String>,
    forbidden_approaches: Vec<String>,
    allowed_paths: Vec<String>,
    forbidden_paths: Vec<String>,
    custom_rules: Vec<String>,
}

impl Constraints {
    pub(crate) fn shell_disabled_for(&self, actor: &str) -> bool {
        self.forbidden_tools
            .iter()
            .any(|entry| entry.split_once('/') == Some((actor, "shell")))
    }

    pub fn prompt(&self) -> String {
        [
            ("Required tools", &self.required_tools),
            ("Forbidden tools", &self.forbidden_tools),
            ("Required approaches", &self.required_approaches),
            ("Forbidden approaches", &self.forbidden_approaches),
            ("Allowed write paths", &self.allowed_paths),
            ("Forbidden paths", &self.forbidden_paths),
            ("Mandatory quality rules", &self.custom_rules),
        ]
        .into_iter()
        .filter(|(_, entries)| !entries.is_empty())
        .map(|(name, entries)| format!("{name}:\n- {}\n", entries.join("\n- ")))
        .collect()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConsultationPhase {
    Planning,
    Review,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Stakeholder {
    pub name: String,
    #[serde(alias = "description")]
    pub persona: String,
    #[serde(default = "both_phases")]
    pub phases: Vec<ConsultationPhase>,
    #[serde(default, skip_serializing_if = "RoleConfig::is_default")]
    agent: RoleConfig,
}

impl RoleConfig {
    fn is_default(&self) -> bool {
        self.model.is_none() && self.reasoning_effort.is_none()
    }
}

impl Stakeholder {
    /// This participant's model and effort, falling back to the role default.
    pub fn spec<'a>(&'a self, fallback: ModelSpec<'a>) -> ModelSpec<'a> {
        fallback.with(
            self.agent.model.as_deref(),
            self.agent.reasoning_effort.as_deref(),
        )
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Worker {
    pub name: String,
    pub persona: String,
    pub model: Option<String>,
    // Omitted when unset so plan digests of existing runs stay stable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

impl Worker {
    /// This worker's model and effort, falling back to the role default.
    pub fn spec<'a>(&'a self, fallback: ModelSpec<'a>) -> ModelSpec<'a> {
        fallback.with(self.model.as_deref(), self.reasoning_effort.as_deref())
    }
}

#[cfg(test)]
mod effort_tests {
    use super::{ModelSpec, Stakeholder, Worker, valid_effort};

    #[test]
    fn agent_settings_override_the_role_default_and_keep_plan_digests_stable() {
        let role = ModelSpec {
            model: "role-model",
            effort: Some("low"),
        };
        let plain: Worker = toml::from_str("name = \"A\"\npersona = \"B\"").unwrap();
        let spec = plain.spec(role);
        assert_eq!((spec.model, spec.effort), ("role-model", Some("low")));
        // The roster is hashed into the plan ID, so an unset effort must not appear.
        assert_eq!(
            serde_json::to_string(&plain).unwrap(),
            r#"{"name":"A","persona":"B","model":null}"#
        );
        let tuned: Worker =
            toml::from_str("name = \"A\"\npersona = \"B\"\nreasoning_effort = \"high\"").unwrap();
        let spec = tuned.spec(role);
        assert_eq!((spec.model, spec.effort), ("role-model", Some("high")));
        let reviewer: Stakeholder = toml::from_str(
            "name = \"A\"\npersona = \"B\"\n[agent]\nmodel = \"own\"\nreasoning_effort = \"medium\"",
        )
        .unwrap();
        let spec = reviewer.spec(role);
        assert_eq!((spec.model, spec.effort), ("own", Some("medium")));
        assert!(valid_effort("xhigh") && valid_effort("low-latency_2"));
        assert!(!valid_effort("") && !valid_effort("Very High") && !valid_effort(&"x".repeat(33)));
    }
}

fn both_phases() -> Vec<ConsultationPhase> {
    vec![ConsultationPhase::Planning, ConsultationPhase::Review]
}

pub struct Config {
    pub goal: Option<String>,
    /// Profile this configuration was read from, if any.
    pub profile: Option<PathBuf>,
    pub cwd: PathBuf,
    pub endpoint: String,
    pub api_key: Option<String>,
    pub worker_model: String,
    pub reviewer_model: String,
    pub worker_effort: Option<String>,
    pub reviewer_effort: Option<String>,
    pub context_window: usize,
    pub output_tokens: usize,
    pub legacy_max_tokens: bool,
    pub provider: Provider,
    pub orchestration: Orchestration,
    pub constraints: Constraints,
    pub stakeholders: Vec<Stakeholder>,
    pub reviewers: Vec<Stakeholder>,
    pub workers: Vec<Worker>,
    pub mcp_servers: BTreeMap<String, McpServer>,
    pub skills: BTreeMap<String, Skill>,
    pub headless: bool,
    pub debug: bool,
    pub resume: bool,
    pub fixed_team: bool,
    pub review_plan: bool,
}

impl Config {
    /// Default model and effort for work-phase agents.
    pub fn worker_spec(&self) -> ModelSpec<'_> {
        ModelSpec {
            model: &self.worker_model,
            effort: self.worker_effort.as_deref(),
        }
    }

    /// Default model and effort for the lead reviewer and inspection agents.
    pub fn reviewer_spec(&self) -> ModelSpec<'_> {
        ModelSpec {
            model: &self.reviewer_model,
            effort: self.reviewer_effort.as_deref(),
        }
    }

    /// What the next run will use, for display to the operator. Built as an
    /// allowlist: the key and header values never appear, only their presence.
    pub fn summary(&self) -> serde_json::Value {
        let endpoint = reqwest::Url::parse(&self.endpoint).map_or_else(
            |_| "invalid URL".to_owned(),
            |mut url| {
                url.set_query(None);
                url.set_fragment(None);
                let _ = url.set_username("");
                let _ = url.set_password(None);
                url.to_string()
            },
        );
        let limits = &self.orchestration;
        fn names(agents: &[Stakeholder]) -> Vec<&str> {
            agents.iter().map(|agent| agent.name.as_str()).collect()
        }
        serde_json::json!({
            "workspace": self.cwd,
            "profile": self.profile,
            "provider": {
                "endpoint": endpoint,
                "api": match self.provider.api {
                    ProviderApi::ChatCompletions => "chat_completions",
                    ProviderApi::Responses => "responses",
                },
                "stream": self.provider.stream,
                "key": self.api_key.is_some(),
                "headers": self.provider.headers.keys().collect::<Vec<_>>(),
            },
            "models": {
                "worker": self.worker_model, "reviewer": self.reviewer_model,
                "worker_effort": self.worker_effort, "reviewer_effort": self.reviewer_effort,
            },
            "context_window": self.context_window,
            "output_tokens": self.output_tokens,
            "limits": {
                "max_review_cycles": limits.max_review_cycles,
                "max_requests": limits.max_requests,
                "max_total_tokens": limits.max_total_tokens,
                "max_run_secs": limits.max_run_secs,
                "max_tool_rounds": limits.max_tool_rounds,
                "max_parallel_agents": limits.max_parallel_agents,
                "request_timeout_secs": limits.request_timeout_secs,
                "command_timeout_secs": limits.command_timeout_secs,
            },
            "team": {
                "mode": if !self.workers.is_empty() {
                    "configured"
                } else if self.fixed_team {
                    "fixed"
                } else {
                    "generated"
                },
                "workers": self.workers.iter().map(|worker| &worker.name).collect::<Vec<_>>(),
                "reviewers": names(&self.reviewers),
                "stakeholders": names(&self.stakeholders),
            },
            "staging": {
                "enabled": limits.stage_workspace.unwrap_or_default(),
                "verification_commands": limits.verification_commands.as_deref().unwrap_or_default(),
            },
            "instructions": {
                "enabled": limits.project_instructions,
                "files": limits.instruction_files,
            },
            "mcp_servers": self.mcp_servers.keys().collect::<Vec<_>>(),
            "skills": self.skills.keys().collect::<Vec<_>>(),
        })
    }

    /// Project instruction text for agent prompts, read from the workspace root.
    pub(crate) fn project_instructions(&self) -> Option<String> {
        self.orchestration
            .project_instructions
            .then(|| {
                crate::instructions::load(
                    &self.cwd,
                    &self.orchestration.instruction_files,
                    crate::instructions::limit(self.context_window),
                )
            })
            .flatten()
    }

    pub fn profile_candidate(explicit: Option<&std::path::Path>) -> Option<PathBuf> {
        if let Some(path) = explicit {
            return Some(path.to_owned());
        }
        let base = if cfg!(windows) {
            env::var_os("APPDATA").map(PathBuf::from)
        } else {
            env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        }?;
        Some(base.join("ratio").join("config.toml"))
    }

    fn profile(explicit: Option<&std::path::Path>) -> Option<PathBuf> {
        Self::profile_candidate(explicit)
            .filter(|path| explicit.is_some() || path.try_exists().unwrap_or(true))
    }

    fn read_file(path: Option<&std::path::Path>) -> Result<FileConfig> {
        Ok(match path {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("read {}", path.display()))?;
                toml::from_str(&text).map_err(|error| {
                    anyhow::anyhow!("invalid config {}: {}", path.display(), error.message())
                })?
            }
            None => FileConfig::default(),
        })
    }

    pub fn history_workspace(cli: &Cli) -> Result<PathBuf> {
        let profile = Self::profile(cli.config.as_deref());
        let file = Self::read_file(profile.as_deref())?;
        Self::workspace(cli.cwd.clone().or(file.cwd))
    }

    fn workspace(cwd: Option<PathBuf>) -> Result<PathBuf> {
        let cwd = cwd.unwrap_or_else(|| PathBuf::from("."));
        let cwd = cwd
            .canonicalize()
            .with_context(|| format!("resolve {}", cwd.display()))?;
        ensure!(cwd.is_dir(), "workspace must be a directory");
        Ok(cwd)
    }

    pub fn export_campaign(&self, draft: &crate::setup::Draft) -> Result<String> {
        let endpoint = reqwest::Url::parse(&self.endpoint).context("parse export endpoint")?;
        ensure!(
            self.provider.headers.is_empty()
                && endpoint.username().is_empty()
                && endpoint.password().is_none()
                && endpoint.query().is_none()
                && endpoint.fragment().is_none(),
            "TOML export requires an endpoint without embedded credentials or query parameters and a profile without custom provider headers. Use the JSON preview for this profile."
        );
        let mut orchestration = self.orchestration.clone();
        orchestration.require_task_graph |=
            !draft.workers.is_empty() && !draft.acceptance.is_empty();
        if let Some(execution) = &draft.execution {
            orchestration.stage_workspace = Some(execution.stage_workspace);
            orchestration.max_parallel_agents = execution.max_parallel_agents;
            orchestration.verification_commands = Some(execution.verification_commands.clone());
        }
        let mut goal = draft.goal.clone();
        if !draft.acceptance.is_empty() {
            goal.push_str("\n\nStartup acceptance criteria:\n");
            goal.push_str(&draft.acceptance.join("\n"));
        }
        if let Some(warning) = draft
            .execution
            .as_ref()
            .and_then(|execution| execution.pending_ownership.as_deref())
        {
            goal.push_str("\n\n");
            goal.push_str(warning);
        }
        if let Some(campaign) = &draft.campaign {
            goal.push_str("\n\nDraft campaign for coordination:\n");
            goal.push_str(&serde_json::to_string_pretty(campaign)?);
        }
        let file = FileConfig {
            goal: Some(goal),
            cwd: Some(self.cwd.clone()),
            endpoint: Some(self.endpoint.clone()),
            api_key: None,
            model: (self.reviewer_model == self.worker_model).then(|| self.worker_model.clone()),
            context_window: Some(self.context_window),
            output_tokens: Some(self.output_tokens),
            legacy_max_tokens: self.legacy_max_tokens,
            provider: self.provider.clone(),
            worker: RoleConfig {
                model: (self.reviewer_model != self.worker_model)
                    .then(|| self.worker_model.clone()),
                reasoning_effort: self.worker_effort.clone(),
            },
            reviewer: RoleConfig {
                model: (self.reviewer_model != self.worker_model)
                    .then(|| self.reviewer_model.clone()),
                reasoning_effort: self.reviewer_effort.clone(),
            },
            orchestration,
            constraints: self.constraints.clone(),
            stakeholders: draft.stakeholders.clone(),
            reviewers: draft.reviewers.clone(),
            workers: draft.workers.clone(),
            mcp_servers: self.mcp_servers.clone(),
            skills: self.skills.clone(),
        };
        let text = toml::to_string_pretty(&file).context("encode campaign profile")?;
        ensure!(
            self.api_key.as_ref().is_none_or(|key| !text.contains(key)),
            "generated profile contains the configured API key"
        );
        // Ignore environment-backed argument overrides while checking the exported profile.
        let cli = Cli::command().try_get_matches_from(["ra"])?;
        let mut cli = Cli::from_arg_matches(&cli)?;
        cli.endpoint = None;
        cli.api_key = None;
        cli.model = None;
        cli.context_window = None;
        Self::resolve(
            cli,
            toml::from_str(&text).context("decode exported campaign profile")?,
        )?;
        Ok(text)
    }

    pub fn load(mut cli: Cli) -> Result<Self> {
        cli.config = Self::profile(cli.config.as_deref());
        let file = Self::read_file(cli.config.as_deref())?;
        Self::resolve(cli, file)
    }

    fn resolve(cli: Cli, mut file: FileConfig) -> Result<Self> {
        ensure!(
            file.mcp_servers.len() <= 16,
            "configure at most 16 MCP servers"
        );
        for (name, server) in &file.mcp_servers {
            ensure!(
                !name.is_empty()
                    && name.len() <= 64
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
                    && !server.command.as_os_str().is_empty()
                    && matches!(server.protocol.as_str(), "2025-11-25" | "2026-07-28"),
                "invalid MCP server {name}: require a command and a supported protocol"
            );
            server
                .validate_policy()
                .with_context(|| format!("invalid MCP server {name}"))?;
        }
        let model = cli.model.or(file.model);
        let worker_model = cli.worker_model.or(file.worker.model).or(model.clone());
        let reviewer_model = cli.reviewer_model.or(file.reviewer.model).or(model);
        let worker_model = worker_model
            .filter(|m| !m.trim().is_empty())
            .or_else(|| cli.check_mcp.as_ref().map(|_| String::new()))
            .context("set --model or --worker-model")?;
        let reviewer_model = reviewer_model
            .filter(|m| !m.trim().is_empty())
            .or_else(|| cli.check_mcp.as_ref().map(|_| String::new()))
            .context("set --model or --reviewer-model")?;
        let context_window = cli
            .context_window
            .or(file.context_window)
            .or_else(|| cli.check_mcp.as_ref().map(|_| 2))
            .context("set --context-window to the model's token limit")?;
        ensure!(
            context_window >= 2,
            "context window must allow at least one input and one output token"
        );
        let output_tokens = cli
            .output_tokens
            .or(file.output_tokens)
            .unwrap_or((context_window / 4).clamp(1, 4096));
        ensure!(
            (1..context_window).contains(&output_tokens),
            "output token budget must be positive and smaller than the context window"
        );
        let mut provider = file.provider;
        // An agent's own setting wins, then its role, then the shared default.
        let shared_effort = cli.reasoning_effort.or(provider.reasoning_effort.take());
        let worker_effort = file.worker.reasoning_effort.or(shared_effort.clone());
        let reviewer_effort = file.reviewer.reasoning_effort.or(shared_effort);
        ensure!(
            worker_effort
                .iter()
                .chain(&reviewer_effort)
                .chain(
                    file.workers
                        .iter()
                        .filter_map(|worker| worker.reasoning_effort.as_ref())
                )
                .chain(
                    file.reviewers
                        .iter()
                        .chain(&file.stakeholders)
                        .filter_map(|agent| agent.agent.reasoning_effort.as_ref())
                )
                .all(|value| valid_effort(value)),
            "reasoning_effort must be 1 to 32 characters of a-z, 0-9, _ or -"
        );
        provider.api = cli.provider_api.unwrap_or(provider.api);
        provider.stream = cli.stream.unwrap_or(provider.stream);
        provider.temperature = cli.temperature.or(provider.temperature);
        provider.top_p = cli.top_p.or(provider.top_p);
        provider.seed = cli.seed.or(provider.seed);
        provider.parallel_tool_calls = cli.parallel_tool_calls.or(provider.parallel_tool_calls);
        ensure!(
            provider.api != ProviderApi::Responses
                || (provider.seed.is_none() && !(cli.legacy_max_tokens || file.legacy_max_tokens)),
            "seed and legacy_max_tokens require the Chat Completions provider API"
        );
        ensure!(
            provider
                .temperature
                .is_none_or(|value| (0.0..=2.0).contains(&value)),
            "provider temperature must be in 0..=2"
        );
        ensure!(
            provider
                .top_p
                .is_none_or(|value| (0.0..=1.0).contains(&value)),
            "provider top_p must be in 0..=1"
        );
        let mut orchestration = file.orchestration;
        if let Some(limit) = cli.max_requests {
            orchestration.max_requests = limit;
        }
        if let Some(limit) = cli.max_total_tokens {
            orchestration.max_total_tokens = limit;
        }
        if let Some(limit) = cli.max_run_secs {
            orchestration.max_run_secs = limit;
        }
        ensure!(file.workers.len() <= 64, "configure at most 64 workers");
        let commands = orchestration
            .verification_commands
            .as_deref()
            .unwrap_or_default();
        ensure!(
            !orchestration.stage_workspace.unwrap_or_default() || !commands.is_empty(),
            "staged execution requires 1–16 nonempty verification commands of at most 4096 bytes each"
        );
        // Direct runs check the same commands in the live workspace.
        ensure!(
            commands.len() <= 16
                && commands
                    .iter()
                    .all(|command| !command.trim().is_empty() && command.len() <= 4096),
            "verification_commands accepts up to 16 nonempty commands of at most 4096 bytes each"
        );
        ensure!(
            file.workers
                .iter()
                .all(|worker| !worker.name.trim().is_empty()
                    && !worker.persona.trim().is_empty()
                    && worker
                        .model
                        .as_ref()
                        .is_none_or(|model| !model.trim().is_empty())),
            "worker entries require a name, persona, and nonempty model override when supplied"
        );
        ensure!(
            (1..=64).contains(&orchestration.max_parallel_agents),
            "max_parallel_agents must be 1–64"
        );
        ensure!(
            orchestration.instruction_files.len() <= 8
                && orchestration
                    .instruction_files
                    .iter()
                    .all(|name| crate::instructions::valid_name(name)),
            "instruction_files accepts at most 8 workspace-relative file paths"
        );
        ensure!(
            file.reviewers.len() + file.stakeholders.len() <= 64,
            "configure at most 64 reviewers and stakeholders combined"
        );
        if let Some(max_cycles) = cli.max_cycles {
            orchestration.max_review_cycles = max_cycles;
        }
        ensure!(
            orchestration.request_timeout_secs > 0 && orchestration.command_timeout_secs > 0,
            "timeouts must be positive"
        );
        let cwd = Self::workspace(cli.cwd.or(file.cwd))?;
        ensure!(file.skills.len() <= 32, "configure at most 32 skills");
        for (name, skill) in &mut file.skills {
            ensure!(
                !name.is_empty()
                    && name.len() <= 64
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
                    && !skill.description.trim().is_empty()
                    && skill.description.len() <= 512,
                "invalid skill {name}: require an ID and a description of at most 512 bytes"
            );
            let root = cli
                .config
                .as_deref()
                .and_then(std::path::Path::parent)
                .unwrap_or(&cwd);
            skill.directory = root
                .join(&skill.directory)
                .canonicalize()
                .with_context(|| format!("resolve skill directory for {name}"))?;
            ensure!(
                skill.directory.is_dir() && skill.directory.join("SKILL.md").is_file(),
                "skill {name} must contain SKILL.md"
            );
        }

        Ok(Self {
            goal: cli.goal.or(file.goal),
            profile: cli.config,
            cwd,
            endpoint: cli
                .endpoint
                .or(file.endpoint)
                .or_else(|| env::var("OPENAI_BASE_URL").ok())
                .unwrap_or_else(|| "https://api.openai.com/v1".into()),
            api_key: cli
                .api_key
                .or(file.api_key)
                .or_else(|| env::var("OPENAI_API_KEY").ok())
                .filter(|key| !key.is_empty()),
            worker_model,
            reviewer_model,
            worker_effort,
            reviewer_effort,
            context_window,
            output_tokens,
            legacy_max_tokens: cli.legacy_max_tokens || file.legacy_max_tokens,
            provider,
            orchestration,
            constraints: file.constraints,
            stakeholders: file.stakeholders,
            reviewers: file.reviewers,
            workers: file.workers,
            mcp_servers: file.mcp_servers,
            skills: file.skills,
            headless: cli.headless,
            debug: cli.debug,
            resume: cli.resume,
            fixed_team: cli.fixed_team,
            review_plan: cli.review_plan,
        })
    }
}
