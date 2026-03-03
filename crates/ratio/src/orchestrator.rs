//! Core orchestration engine.
//!
//! The orchestrator manages two LLM agents:
//! - **Reviewer** — accepts the user's goal, asks clarifying questions until
//!   the task is irrefutably understood, formulates work instructions, and
//!   reviews worker output in unlimited cycles until approved or rejected.
//! - **Worker** — executes work instructions, maintains implementation notes
//!   on disk, and keeps the shared todo list up to date.
//!
//! There is **no cycle limit** — the loop runs until the reviewer approves,
//! explicitly rejects, or the user aborts.

use tokio::sync::mpsc;

use crate::config::Config;
use crate::protocol::{AgentEvent, StopReason, WorkerConnection};
use crate::session::SessionState;
use crate::subprocess::AgentProcess;

// ---------------------------------------------------------------------------
// Orchestrator state machine
// ---------------------------------------------------------------------------

/// The top-level state of the orchestrator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Waiting for the user to provide / confirm the goal.
    Idle,
    /// Setting up agent subprocesses and ACP sessions.
    Initializing,
    /// The reviewer is asking questions and formulating the work instruction.
    Planning,
    /// Worker is actively executing a task.
    Working,
    /// Reviewer is inspecting the worker's output via LLM.
    Reviewing,
    /// Worker is applying revisions based on reviewer feedback.
    Revising,
    /// Task is approved — orchestration complete.
    Approved,
    /// Task failed or was rejected.
    Failed(String),
    /// User requested emergency stop.
    Aborted,
}

/// A single review cycle's verdict, as determined by the reviewer LLM.
#[derive(Debug, Clone)]
pub enum ReviewVerdict {
    /// The output meets all requirements.
    Approved { summary: String },
    /// The output needs revisions; includes the feedback to send back.
    NeedsRevision { feedback: String },
    /// The output is fatally flawed and cannot be fixed iteratively.
    Rejected { reason: String },
}

/// Record of a single work-review cycle.
#[derive(Debug, Clone)]
pub struct CycleRecord {
    pub cycle: usize,
    pub worker_instruction: String,
    pub worker_output: String,
    pub worker_stop_reason: StopReason,
    pub reviewer_assessment: String,
    pub verdict: ReviewVerdict,
}

// ---------------------------------------------------------------------------
// Orchestrator
// ---------------------------------------------------------------------------

/// The main orchestration engine.
///
/// Drives two LLM agents (reviewer + worker) through unlimited iterative
/// work-review cycles until the reviewer approves, explicitly rejects,
/// or an error / abort occurs.
pub struct Orchestrator {
    config: Config,
    phase: Phase,
    cycles: Vec<CycleRecord>,
    event_tx: mpsc::UnboundedSender<OrchestratorEvent>,
}

/// Events emitted by the orchestrator to the UI layer.
#[derive(Debug, Clone)]
pub enum OrchestratorEvent {
    /// Phase transition.
    PhaseChanged(Phase),
    /// An agent-level event from the worker.
    WorkerEvent(AgentEvent),
    /// An agent-level event from the reviewer.
    ReviewerEvent(AgentEvent),
    /// A log message from the orchestrator itself.
    Log(LogLevel, String),
    /// A review cycle completed.
    CycleCompleted(CycleRecord),
    /// The entire orchestration finished (approved or failed).
    Finished(Phase),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

impl Orchestrator {
    pub fn new(config: Config, event_tx: mpsc::UnboundedSender<OrchestratorEvent>) -> Self {
        Self {
            config,
            phase: Phase::Idle,
            cycles: Vec::new(),
            event_tx,
        }
    }

    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    pub fn cycles(&self) -> &[CycleRecord] {
        &self.cycles
    }

    fn set_phase(&mut self, phase: Phase) {
        self.phase = phase.clone();
        let _ = self.event_tx.send(OrchestratorEvent::PhaseChanged(phase));
    }

