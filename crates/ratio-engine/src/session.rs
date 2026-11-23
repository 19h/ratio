use std::{
    cell::RefCell,
    fs::{self, File, OpenOptions},
    io::{BufReader, Write},
    path::{Path, PathBuf},
    rc::Rc,
};

use anyhow::{Context, Result, ensure};
use ratio_protocol::{
    AgentView, ContextReceipt, Effect, IpcEvent, RunStop, TaskGraph, TaskState, TaskView,
    VerificationView, Workflow,
};
use serde::{Deserialize, Serialize};

use crate::{
    Journal, RunReader,
    config::{Stakeholder, Worker},
    context::Dialogue,
    ipc::{GateState, MailboxState},
    jobs::Jobs,
    journal::CheckpointDetails,
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
    ProposeTeam,
    ContinuationReview,
    ContinuationDiscuss,
    ContinuationDecision,
    OnboardWorker,
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
    Publish,
    Approved,
    Rejected,
    Exhausted,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Self::ContinuationReview => "Continuation review",
            Self::ContinuationDiscuss => "Continuation discussion",
            Self::ContinuationDecision => "Continuation decision",
            Self::ProposeTeam => "Propose team",
            Self::OnboardWorker => "Onboard worker",
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
            Self::Publish => "Publish approved candidate",
            Self::Approved => "Approved",
            Self::Rejected => "Rejected",
            Self::Exhausted => "Run limit reached",
        }
    }

    pub fn done(self) -> bool {
        matches!(self, Self::Approved | Self::Rejected | Self::Exhausted)
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Continuation {
    pub id: String,
    pub approved_checkpoint: u64,
    pub cycle: usize,
    pub prompt: String,
    pub initial_reports: Vec<(String, String)>,
    pub discussion: Vec<(String, String)>,
    pub decision: Option<ContinuationDecision>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContinuationDecision {
    pub decision: ContinuationChoice,
    pub reason: String,
    pub plan: Option<String>,
}

#[derive(Clone, Copy, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ContinuationChoice {
    Continue,
    Decline,
}

impl Continuation {
    fn overview(&self, phase: Phase, panel: Option<&PanelProgress>) -> serde_json::Value {
        let completed = panel.map_or(0, |panel| {
            panel
                .agents
                .iter()
                .filter(|agent| agent.assessment.is_some())
                .count()
        });
        serde_json::json!({
            "id": self.id, "approved_checkpoint": self.approved_checkpoint,
            "cycle": self.cycle, "prompt": self.prompt, "decision": self.decision,
            "reviewed": if phase == Phase::ContinuationReview { completed } else { self.initial_reports.len() },
            "discussed": if phase == Phase::ContinuationDiscuss { completed } else { self.discussion.len() }
        })
    }
}

#[derive(Deserialize, Serialize)]
pub struct Session {
    version: u32,
    pub goal: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub team_acceptance: Vec<String>,
    #[serde(default)]
    pub(crate) work_intake_closed: bool,
    #[serde(default)]
    pub(crate) last_worker_request: Option<i64>,
    #[serde(default)]
    pub(crate) active_worker_request: Option<i64>,
    #[serde(default)]
    pub(crate) worker_addition_status: Option<String>,
    #[serde(default)]
    pub(crate) onboarding: Option<WorkerOnboarding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) continuation: Option<Continuation>,
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) phase_result: Option<String>,
    pub cycle: usize,
    pub plan_attempt: usize,
    run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) stopped: Option<RunStop>,
    pub(crate) budget_started: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) campaign_execution: Option<crate::setup::Execution>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) frozen_campaign_checks: Vec<crate::setup::Check>,
    /// Records whether the selected execution workspace was opened successfully.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) workspace_prepared: Option<bool>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub require_task_graph: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) staging_commands: Option<Vec<String>>,
    /// Commands a direct run checks in the live workspace after the work phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) verification_commands: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) verification: Option<Verification>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) panel: Option<PanelProgress>,
    #[serde(default, skip_serializing_if = "SharedMemory::is_empty")]
    pub(crate) memory: SharedMemory,
    #[serde(default, skip_serializing_if = "Jobs::is_empty")]
    pub(crate) jobs: Jobs,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct WorkerOnboarding {
    pub worker: Worker,
    pub plan: WorkerPlan,
    pub mentor: usize,
    pub stakeholder: Option<usize>,
    pub briefed: bool,
    #[serde(default)]
    pub brief: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct WorkerPlan {
    pub text: String,
    pub id: String,
    pub acknowledgements: Vec<usize>,
    pub ownership: Vec<Vec<WritePath>>,
    pub attempt: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph: Option<TaskGraph>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) additional_checks: Vec<crate::setup::Check>,
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
            graph: None,
            additional_checks: Vec::new(),
        };
        plan.id = plan.digest(cycle, workers);
        plan
    }

    pub fn bind_graph(
        mut self,
        graph: Option<TaskGraph>,
        cycle: usize,
        workers: &[Worker],
    ) -> Self {
        self.graph = graph;
        self.id = self.digest(cycle, workers);
        self
    }

    pub(crate) fn bind_checks(
        mut self,
        checks: Vec<crate::setup::Check>,
        cycle: usize,
        workers: &[Worker],
    ) -> Self {
        self.additional_checks = checks;
        self.id = self.digest(cycle, workers);
        self
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
        if let Some(graph) = &self.graph {
            let part = serde_json::to_vec(graph).expect("serializable task graph");
            sha.update(&(part.len() as u64).to_be_bytes());
            sha.update(&part);
        }
        if !self.additional_checks.is_empty() {
            let part = serde_json::to_vec(&self.additional_checks).expect("serializable checks");
            sha.update(b"verification checks");
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
            && self
                .graph
                .as_ref()
                .is_none_or(|graph| graph.dependencies(workers.len()).is_ok())
            && self.id == self.digest(cycle, workers)
            && self.acknowledgements.iter().copied().eq(0..workers.len())
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct WorkerProgress {
    pub dialogue: Dialogue,
    pub assessment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_sha256: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Verification {
    pub digest: String,
    pub passed: bool,
    pub report: String,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct TeamIpc {
    pub mailbox: MailboxState,
    pub stakeholders: Vec<WorkerProgress>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct PanelProgress {
    pub phase: Phase,
    pub mailbox: MailboxState,
    pub gate: Option<GateState>,
    pub agents: Vec<WorkerProgress>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actor_ids: Vec<String>,
}

impl Session {
    fn workflow(&self) -> Workflow {
        let mut workflow = Workflow {
            stopped: self.stopped,
            plan_attempt: self.plan_attempt,
            parent_run: self.parent_run_id.clone(),
            worker_addition: self.worker_addition_status.clone(),
            active_worker_request: self.active_worker_request,
            panel_actors: self
                .panel
                .as_ref()
                .map_or_else(Vec::new, |panel| panel.actor_ids.clone()),
            verdict: self
                .verdict
                .as_ref()
                .map(|verdict| verdict.verdict.label().to_owned()),
            verification: self.verification.as_ref().map(|receipt| VerificationView {
                candidate: receipt.digest.clone(),
                passed: receipt.passed,
            }),
            ..Workflow::default()
        };
        workflow.agents.push(AgentView {
            id: "user".into(),
            name: "You".into(),
        });
        if self.phase == Phase::ProposeTeam || self.request.is_some() {
            workflow.agents.push(AgentView {
                id: crate::setup::ACTOR.into(),
                name: crate::setup::LABEL.into(),
            });
        }
        if self.phase == Phase::ProposeTeam {
            return workflow;
        }
        workflow.agents.push(AgentView {
            id: "lead_reviewer".into(),
            name: "Lead reviewer".into(),
        });
        workflow.agents.extend(
            self.workers
                .as_deref()
                .unwrap_or_default()
                .iter()
                .enumerate()
                .map(|(index, worker)| AgentView {
                    id: format!("worker_{}", index + 1),
                    name: worker.name.clone(),
                }),
        );
        if self.workers.as_deref().unwrap_or_default().is_empty() {
            workflow.agents.push(AgentView {
                id: "worker_1".into(),
                name: "Worker".into(),
            });
        }
        for (role, roster) in [
            ("reviewer", &self.reviewers),
            ("stakeholder", &self.stakeholders),
        ] {
            workflow.agents.extend(
                roster
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .enumerate()
                    .map(|(index, agent)| AgentView {
                        id: format!("{role}_{}", index + 1),
                        name: agent.name.clone(),
                    }),
            );
        }
        if let Some(newcomer) = &self.onboarding {
            let count = self.workers.as_ref().map_or(0, Vec::len);
            workflow.agents.push(AgentView {
                id: format!("worker_{}", count + 1),
                name: newcomer.worker.name.clone(),
            });
            if newcomer.stakeholder.is_none() {
                let stakeholders = self.stakeholders.as_ref().map_or(0, Vec::len);
                workflow.agents.push(AgentView {
                    id: format!("stakeholder_{}", stakeholders + 1),
                    name: "Onboarding adviser".into(),
                });
            }
        }
        self.for_each_dialogue(|actor, dialogue| {
            if let Some(receipt) = &dialogue.last_compaction {
                workflow.context.push(ContextReceipt {
                    actor: actor.into(),
                    folded_groups: receipt.folded_groups,
                    before_tokens: receipt.before_tokens,
                    after_tokens: receipt.after_tokens,
                });
            }
        });
        let Some(plan) = &self.worker_plan else {
            return workflow;
        };
        workflow.plan_id = Some(plan.id.clone());
        let workers = self.workers.as_deref().unwrap_or_default().len();
        let gate = self.panel.as_ref().and_then(|panel| panel.gate.as_ref());
        workflow.acknowledgements = (0..workers)
            .filter(|worker| {
                plan.acknowledgements.contains(worker)
                    || gate.is_some_and(|gate| gate.accepted.get(*worker) == Some(&true))
            })
            .map(|worker| format!("worker_{}", worker + 1))
            .collect();
        if let Some(graph) = &plan.graph {
            workflow.tasks = graph
                .0
                .iter()
                .cloned()
                .map(|task| {
                    let find_progress = |owner: &str| {
                        owner
                            .strip_prefix("worker_")
                            .and_then(|index| index.parse::<usize>().ok())
                            .and_then(|index| index.checked_sub(1))
                            .and_then(|index| self.worker_progress.get(index))
                    };
                    let is_complete = |owner| {
                        find_progress(owner).is_some_and(|progress| {
                            if self.staging_commands.is_some() {
                                progress.artifact_sha256.is_some()
                            } else {
                                progress.assessment.is_some()
                            }
                        })
                    };
                    let state = if workflow.acknowledgements.len() < workers {
                        TaskState::AwaitingAck
                    } else if is_complete(&task.worker) {
                        TaskState::Complete
                    } else if task.depends_on.iter().any(|id| {
                        graph
                            .0
                            .iter()
                            .find(|parent| parent.id == *id)
                            .is_none_or(|parent| !is_complete(&parent.worker))
                    }) {
                        TaskState::Blocked
                    } else if find_progress(&task.worker)
                        .is_some_and(|progress| progress.dialogue.tool_rounds > 0)
                    {
                        TaskState::Working
                    } else {
                        TaskState::Ready
                    };
                    let artifact_sha256 = find_progress(&task.worker)
                        .and_then(|progress| progress.artifact_sha256.clone());
                    TaskView {
                        task,
                        state,
                        artifact_sha256,
                    }
                })
                .collect();
        }
        workflow
    }

    pub fn fork(self, goal: Option<String>) -> Self {
        let same_goal = goal.as_ref().is_none_or(|goal| goal == &self.goal);
        let mut next = Self::new(goal.unwrap_or(self.goal));
        next.parent_run_id = Some(self.run_id);
        next.reviewers = self.reviewers;
        next.stakeholders = self.stakeholders;
        next.workers = self.workers;
        next.require_task_graph = self.require_task_graph;
        if same_goal {
            next.team_acceptance = self.team_acceptance;
        }
        next
    }

    fn effects(&self) -> Vec<Effect> {
        let mut effects = Vec::new();
        self.for_each_dialogue(|actor, dialogue| {
            dialogue.effects(actor, self.plan_attempt, &mut effects)
        });
        effects
    }

    fn for_each_dialogue(&self, mut visit: impl FnMut(&str, &Dialogue)) {
        let lead = if self.phase == Phase::Work {
            "worker_1"
        } else {
            "lead_reviewer"
        };
        self.jobs.for_each_dialogue(&mut visit);
        if self.phase != Phase::Work || self.worker_progress.is_empty() {
            visit(lead, &self.dialogue);
        }
        if self.phase == Phase::Work {
            for (index, worker) in self.worker_progress.iter().enumerate() {
                visit(&format!("worker_{}", index + 1), &worker.dialogue);
            }
            if let Some(ipc) = &self.team_ipc {
                for (index, agent) in ipc.stakeholders.iter().enumerate() {
                    visit(&format!("stakeholder_{}", index + 1), &agent.dialogue);
                }
            }
        }
        if let Some(panel) = &self.panel {
            for (index, agent) in panel.agents.iter().enumerate() {
                visit(&format!("panel_{}", index + 1), &agent.dialogue);
            }
        }
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn new(goal: String) -> Self {
        let dialogue = Dialogue::start(
            "Inspect the workspace and draft implementation instructions for the worker.".into(),
        );
        Self {
            version: FORMAT,
            continuation: None,
            goal,
            phase: Phase::Plan,
            phase_result: None,
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
            campaign_execution: None,
            frozen_campaign_checks: Vec::new(),
            workspace_prepared: None,
            require_task_graph: false,
            parent_run_id: None,
            stopped: None,
            instructions: None,
            staging_commands: None,
            verification_commands: None,
            verification: None,
            dialogue,
            assessment: String::new(),
            verdict: None,
            verdict_tool_call_id: None,
            stakeholders: None,
            reviewers: None,
            workers: None,
            request: None,
            team_acceptance: Vec::new(),
            work_intake_closed: false,
            last_worker_request: None,
            active_worker_request: None,
            worker_addition_status: None,
            onboarding: None,
            worker_plan: None,
            worker_progress: Vec::new(),
            team_ipc: None,
            panel: None,
            memory: SharedMemory::default(),
            jobs: Jobs::default(),
        }
    }
}

pub struct Store {
    directory: PathBuf,
    lock: File,
    pub(crate) journal: Rc<RefCell<Journal>>,
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.lock);
    }
}

impl Store {
    pub fn has_live_writer(workspace: &Path) -> Result<bool> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(workspace.join(".ratio/session.lock"))?;
        match fs2::FileExt::try_lock_exclusive(&lock) {
            Ok(()) => Ok(false),
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                Ok(true)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub fn record_control(&self, run: &str, action: &str, detail: &str) -> Result<()> {
        Ok(self
            .journal
            .borrow_mut()
            .record_control(run, action, detail)?)
    }
    pub fn submit_triage(&self, run: &str, text: &str) -> Result<i64> {
        let text = text.trim();
        ensure!(
            !text.is_empty() && text.len() <= 4096,
            "Triage information requires 1 to 4096 UTF-8 bytes"
        );
        let session = self.load_run(Some(run))?;
        ensure!(
            !session.phase.done() && session.stopped.is_none() && session.phase != Phase::Publish,
            "This run is no longer accepting triage"
        );
        Ok(self.journal.borrow().submit_triage(run, text)?)
    }

    pub fn retry_triage(&self, run: &str, id: i64) -> Result<bool> {
        Ok(self.journal.borrow().retry_triage(run, id)?)
    }

    pub fn request_worker(&self, run: &str, request: &str) -> Result<i64> {
        let request = request.trim();
        ensure!(
            !request.is_empty() && request.len() <= 4096,
            "worker request must be 1 to 4096 UTF-8 bytes"
        );
        let session = self.load_run(Some(run))?;
        let workers = session.workers.as_deref().unwrap_or_default();
        ensure!(
            session.phase == Phase::Work
                && session.stopped.is_none()
                && !session.work_intake_closed
                && (1..64).contains(&workers.len())
                && session
                    .worker_plan
                    .as_ref()
                    .is_some_and(|plan| plan.verified(session.cycle, workers)),
            "a worker can be added during Work under a verified frozen plan with fewer than 64 workers"
        );
        ensure!(
            self.pending_worker(run)?.is_none(),
            "a worker request is already pending"
        );
        Ok(self.journal.borrow().enqueue_worker(run, request)?)
    }

    pub(crate) fn pending_worker(&self, run: &str) -> Result<Option<(i64, String)>> {
        Ok(self.journal.borrow().worker_request(run)?)
    }

    pub(crate) fn finish_worker_request(&self, id: i64) -> Result<()> {
        Ok(self.journal.borrow().finish_worker_request(id)?)
    }
    pub fn mark_stopped(&self, run_id: &str, reason: RunStop) -> Result<()> {
        let Some(bytes) = self.journal.borrow().checkpoint(Some(run_id))? else {
            return Ok(());
        };
        let mut session = Self::decode(bytes.as_slice())?;
        if !session.phase.done() && session.stopped != Some(reason) {
            session.stopped = Some(reason);
            self.save(&session)?;
        }
        Ok(())
    }

    pub fn reader(workspace: &Path) -> Result<RunReader> {
        RunReader::open(&workspace.join(".ratio/runs.sqlite3")).context("open Ratio run history")
    }

    pub fn set_web_share(workspace: &Path, run: &str, digest: Option<&[u8]>) -> Result<()> {
        Journal::set_web_share(&workspace.join(".ratio/runs.sqlite3"), run, digest)
            .context("save Ratio web share")
    }

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
        let journal_path = directory.join("runs.sqlite3");
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&journal_path)
            {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error).context("create Ratio journal"),
            }
        }
        let journal = Journal::open(&journal_path).context("open Ratio run journal")?;
        Ok(Self {
            directory,
            lock,
            journal: Rc::new(RefCell::new(journal)),
        })
    }

    pub fn load(&self) -> Result<Session> {
        self.load_run(None)
    }

    pub fn load_run(&self, run_id: Option<&str>) -> Result<Session> {
        if let Some(content) = self.journal.borrow().checkpoint(run_id)? {
            return Self::decode(content.as_slice());
        }
        ensure!(run_id.is_none(), "unknown Ratio run");
        let path = self.directory.join("session.json");
        let file = File::open(&path).with_context(|| format!("open {}", path.display()))?;
        Self::decode(file)
    }

    pub fn begin_continuation(
        &self,
        run: &str,
        expected: u64,
        id: &str,
        prompt: &str,
    ) -> Result<Option<Session>> {
        let prompt = prompt.trim();
        ensure!(
            (1..=4096).contains(&prompt.len()),
            "Enter a follow-up prompt up to 4096 UTF-8 bytes"
        );
        ensure!(
            (1..=80).contains(&id.len())
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
            "Invalid continuation action ID"
        );
        let reader = RunReader::open(&self.directory.join("runs.sqlite3"))?;
        if reader.continuation_receipt(run, id, expected, prompt)? {
            return Ok(None);
        }
        let record = reader.run(run)?.context("Run not found")?;
        ensure!(
            record.phase == "Approved" && record.last_sequence == expected,
            "The approved checkpoint changed. Refresh and submit the follow-up again."
        );
        let mut session = self.load_run(Some(run))?;
        let prior = session.continuation.as_ref();
        let approved_checkpoint = prior
            .filter(|item| {
                item.decision
                    .as_ref()
                    .is_some_and(|decision| decision.decision == ContinuationChoice::Decline)
            })
            .map_or(expected, |item| item.approved_checkpoint);
        session.cycle = session
            .cycle
            .max(prior.map_or(0, |item| item.cycle))
            .checked_add(1)
            .context("Run cycle overflow")?;
        session.phase = Phase::ContinuationReview;
        session.stopped = None;
        session.panel = None;
        session.phase_result = None;
        session.dialogue = Dialogue::start("Review the follow-up request.".into());
        session.continuation = Some(Continuation {
            id: id.into(),
            approved_checkpoint,
            cycle: session.cycle,
            prompt: prompt.into(),
            initial_reports: Vec::new(),
            discussion: Vec::new(),
            decision: None,
        });
        session.dialogue.audit.push(crate::audit::Observation {
            key: format!("continuation-request-{id}"),
            kind: "continuation_request",
            actor: "user".into(),
            data: serde_json::json!({"text":prompt,"expected":expected}),
        });
        self.save(&session)?;
        Ok(Some(session))
    }

    pub(crate) fn load_checkpoint(&self, run: &str, sequence: u64) -> Result<Session> {
        let reader = RunReader::open(&self.directory.join("runs.sqlite3"))?;
        let bytes = reader
            .snapshot(run, Some(sequence))?
            .context("Continuation approval checkpoint is missing")?;
        Self::decode(bytes.as_slice())
    }

    pub(crate) fn recover_budget_checkpoint(&self, run: &str) -> Result<Option<Session>> {
        let reader = RunReader::open(&self.directory.join("runs.sqlite3"))?;
        let Some((sequence, _)) = reader.budget_resume_checkpoint(run)? else {
            return Ok(None);
        };
        let checkpoint = reader
            .snapshot(run, Some(sequence))?
            .context("pre-budget checkpoint is missing")?;
        Self::decode(checkpoint.as_slice()).map(Some)
    }

    fn decode(source: impl std::io::Read) -> Result<Session> {
        // Check the version before deserializing version-dependent fields.
        let data: serde_json::Value =
            serde_json::from_reader(BufReader::new(source)).context("decode Ratio checkpoint")?;
        ensure!(
            data.get("version").and_then(serde_json::Value::as_u64) == Some(u64::from(FORMAT)),
            "unsupported Ratio checkpoint version"
        );
        serde_json::from_value(data).context("decode Ratio session")
    }

    pub fn save(&self, session: &Session) -> Result<()> {
        self.save_with_traffic(session, Vec::new())
    }

    fn overview(session: &Session) -> serde_json::Value {
        serde_json::json!({
            "phase":session.phase,"stopped":session.stopped,
            "continuation":session.continuation.as_ref().map(|item| item.overview(session.phase, session.panel.as_ref())),
            "goal":session.goal,"request":session.request,"verdict":session.verdict,
            "parent_run_id":session.parent_run_id,"assessment":session.assessment,
            "team_acceptance":session.team_acceptance,"workers":session.workers,
            "reviewers":session.reviewers,"stakeholders":session.stakeholders,
            "worker_plan":session.worker_plan,
            "onboarding":session.onboarding.as_ref().map(|stage| serde_json::json!({
                "plan":stage.plan,"mentor":stage.mentor,"stakeholder":stage.stakeholder,"briefed":stage.briefed
            })),
        })
    }

    pub(crate) fn save_with_traffic(&self, session: &Session, ipc: Vec<IpcEvent>) -> Result<()> {
        let bytes = serde_json::to_vec(session)?;
        let mut audit = session.jobs.audit();
        session.for_each_dialogue(|actor, dialogue| {
            let actor = actor
                .strip_prefix("panel_")
                .and_then(|slot| slot.parse::<usize>().ok())
                .and_then(|slot| slot.checked_sub(1))
                .and_then(|slot| session.panel.as_ref()?.actor_ids.get(slot))
                .map_or(actor, String::as_str);
            audit.extend(dialogue.audit.pending().into_iter().map(|mut event| {
                if event.actor.is_empty() {
                    event.actor = actor.into();
                }
                event
            }));
        });
        self.journal
            .borrow_mut()
            .commit_checkpoint(
                &session.run_id,
                &format!("{:?}", session.phase),
                session.cycle,
                &bytes,
                CheckpointDetails {
                    effects: session.effects(),
                    ipc,
                    workflow: session.workflow(),
                    audit,
                    overview: Some(Self::overview(session)),
                },
            )
            .context("commit Ratio run checkpoint")?;
        session.for_each_dialogue(|_, dialogue| dialogue.audit.committed());
        session.jobs.committed();
        // The JSON file remains a compatibility projection. Resume reads the journal.
        let mut temp = tempfile::NamedTempFile::new_in(&self.directory)?;
        temp.write_all(&bytes)?;
        temp.as_file().sync_all()?;
        temp.persist(self.directory.join("session.json"))
            .context("replace Ratio checkpoint")?;
        Ok(())
    }
}
