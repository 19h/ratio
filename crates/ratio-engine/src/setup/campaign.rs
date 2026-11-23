use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::Read,
    path::Path,
};

use anyhow::{Result, ensure};
use ratio_protocol::TaskGraph;
use serde::{Deserialize, Serialize};

use crate::{
    config::{Config, Worker},
    session::WritePath,
};

pub(super) const ARCHITECT: &str = include_str!("architect.txt");

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Campaign {
    pub tasks: TaskGraph,
    pub ownership: BTreeMap<String, Vec<WritePath>>,
    pub rationale: String,
    #[serde(default)]
    pub verification_checks: Vec<String>,
    #[serde(default)]
    pub prefer_staging: bool,
    pub max_parallel_agents: Option<usize>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Check {
    pub id: String,
    pub source: String,
    pub command: String,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Execution {
    pub stage_workspace: bool,
    pub max_parallel_agents: usize,
    pub verification_commands: Vec<String>,
    pub selected_checks: Vec<Check>,
    pub available_checks: Vec<Check>,
    pub verification_from_profile: bool,
}

impl Execution {
    pub(crate) fn additional_checks(&self, ids: &[String]) -> Result<Vec<Check>> {
        let mut seen = BTreeSet::new();
        ids.iter()
            .map(|id| {
                ensure!(seen.insert(id), "duplicate verification check {id:?}");
                self.available_checks
                    .iter()
                    .find(|check| &check.id == id)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("unknown verification check {id:?}"))
            })
            .collect()
    }
}

pub(super) struct WorkspaceFacts {
    pub checks: Vec<Check>,
    pub git_root_with_head: bool,
}

impl WorkspaceFacts {
    pub(super) fn read(workspace: &Path, discover_checks: bool, detect_staging: bool) -> Self {
        Self {
            checks: if discover_checks {
                discover(workspace)
            } else {
                Vec::new()
            },
            git_root_with_head: detect_staging && crate::staging::is_root_with_head(workspace),
        }
    }

    pub(super) fn resolve(
        &self,
        config: &Config,
        campaign: Option<&Campaign>,
        workers: &[Worker],
    ) -> Result<Execution> {
        let limits = &config.orchestration;
        let mut execution = Execution {
            stage_workspace: limits.stage_workspace.unwrap_or_default(),
            max_parallel_agents: limits.max_parallel_agents,
            verification_commands: limits.verification_commands.clone().unwrap_or_default(),
            selected_checks: Vec::new(),
            available_checks: self.checks.clone(),
            verification_from_profile: limits.verification_commands.is_some(),
        };
        if let Some(campaign) = campaign {
            ensure!(
                !campaign.rationale.trim().is_empty(),
                "campaign requires a rationale"
            );
            crate::orchestrator::validate_campaign_plan(
                &campaign.tasks,
                &campaign.ownership,
                workers,
                &config.cwd,
            )?;
            if let Some(width) = campaign.max_parallel_agents {
                ensure!(
                    (1..=limits.max_parallel_agents).contains(&width),
                    "campaign concurrency must be within the profile limit"
                );
                execution.max_parallel_agents = width;
            }
            execution.selected_checks =
                execution.additional_checks(&campaign.verification_checks)?;
            execution.verification_commands.extend(
                execution
                    .selected_checks
                    .iter()
                    .map(|check| check.command.clone()),
            );
            if limits.stage_workspace.is_none() {
                execution.stage_workspace = campaign.prefer_staging
                    && self.git_root_with_head
                    && !execution.verification_commands.is_empty();
            }
        }
        Ok(execution)
    }
}

fn manifest(root: &Path, name: &str) -> Option<String> {
    const MAX_BYTES: u64 = 256 * 1024;
    let path = root.join(name);
    let metadata = path.symlink_metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return None;
    }
    let mut text = String::new();
    fs::File::open(path)
        .ok()?
        .take(MAX_BYTES + 1)
        .read_to_string(&mut text)
        .ok()?;
    (text.len() as u64 <= MAX_BYTES).then_some(text)
}

fn runner_available(name: &str) -> bool {
    let Some(search_path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&search_path)
        .filter(|path| path.is_absolute())
        .any(|path| {
            #[cfg(windows)]
            {
                ["exe", "cmd", "bat"]
                    .iter()
                    .any(|extension| path.join(name).with_extension(extension).is_file())
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                path.join(name).metadata().is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
            }
        })
}

fn discover(root: &Path) -> Vec<Check> {
    let mut checks = Vec::new();
    let mut add = |id: &str, source: &str, command: String| {
        checks.push(Check {
            id: id.into(),
            source: source.into(),
            command,
        })
    };
    if runner_available("npm")
        && let Some(package) = manifest(root, "package.json")
        && let Ok(package) = serde_json::from_str::<serde_json::Value>(&package)
    {
        for script in ["test", "lint", "check", "typecheck"] {
            if let Some(body) = package["scripts"][script].as_str()
                && !body.trim().is_empty()
            {
                add(
                    &format!("npm-{script}"),
                    &format!("package.json scripts.{script}"),
                    format!("npm run {script}"),
                );
            }
        }
    }
    if runner_available("cargo")
        && manifest(root, "Cargo.toml")
            .and_then(|text| text.parse::<toml::Table>().ok())
            .is_some_and(|manifest| {
                ["package", "workspace"]
                    .iter()
                    .any(|key| manifest.get(*key).is_some_and(toml::Value::is_table))
            })
        && root
            .join("Cargo.lock")
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.is_file())
    {
        add(
            "cargo-test",
            "Cargo.toml and Cargo.lock",
            "cargo test --locked --workspace".into(),
        );
    }
    if runner_available("go")
        && manifest(root, "go.mod")
            .is_some_and(|text| text.lines().any(|line| line.starts_with("module ")))
    {
        add("go-test", "go.mod", "go test ./...".into());
    }
    checks
}