    /// Persist session state for resume capability.
    fn save_session(
        &self,
        reviewer_conn: &WorkerConnection,
        worker_conn: &WorkerConnection,
        last_active: &str,
    ) {
        let reviewer_id = reviewer_conn
            .session_id()
            .map(|s| s.to_string())
            .unwrap_or_default();
        let worker_id = worker_conn
            .session_id()
            .map(|s| s.to_string())
            .unwrap_or_default();

        let phase_str = match &self.phase {
            Phase::Idle => "idle",
            Phase::Initializing => "initializing",
            Phase::Planning => "planning",
            Phase::Working => "working",
            Phase::Reviewing => "reviewing",
            Phase::Revising => "revising",
            Phase::Approved => "approved",
            Phase::Failed(_) => "failed",
            Phase::Aborted => "aborted",
        };

        let state = SessionState {
            reviewer_session_id: reviewer_id,
            worker_session_id: worker_id,
            last_active_agent: last_active.to_string(),
            phase: phase_str.to_string(),
            cycle: self.cycles.len(),
            goal: self.config.goal.clone(),
        };

        if let Err(e) = state.save(&self.config.cwd) {
            self.log(LogLevel::Warn, format!("Failed to save session state: {e}"));
        }
    }

    fn log(&self, level: LogLevel, msg: impl Into<String>) {
        let _ = self
            .event_tx
            .send(OrchestratorEvent::Log(level, msg.into()));
    }

    // -----------------------------------------------------------------------
    // Prompt construction
    // -----------------------------------------------------------------------

    /// Shared instructions about todo list and implementation notes that both
    /// agents receive.
    fn shared_instructions(&self) -> String {
        format!(
            "\n\n═══ SHARED WORKSPACE PROTOCOL ═══\n\n\
             IMPLEMENTATION NOTES:\n\
             You MUST maintain an `agents.md` file (or `claude.md` if that is the convention \
             in the project) in the working directory. This file must contain:\n\
             - Current understanding of the task\n\
             - Implementation decisions and rationale\n\
             - Open questions and blockers\n\
             - Progress notes\n\
             Keep this file up to date as you work. The reviewer will read it.\n\n\
             TODO LIST:\n\
             You MUST use the TodoWrite tool to maintain a shared todo list. \
             This todo list is visible to BOTH the reviewer and the worker in real time \
             via the orchestrator's TUI. It is critical that you keep it accurate and \
             up to date as tasks are started, progressed, and completed.\n\n\
             Working directory: {cwd}\n\
             ═══ END SHARED WORKSPACE PROTOCOL ═══",
            cwd = self.config.cwd.display(),
        )
    }

