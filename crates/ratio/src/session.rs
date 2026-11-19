use std::{
    fs::{self, File, OpenOptions},
    io::{BufReader, BufWriter, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{
    config::{Stakeholder, Worker},
    context::Dialogue,
    ipc::MailboxState,
    memory::SharedMemory,
};

const FORMAT: u32 = 2;

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictKind {
    Approved,
    NeedsRevision,
    Rejected,
}

impl VerdictKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Approved => "APPROVED",
            Self::NeedsRevision => "NEEDS_REVISION",
            Self::Rejected => "REJECTED",
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Verdict {
    pub verdict: VerdictKind,
    pub assessment: String,
    pub revision_instructions: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Phase {
    Plan,
    PlanConsult,
    PlanSynthesize,
    Coordinate,
    FreezePlan,
    Acknowledge,
    Work,
    Review,
    ReviewConsult,
    ReviewSynthesize,
    Approved,
    Rejected,
    Exhausted,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Plan => "Reviewer plan",
            Self::PlanConsult => "Planning consultation",
            Self::PlanSynthesize => "Plan synthesis",
            Self::Coordinate => "Worker coordination (read-only)",
            Self::FreezePlan => "Freeze worker plan",
            Self::Acknowledge => "Unanimous plan acknowledgement (read-only)",
            Self::Work => "Worker",
            Self::Review => "Reviewer assessment",
            Self::ReviewConsult => "Review consultation",
            Self::ReviewSynthesize => "Final review",
            Self::Approved => "Approved",
            Self::Rejected => "Rejected",
            Self::Exhausted => "Run limit reached",
        }
    }

    pub fn done(self) -> bool {
        matches!(self, Self::Approved | Self::Rejected | Self::Exhausted)
    }
}

#[derive(Deserialize, Serialize)]
pub struct Session {
    version: u32,
    pub goal: String,
    pub phase: Phase,
    pub cycle: usize,
    pub plan_attempt: usize,
    run_id: String,
    pub(crate) budget_started: bool,
    pub dialogue: Dialogue,
    pub assessment: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict_tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stakeholders: Option<Vec<Stakeholder>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewers: Option<Vec<Stakeholder>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workers: Option<Vec<Worker>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_plan: Option<WorkerPlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub worker_progress: Vec<WorkerProgress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) team_ipc: Option<TeamIpc>,
    #[serde(default, skip_serializing_if = "SharedMemory::is_empty")]
    pub(crate) memory: SharedMemory,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct WorkerPlan {
    pub text: String,
    pub id: String,
    pub acknowledgements: Vec<usize>,
    pub ownership: Vec<Vec<WritePath>>,
    pub attempt: usize,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WritePath {
    pub path: PathBuf,
    pub directory: bool,
}

impl WritePath {
    pub fn includes(&self, path: &Path) -> bool {
        path == self.path || (self.directory && path.starts_with(&self.path))
    }
}

impl WorkerPlan {
    pub fn new(text: String, cycle: usize, workers: &[Worker]) -> Self {
        Self::freeze(text, cycle, 1, workers, vec![Vec::new(); workers.len()])
    }

    pub fn freeze(
        text: String,
        cycle: usize,
        attempt: usize,
        workers: &[Worker],
        ownership: Vec<Vec<WritePath>>,
    ) -> Self {
        let mut plan = Self {
            text,
            id: String::new(),
            acknowledgements: Vec::new(),
            ownership,
            attempt,
        };
        plan.id = plan.digest(cycle, workers);
        plan
    }

    pub fn digest(&self, cycle: usize, workers: &[Worker]) -> String {
        let mut sha = ring::digest::Context::new(&ring::digest::SHA256);
        for part in [
            cycle.to_string().into_bytes(),
            serde_json::to_vec(workers).expect("serializable worker roster"),
            self.text.as_bytes().to_vec(),
            self.attempt.to_string().into_bytes(),
            serde_json::to_vec(&self.ownership).expect("serializable ownership map"),
        ] {
            sha.update(&(part.len() as u64).to_be_bytes());
            sha.update(&part);
        }
        sha.finish()
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    pub fn verified(&self, cycle: usize, workers: &[Worker]) -> bool {
        self.ownership.len() == workers.len()
            && self.id == self.digest(cycle, workers)
            && self.acknowledgements.iter().copied().eq(0..workers.len())
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct WorkerProgress {
    pub dialogue: Dialogue,
    pub assessment: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct TeamIpc {
    pub mailbox: MailboxState,
    pub stakeholders: Vec<WorkerProgress>,
}

impl Session {
    pub fn new(goal: String) -> Self {
        let dialogue = Dialogue::start(
            "Inspect the workspace and draft implementation instructions for the worker.".into(),
        );
        Self {
            version: FORMAT,
            goal,
            phase: Phase::Plan,
            cycle: 1,
            plan_attempt: 1,
            run_id: format!(
                "{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ),
            budget_started: false,
            dialogue,
            assessment: String::new(),
            verdict: None,
            verdict_tool_call_id: None,
            stakeholders: None,
            reviewers: None,
            workers: None,
            worker_plan: None,
            worker_progress: Vec::new(),
            team_ipc: None,
            memory: SharedMemory::default(),
        }
    }
}

pub struct Store {
    directory: PathBuf,
    lock: File,
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.lock);
    }
}

impl Store {
    pub(crate) fn budget_path(&self, session: &Session) -> Result<PathBuf> {
        ensure!(
            !session.run_id.is_empty()
                && session
                    .run_id
                    .bytes()
                    .all(|ch| ch.is_ascii_digit() || ch == b'-'),
            "invalid run ID"
        );
        Ok(self
            .directory
            .join(format!("budget-{}.json", session.run_id)))
    }

    pub fn archive_old_checkpoint(&self) -> Result<()> {
        let path = self.directory.join("session.json");
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let data: serde_json::Value =
            serde_json::from_reader(BufReader::new(file)).context("inspect previous checkpoint")?;
        if data.get("version").and_then(serde_json::Value::as_u64) != Some(u64::from(FORMAT)) {
            let backup = tempfile::Builder::new()
                .prefix("session-old-")
                .suffix(".json")
                .tempfile_in(&self.directory)?;
            let mut backup = backup;
            std::io::copy(&mut File::open(path)?, &mut backup)?;
            backup.as_file().sync_all()?;
            backup.keep().context("archive previous checkpoint")?;
        }
        Ok(())
    }
    pub fn open(workspace: &Path) -> Result<Self> {
        let directory = workspace.join(".ratio");
        fs::create_dir_all(&directory)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("session.lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock).context("lock Ratio session for this workspace")?;
        Ok(Self { directory, lock })
    }

    pub fn load(&self) -> Result<Session> {
        let path = self.directory.join("session.json");
        // Check the version before deserializing version-dependent fields.
        let file = File::open(&path).with_context(|| format!("open {}", path.display()))?;
        let data: serde_json::Value =
            serde_json::from_reader(BufReader::new(file)).context("decode Ratio checkpoint")?;
        ensure!(
            data.get("version").and_then(serde_json::Value::as_u64) == Some(u64::from(FORMAT)),
            "unsupported Ratio checkpoint version"
        );
        serde_json::from_value(data).context("decode Ratio session")
    }

    pub fn save(&self, session: &Session) -> Result<()> {
        let mut temp = tempfile::NamedTempFile::new_in(&self.directory)?;
        {
            let mut writer = BufWriter::new(temp.as_file_mut());
            serde_json::to_writer(&mut writer, session)?;
            writer.flush()?;
        }
        temp.as_file().sync_all()?;
        temp.persist(self.directory.join("session.json"))
            .context("replace Ratio checkpoint")?;
        Ok(())
    }
}
