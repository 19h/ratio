use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    path::PathBuf,
};

use anyhow::{Context, Result, ensure};
use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};

#[derive(Parser)]
#[command(
    version,
    about = "Plan, implement and review a coding goal with local tools"
)]
pub struct Cli {
    /// Serve the workspace's live and historical runs in a local browser.
    #[arg(long, conflicts_with_all = ["request", "goal", "goal_stdin", "attach", "resume", "fork", "detached", "control", "plan_only", "review_plan", "check_mcp", "list_runs", "events", "metrics", "otlp_trace", "evaluate"])]
    pub web: bool,
    /// Loopback port for --web. Zero selects a free port.
    #[arg(long, requires = "web", default_value_t = 0)]
    pub web_port: u16,
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
    /// Review and refine the team proposal in the TUI before starting.
    #[arg(long, conflicts_with_all = ["headless", "detached", "resume"])]
    pub review_plan: bool,
    /// Use the configured roster or the built-in worker without team generation.
    #[arg(long)]
    pub fixed_team: bool,
    /// List permitted tools from a configured stdio server without starting a run.
    #[arg(long, value_name = "NAME", conflicts_with_all = ["request", "goal", "goal_stdin", "resume", "detached", "plan_only"])]
    pub check_mcp: Option<String>,
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
pub enum ControlOperation {
    Status,
    Pause,
    Resume,
    Cancel,
    Guide,
    AddWorker,
}

#[derive(Default, Deserialize)]
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

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Skill {
    pub directory: PathBuf,
    pub description: String,
}

#[derive(Clone, Deserialize)]
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

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpLifetime {
    #[default]
    Call,
    Worker,
}

fn default_mcp_protocol() -> String {
    "2025-11-25".into()
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Provider {
    pub api: ProviderApi,
    pub stream: bool,
    pub stream_usage: bool,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub seed: Option<i64>,
    pub parallel_tool_calls: Option<bool>,
    pub headers: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, ValueEnum)]
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
            headers: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct RoleConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

#[derive(Deserialize)]
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
    pub stage_workspace: bool,
    pub verification_commands: Vec<String>,
    pub worker_system_prompt: Option<String>,
    pub reviewer_system_prompt: Option<String>,
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
            stage_workspace: false,
            verification_commands: Vec::new(),
            worker_system_prompt: None,
            reviewer_system_prompt: None,
        }
    }
}

#[derive(Default, Deserialize)]
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
        self.model.is_none()
    }
}

impl Stakeholder {
    pub fn model<'a>(&'a self, fallback: &'a str) -> &'a str {
        self.agent.model.as_deref().unwrap_or(fallback)
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Worker {
    pub name: String,
    pub persona: String,
    pub model: Option<String>,
}

fn both_phases() -> Vec<ConsultationPhase> {
    vec![ConsultationPhase::Planning, ConsultationPhase::Review]
}

pub struct Config {
    pub goal: Option<String>,
    pub cwd: PathBuf,
    pub endpoint: String,
    pub api_key: Option<String>,
    pub worker_model: String,
    pub reviewer_model: String,
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
    fn profile(explicit: Option<&std::path::Path>) -> Option<PathBuf> {
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
        let path = base.join("ratio").join("config.toml");
        path.try_exists().unwrap_or(true).then_some(path)
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

    pub fn load(mut cli: Cli) -> Result<Self> {
        cli.config = Self::profile(cli.config.as_deref());
        let mut file = Self::read_file(cli.config.as_deref())?;
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
        ensure!(
            !orchestration.stage_workspace
                || (!orchestration.verification_commands.is_empty()
                    && orchestration.verification_commands.len() <= 16
                    && orchestration
                        .verification_commands
                        .iter()
                        .all(|command| !command.trim().is_empty() && command.len() <= 4096)),
            "staged execution requires 1–16 nonempty verification commands of at most 4096 bytes each"
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