    /// Build the prompt that asks the reviewer to deeply understand the task
    /// before formulating work instructions.
    fn build_planning_prompt(&self) -> String {
        let constraints = self.config.constraints.render_prompt_section();
        let constraint_block = if constraints.is_empty() {
            String::new()
        } else {
            format!("\n\n{constraints}\n")
        };

        let system_prefix = self
            .config
            .orchestration
            .reviewer_system_prompt
            .as_deref()
            .unwrap_or(
                "You are a senior technical reviewer and task planner. \
                 Your primary responsibility is to ensure tasks are understood \
                 completely and unambiguously before any work begins.",
            );

        let shared = self.shared_instructions();

        format!(
            "{system_prefix}\n\n\
             A user has requested the following goal:\n\n\
             ═══ GOAL ═══\n\
             {goal}\n\
             ═══ END GOAL ═══\n\
             {constraint_block}\
             {shared}\n\n\
             ═══ PLANNING PHASE INSTRUCTIONS ═══\n\n\
             Your job right now is to DEEPLY UNDERSTAND the task before producing \
             any work instruction. You must:\n\n\
             1. READ the codebase thoroughly — examine the project structure, existing \
                code, tests, configuration files, and any documentation.\n\
             2. IDENTIFY every open question, ambiguity, or missing detail in the goal. \
                Think about edge cases, implicit requirements, architectural decisions, \
                and potential conflicts with existing code.\n\
             3. Keep asking questions and investigating until you are ABSOLUTELY CERTAIN \
                that the task is irrefutably understood. Leave NO open questions.\n\
             4. Once you are confident, write a comprehensive `agents.md` file with your \
                analysis and create an initial todo list with the TodoWrite tool.\n\
             5. THEN produce a DETAILED, PRECISE work instruction for the worker agent.\n\n\
             The worker agent has access to the filesystem, can run commands, and can \
             edit files. Be specific about:\n\
             - What files to examine, modify, or create\n\
             - What tools/commands the worker must run\n\
             - All constraints that must be followed\n\
             - Expected outcomes and acceptance criteria\n\n\
             Tell the worker:\n\
             - It MUST use TodoWrite to keep the shared todo list updated\n\
             - It MUST keep agents.md updated with implementation notes\n\
             - It must summarize its changes when done\n\
             - It must NOT ask follow-up questions — just do the work\n\
             - The todo list is shared with the reviewer and must be kept current\n\n\
             Output your work instruction at the end, after you've completed your analysis.\n\
             ═══ END PLANNING INSTRUCTIONS ═══",
            goal = self.config.goal,
        )
    }

    /// Build the prompt that asks the reviewer to assess the worker's output.
    fn build_review_prompt(&self, cycle: usize, instruction: &str, output: &str) -> String {
        let constraints = self.config.constraints.render_prompt_section();

        let system_prefix = self
            .config
            .orchestration
            .reviewer_system_prompt
            .as_deref()
            .unwrap_or(
                "You are a senior technical reviewer. Your job is to critically \
                 evaluate the output of a coding agent against the original goal \
                 and all specified constraints.",
            );

        let shared = self.shared_instructions();

        format!(
            "{system_prefix}\n\n\
             ═══ ORIGINAL GOAL ═══\n\
             {goal}\n\
             ═══ END GOAL ═══\n\n\
             {constraints}\n\n\
             {shared}\n\n\
             ═══ INSTRUCTION SENT TO WORKER ═══\n\
             {instruction}\n\
             ═══ END INSTRUCTION ═══\n\n\
             ═══ WORKER OUTPUT (cycle {cycle}) ═══\n\
             {output}\n\
             ═══ END WORKER OUTPUT ═══\n\n\
             IMPORTANT: Read the agents.md file and the current todo list to understand \
             the full context. Check the actual files on disk — don't just trust the \
             worker's summary.\n\n\
             Review the worker's output thoroughly. Check:\n\
             1. Did the worker accomplish the goal?\n\
             2. Were all required tools used?\n\
             3. Were all constraints respected?\n\
             4. Is the code correct, idiomatic, and complete?\n\
             5. Are there any issues, omissions, or violations?\n\
             6. Is the todo list accurately reflecting the current state?\n\
             7. Is agents.md up to date with implementation notes?\n\n\
             Update the todo list with your findings using TodoWrite.\n\
             Update agents.md with your review notes.\n\n\
             You MUST respond in EXACTLY this format:\n\n\
             VERDICT: APPROVED|NEEDS_REVISION|REJECTED\n\n\
             ASSESSMENT:\n\
             <your detailed reasoning>\n\n\
             If NEEDS_REVISION, add:\n\
             FEEDBACK:\n\
             <specific, actionable instructions for the worker to fix the issues>\n\n\
             If REJECTED, add:\n\
             REASON:\n\
             <why this cannot be fixed iteratively>",
            goal = self.config.goal,
        )
    }

