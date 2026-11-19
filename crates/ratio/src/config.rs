use std::{collections::BTreeMap, env, path::PathBuf};

use anyhow::{Context, Result, ensure};
use clap::Parser;
use serde::{Deserialize, Serialize};

#[derive(Parser)]
#[command(
    version,
    about = "Plan, implement and review a coding goal with local tools"
)]
pub struct Cli {
    #[arg(short, long)]
    pub goal: Option<String>,
    #[arg(short, long)]
    config: Option<PathBuf>,
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
    #[arg(long)]
    max_requests: Option<u64>,
    #[arg(long)]
    max_total_tokens: Option<u64>,
    #[arg(long)]
    max_run_secs: Option<u64>,
    /// Print only the final assessment to stdout and diagnostics to stderr.
    #[arg(long)]
    pub headless: bool,
    /// Report request sizes and model usage to stderr.
    #[arg(long)]
    pub debug: bool,
    /// Continue the checkpoint for this workspace.
    #[arg(long)]
    pub resume: bool,
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
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Provider {
    pub stream: bool,
    pub stream_usage: bool,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub seed: Option<i64>,
    pub parallel_tool_calls: Option<bool>,
    pub headers: BTreeMap<String, String>,
}

impl Default for Provider {
    fn default() -> Self {
        Self {
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
    pub parallel_stakeholders: bool,
    pub max_parallel_agents: usize,
    pub worker_system_prompt: Option<String>,
    pub reviewer_system_prompt: Option<String>,
}

impl Default for Orchestration {
    fn default() -> Self {
        Self {
            max_plan_attempts: 3,
            max_requests: 512,
            max_total_tokens: 8_000_000,
            max_run_secs: 3600,
            max_review_cycles: 3,
            max_tool_rounds: 100,
            request_timeout_secs: 180,
            command_timeout_secs: 120,
            parallel_stakeholders: true,
            max_parallel_agents: 8,
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
    pub headless: bool,
    pub debug: bool,
    pub resume: bool,
}

impl Config {
    pub fn load(cli: Cli) -> Result<Self> {
        let file: FileConfig = match cli.config {
            Some(path) => {
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("read {}", path.display()))?;
                toml::from_str(&text).map_err(|error| {
                    anyhow::anyhow!("invalid config {}: {}", path.display(), error.message())
                })?
            }
            None => FileConfig::default(),
        };
        let model = cli.model.or(file.model);
        let worker_model = cli.worker_model.or(file.worker.model).or(model.clone());
        let reviewer_model = cli.reviewer_model.or(file.reviewer.model).or(model);
        let worker_model = worker_model
            .filter(|m| !m.trim().is_empty())
            .context("set --model or --worker-model")?;
        let reviewer_model = reviewer_model
            .filter(|m| !m.trim().is_empty())
            .context("set --model or --reviewer-model")?;
        let context_window = cli
            .context_window
            .or(file.context_window)
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
        provider.stream = cli.stream.unwrap_or(provider.stream);
        provider.temperature = cli.temperature.or(provider.temperature);
        provider.top_p = cli.top_p.or(provider.top_p);
        provider.seed = cli.seed.or(provider.seed);
        provider.parallel_tool_calls = cli.parallel_tool_calls.or(provider.parallel_tool_calls);
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
        ensure!(
            orchestration.max_requests > 0
                && orchestration.max_total_tokens > 0
                && orchestration.max_run_secs > 0
                && orchestration.max_plan_attempts > 0,
            "run budgets and max_plan_attempts must be positive"
        );
        ensure!(file.workers.len() <= 64, "configure at most 64 workers");
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
            orchestration.max_tool_rounds > 0,
            "max_tool_rounds must be positive"
        );
        ensure!(
            orchestration.request_timeout_secs > 0 && orchestration.command_timeout_secs > 0,
            "timeouts must be positive"
        );
        let cwd = cli.cwd.or(file.cwd).unwrap_or_else(|| PathBuf::from("."));
        let cwd = cwd
            .canonicalize()
            .with_context(|| format!("resolve {}", cwd.display()))?;
        ensure!(cwd.is_dir(), "workspace must be a directory");

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
            headless: cli.headless,
            debug: cli.debug,
            resume: cli.resume,
        })
    }
}
