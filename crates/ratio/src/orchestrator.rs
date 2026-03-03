//! Core orchestration engine.
//!
//! The orchestrator manages two LLM agents:
//! - **Reviewer** — the primary agent that accepts the user's goal, formulates
//!   work instructions, and performs substantive LLM-powered review of output
//! - **Worker** — the secondary agent that executes the work instructions
//!
//! The flow:
//! 1. User provides goal + constraints
//! 2. Reviewer receives the goal and produces a work instruction for the worker
//! 3. Worker executes the instruction and produces output
//! 4. Reviewer receives the output and either approves, requests revision, or rejects
//! 5. If revision needed: reviewer's feedback becomes the next instruction, loop

use tokio::sync::mpsc;

use crate::config::Config;
use crate::protocol::{AgentEvent, StopReason, WorkerConnection};
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
    /// The reviewer is formulating the initial work instruction.
    Planning,
    /// Worker is actively executing a task.
    Working,
    /// Reviewer is inspecting the worker's output via LLM.
    Reviewing,
    /// Worker is applying revisions based on reviewer feedback.
    Revising,
    /// Task is approved — orchestration complete.
    Approved,
    /// Task failed or was aborted.
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
/// Drives two LLM agents (reviewer + worker) through iterative work-review
/// cycles until the reviewer approves the output, the cycle limit is reached,
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

    fn log(&self, level: LogLevel, msg: impl Into<String>) {
        let _ = self
            .event_tx
            .send(OrchestratorEvent::Log(level, msg.into()));
    }

    // -----------------------------------------------------------------------
    // Prompt construction for the REVIEWER
    // -----------------------------------------------------------------------

    /// Build the prompt that asks the reviewer to formulate the initial work
    /// instruction for the worker.
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
                 Your job is to decompose goals into precise, actionable \
                 work instructions for a coding agent.",
            );

        format!(
            "{system_prefix}\n\n\
             A user has requested the following goal:\n\n\
             ═══ GOAL ═══\n\
             {goal}\n\
             ═══ END GOAL ═══\n\
             {constraint_block}\n\
             Your task: produce a DETAILED, PRECISE work instruction that a \
             coding agent (the \"worker\") will execute. The worker has access \
             to the filesystem, can run commands, and can edit files.\n\n\
             Requirements for your instruction:\n\
             1. Be specific about what files to examine, modify, or create\n\
             2. Explicitly state which tools/commands the worker must run\n\
             3. Include all constraints above — the worker must follow them\n\
             4. Tell the worker to summarize its changes when done\n\
             5. Tell the worker NOT to ask follow-up questions — just do the work\n\n\
             Output ONLY the work instruction. Nothing else.",
            goal = self.config.goal,
        )
    }

    /// Build the prompt that asks the reviewer to assess the worker's output.
    fn build_review_prompt(&self, cycle: usize, instruction: &str, output: &str) -> String {
        let constraints = self.config.constraints.render_prompt_section();
        let max = self.config.orchestration.max_review_cycles;

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

        format!(
            "{system_prefix}\n\n\
             ═══ ORIGINAL GOAL ═══\n\
             {goal}\n\
             ═══ END GOAL ═══\n\n\
             {constraints}\n\n\
             ═══ INSTRUCTION SENT TO WORKER ═══\n\
             {instruction}\n\
             ═══ END INSTRUCTION ═══\n\n\
             ═══ WORKER OUTPUT (cycle {cycle}/{max}) ═══\n\
             {output}\n\
             ═══ END WORKER OUTPUT ═══\n\n\
             Review the worker's output thoroughly. Check:\n\
             1. Did the worker accomplish the goal?\n\
             2. Were all required tools used?\n\
             3. Were all constraints respected?\n\
             4. Is the code correct, idiomatic, and complete?\n\
             5. Are there any issues, omissions, or violations?\n\n\
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
        let max = self.config.orchestration.max_review_cycles;
        let constraints = self.config.constraints.render_prompt_section();
        let constraint_reminder = if constraints.is_empty() {
            String::new()
        } else {
            format!("\n\nReminder of constraints you must follow:\n{constraints}")
        };

        format!(
            "REVISION REQUEST (cycle {cycle}/{max}):\n\n\
             The reviewer found issues with your previous output. \
             You must address ALL of the following:\n\n\
             {feedback}\n\n\
             Fix every issue listed above. When done, clearly summarize \
             what you changed. Do NOT ask follow-up questions.{constraint_reminder}",
        )
    }

    // -----------------------------------------------------------------------
    // Parsing the reviewer's structured response
    // -----------------------------------------------------------------------

    /// Parse the reviewer LLM's response into a structured verdict.
    ///
    /// Expected format:
    /// ```text
    /// VERDICT: APPROVED|NEEDS_REVISION|REJECTED
    ///
    /// ASSESSMENT:
    /// <reasoning>
    ///
    /// FEEDBACK: (optional, for NEEDS_REVISION)
    /// <actionable feedback>
    ///
    /// REASON: (optional, for REJECTED)
    /// <why>
    /// ```
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
            // Fallback heuristic: if the reviewer didn't follow the format,
            // look for keywords.
            if upper.contains("APPROVED") && !upper.contains("NOT APPROVED") {
                "APPROVED"
            } else if upper.contains("REJECT") {
                "REJECTED"
            } else {
                // Default to needing revision if unclear.
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
                // NEEDS_REVISION
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

        // Take everything until the next section header or end of text.
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
    // Main orchestration loop
    // -----------------------------------------------------------------------

    /// Run the full orchestration loop with two LLM agents.
    ///
    /// # Arguments
    /// - `reviewer_conn` — ACP connection to the reviewer opencode instance
    /// - `worker_conn` — ACP connection to the worker opencode instance
    /// - `reviewer_proc` / `worker_proc` — process handles for killing
    /// - `worker_event_rx` — events from the worker's ACP session
    /// - `reviewer_event_rx` — events from the reviewer's ACP session
    /// - `abort_rx` — emergency stop signal
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

        // ── Phase 1: Planning — reviewer formulates the work instruction ──

        self.set_phase(Phase::Planning);
        self.log(LogLevel::Info, "Asking reviewer to formulate work instruction...");

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

        // ── Phase 2: Work-review loop ─────────────────────────────────────

        let max_cycles = self.config.orchestration.max_review_cycles;
        let mut current_instruction = work_instruction;

        for cycle in 1..=max_cycles {
            // Check for abort.
            if abort_rx.try_recv().is_ok() {
                return self.abort(reviewer_conn, worker_conn, reviewer_proc, worker_proc).await;
            }

            // ── Send instruction to worker ────────────────────────────────

            self.log(
                LogLevel::Info,
                format!("Cycle {cycle}/{max_cycles}: dispatching to worker..."),
            );
            self.set_phase(if cycle == 1 {
                Phase::Working
            } else {
                Phase::Revising
            });

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

            // ── Send output to reviewer for assessment ────────────────────

            self.set_phase(Phase::Reviewing);

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
                format!("Reviewer assessment received ({} chars).", reviewer_response.len()),
            );

            // ── Parse verdict ─────────────────────────────────────────────

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
                    let _ = self
                        .event_tx
                        .send(OrchestratorEvent::Finished(Phase::Approved));
                    return Ok(Phase::Approved);
                }
                ReviewVerdict::NeedsRevision { ref feedback } => {
                    if cycle == max_cycles {
                        let msg = format!(
                            "Reached maximum review cycles ({max_cycles}) without approval."
                        );
                        self.log(LogLevel::Warn, &msg);
                        self.set_phase(Phase::Failed(msg.clone()));
                        let _ = self
                            .event_tx
                            .send(OrchestratorEvent::Finished(Phase::Failed(msg)));
                        return Ok(self.phase.clone());
                    }
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

        Ok(self.phase.clone())
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