    /// Build the prompt that sends the reviewer's feedback back to the worker
    /// as a revision instruction.
    fn build_revision_prompt(&self, feedback: &str, cycle: usize) -> String {
        let constraints = self.config.constraints.render_prompt_section();
        let constraint_reminder = if constraints.is_empty() {
            String::new()
        } else {
            format!("\n\nReminder of constraints you must follow:\n{constraints}")
        };

        let shared = self.shared_instructions();

        format!(
            "REVISION REQUEST (cycle {cycle}):\n\n\
             The reviewer found issues with your previous output. \
             You must address ALL of the following:\n\n\
             {feedback}\n\n\
             {shared}\n\n\
             IMPORTANT:\n\
             - Update the todo list (TodoWrite) to reflect what needs to be fixed\n\
             - Update agents.md with notes about the revision\n\
             - Fix every issue listed above\n\
             - When done, clearly summarize what you changed\n\
             - Do NOT ask follow-up questions\n\
             {constraint_reminder}",
        )
    }

    // -----------------------------------------------------------------------
    // Parsing the reviewer's structured response
    // -----------------------------------------------------------------------

    /// Parse the reviewer LLM's response into a structured verdict.
    fn parse_reviewer_response(&self, response: &str) -> ReviewVerdict {
        let upper = response.to_uppercase();

        // Find the verdict line.
        let verdict_str = if upper.contains("VERDICT: APPROVED")
            || upper.contains("VERDICT:APPROVED")
        {
            "APPROVED"
        } else if upper.contains("VERDICT: NEEDS_REVISION")
            || upper.contains("VERDICT:NEEDS_REVISION")
            || upper.contains("VERDICT: NEEDS REVISION")
        {
            "NEEDS_REVISION"
        } else if upper.contains("VERDICT: REJECTED")
            || upper.contains("VERDICT:REJECTED")
        {
            "REJECTED"
        } else {
            // Fallback heuristic.
            if upper.contains("APPROVED") && !upper.contains("NOT APPROVED") {
                "APPROVED"
            } else if upper.contains("REJECT") {
                "REJECTED"
            } else {
                "NEEDS_REVISION"
            }
        };

        match verdict_str {
            "APPROVED" => {
                let summary = Self::extract_section(response, "ASSESSMENT:")
                    .unwrap_or_else(|| response.to_string());
                ReviewVerdict::Approved { summary }
            }
            "REJECTED" => {
                let reason = Self::extract_section(response, "REASON:")
                    .or_else(|| Self::extract_section(response, "ASSESSMENT:"))
                    .unwrap_or_else(|| response.to_string());
                ReviewVerdict::Rejected { reason }
            }
            _ => {
                let feedback = Self::extract_section(response, "FEEDBACK:")
                    .or_else(|| Self::extract_section(response, "ASSESSMENT:"))
                    .unwrap_or_else(|| response.to_string());
                ReviewVerdict::NeedsRevision { feedback }
            }
        }
    }

    /// Extract text following a section header (e.g. "FEEDBACK:\n<text>").
    fn extract_section(text: &str, header: &str) -> Option<String> {
        let upper = text.to_uppercase();
        let header_upper = header.to_uppercase();
        let idx = upper.find(&header_upper)?;
        let after = &text[idx + header.len()..];

        let section_headers = ["VERDICT:", "ASSESSMENT:", "FEEDBACK:", "REASON:"];
        let end = section_headers
            .iter()
            .filter_map(|h| {
                let h_upper = h.to_uppercase();
                after.to_uppercase().find(&h_upper)
            })
            .filter(|&pos| pos > 0)
            .min()
            .unwrap_or(after.len());

        let result = after[..end].trim().to_string();
        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }

    // -----------------------------------------------------------------------
    // Main orchestration loop — NO CYCLE LIMIT
    // -----------------------------------------------------------------------

    /// Run the full orchestration loop with two LLM agents.
    ///
    /// The loop runs indefinitely until the reviewer approves, explicitly
    /// rejects, or an abort signal is received.
    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        &mut self,
        reviewer_conn: &WorkerConnection,
        worker_conn: &WorkerConnection,
        reviewer_proc: &mut AgentProcess,
        worker_proc: &mut AgentProcess,
        mut worker_event_rx: mpsc::UnboundedReceiver<AgentEvent>,
        mut reviewer_event_rx: mpsc::UnboundedReceiver<AgentEvent>,
        mut abort_rx: mpsc::UnboundedReceiver<()>,
    ) -> anyhow::Result<Phase> {
        // Forward agent events to the orchestrator event channel.
        let orch_tx_w = self.event_tx.clone();
        tokio::task::spawn_local(async move {
            while let Some(evt) = worker_event_rx.recv().await {
                let _ = orch_tx_w.send(OrchestratorEvent::WorkerEvent(evt));
            }
        });
        let orch_tx_r = self.event_tx.clone();
        tokio::task::spawn_local(async move {
            while let Some(evt) = reviewer_event_rx.recv().await {
                let _ = orch_tx_r.send(OrchestratorEvent::ReviewerEvent(evt));
            }
        });

        // ── Phase 1: Planning ────────────────────────────────────────────
        // Reviewer deeply investigates the codebase and formulates the work
        // instruction. It must ask questions and investigate until the task
        // is irrefutably understood.

        self.set_phase(Phase::Planning);
        self.save_session(reviewer_conn, worker_conn, "reviewer");
        self.log(
            LogLevel::Info,
            "Reviewer is analyzing the codebase and formulating work instruction...",
        );

        let planning_prompt = self.build_planning_prompt();

        let plan_result = tokio::select! {
            result = reviewer_conn.prompt(&planning_prompt) => result,
            _ = abort_rx.recv() => {
                return self.abort(reviewer_conn, worker_conn, reviewer_proc, worker_proc).await;
            }
        };

        let (_, work_instruction) = match plan_result {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("Reviewer failed to produce work instruction: {e}");
                self.log(LogLevel::Error, &msg);
                self.set_phase(Phase::Failed(msg.clone()));
                let _ = self.event_tx.send(OrchestratorEvent::Finished(Phase::Failed(msg)));
                return Ok(self.phase.clone());
            }
        };

        if work_instruction.trim().is_empty() {
            let msg = "Reviewer produced an empty work instruction.".to_string();
            self.log(LogLevel::Error, &msg);
            self.set_phase(Phase::Failed(msg.clone()));
            let _ = self.event_tx.send(OrchestratorEvent::Finished(Phase::Failed(msg)));
            return Ok(self.phase.clone());
        }

        self.log(
            LogLevel::Info,
            format!(
                "Reviewer produced work instruction ({} chars). Sending to worker.",
                work_instruction.len()
            ),
        );

        // ── Phase 2: Unlimited work-review loop ──────────────────────────

        let mut current_instruction = work_instruction;
        let mut cycle: usize = 0;

        loop {
            cycle += 1;

            // Check for abort.
            if abort_rx.try_recv().is_ok() {
                return self.abort(reviewer_conn, worker_conn, reviewer_proc, worker_proc).await;
            }

            // ── Send instruction to worker ───────────────────────────────

            self.log(
                LogLevel::Info,
                format!("Cycle {cycle}: dispatching to worker..."),
            );
            self.set_phase(if cycle == 1 {
                Phase::Working
            } else {
                Phase::Revising
            });
            self.save_session(reviewer_conn, worker_conn, "worker");

            let worker_result = tokio::select! {
                result = worker_conn.prompt(&current_instruction) => result,
                _ = abort_rx.recv() => {
                    return self.abort(reviewer_conn, worker_conn, reviewer_proc, worker_proc).await;
                }
            };

            let (worker_stop, worker_output) = match worker_result {
                Ok(r) => r,
                Err(e) => {
                    let msg = format!("Worker prompt failed: {e}");
                    self.log(LogLevel::Error, &msg);
                    self.set_phase(Phase::Failed(msg.clone()));
                    let _ = self
                        .event_tx
                        .send(OrchestratorEvent::Finished(Phase::Failed(msg)));
                    return Ok(self.phase.clone());
                }
            };

            self.log(
                LogLevel::Info,
                format!(
                    "Worker finished (stop: {worker_stop:?}, {} chars). \
                     Sending to reviewer for assessment.",
                    worker_output.len()
                ),
            );

            // ── Send output to reviewer for assessment ───────────────────

            self.set_phase(Phase::Reviewing);
            self.save_session(reviewer_conn, worker_conn, "reviewer");

            let review_prompt =
                self.build_review_prompt(cycle, &current_instruction, &worker_output);

            let review_result = tokio::select! {
                result = reviewer_conn.prompt(&review_prompt) => result,
                _ = abort_rx.recv() => {
                    return self.abort(reviewer_conn, worker_conn, reviewer_proc, worker_proc).await;
                }
            };

            let (_, reviewer_response) = match review_result {
                Ok(r) => r,
                Err(e) => {
                    let msg = format!("Reviewer assessment failed: {e}");
                    self.log(LogLevel::Error, &msg);
                    self.set_phase(Phase::Failed(msg.clone()));
                    let _ = self
                        .event_tx
                        .send(OrchestratorEvent::Finished(Phase::Failed(msg)));
                    return Ok(self.phase.clone());
                }
            };

            self.log(
                LogLevel::Info,
                format!(
                    "Reviewer assessment received ({} chars).",
                    reviewer_response.len()
                ),
            );

            // ── Parse verdict ────────────────────────────────────────────

            let verdict = self.parse_reviewer_response(&reviewer_response);

            let record = CycleRecord {
                cycle,
                worker_instruction: current_instruction.clone(),
                worker_output: worker_output.clone(),
                worker_stop_reason: worker_stop.clone(),
                reviewer_assessment: reviewer_response.clone(),
                verdict: verdict.clone(),
            };
            self.cycles.push(record.clone());
            let _ = self
                .event_tx
                .send(OrchestratorEvent::CycleCompleted(record));

            match verdict {
                ReviewVerdict::Approved { ref summary } => {
                    self.log(LogLevel::Info, format!("APPROVED: {summary}"));
                    self.set_phase(Phase::Approved);
                    SessionState::remove(&self.config.cwd);
                    let _ = self
                        .event_tx
                        .send(OrchestratorEvent::Finished(Phase::Approved));
                    return Ok(Phase::Approved);
                }
                ReviewVerdict::NeedsRevision { ref feedback } => {
                    self.log(
                        LogLevel::Warn,
                        format!("Cycle {cycle}: revision needed."),
                    );
                    current_instruction =
                        self.build_revision_prompt(feedback, cycle + 1);
                }
                ReviewVerdict::Rejected { ref reason } => {
                    let msg = format!("REJECTED: {reason}");
                    self.log(LogLevel::Error, &msg);
                    self.set_phase(Phase::Failed(msg.clone()));
                    let _ = self
                        .event_tx
                        .send(OrchestratorEvent::Finished(Phase::Failed(msg)));
                    return Ok(self.phase.clone());
                }
            }
        }
    }

    /// Emergency-stop both agents.
    async fn abort(
        &mut self,
        reviewer_conn: &WorkerConnection,
        worker_conn: &WorkerConnection,
        reviewer_proc: &mut AgentProcess,
        worker_proc: &mut AgentProcess,
    ) -> anyhow::Result<Phase> {
        self.log(LogLevel::Warn, "Abort signal received — killing both agents.");
        reviewer_conn.cancel().await.ok();
        worker_conn.cancel().await.ok();
        reviewer_proc.kill();
        worker_proc.kill();
        self.set_phase(Phase::Aborted);
        let _ = self
            .event_tx
            .send(OrchestratorEvent::Finished(Phase::Aborted));
        Ok(Phase::Aborted)
    }
}
