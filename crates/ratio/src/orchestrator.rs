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

use crate::config::{Config, StakeholderPhase};
use crate::protocol::{AgentEvent, StopReason, WorkerConnection};
use crate::session::SessionState;
use crate::subprocess::AgentProcess;

/// A live stakeholder — an ACP agent subprocess with its own persona.
pub struct LiveStakeholder {
    /// Index into `config.stakeholders`.
    pub index: usize,
    /// The display name (e.g. "Reverse Engineer").
    pub name: String,
    /// ACP connection to this stakeholder's opencode process.
    pub conn: WorkerConnection,
    /// The opencode child process.
    pub proc: AgentProcess,
}

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
    /// An agent-level event from a stakeholder (index + name).
    StakeholderEvent(usize, String, AgentEvent),
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

    /// Send a prompt to an agent with the stall watchdog enabled.
    ///
    /// If `stall_timeout_secs` is 0, falls through to a plain `prompt()`.
    /// Otherwise uses `prompt_with_nudge()` which will cancel + re-prompt
    /// if the agent goes silent for too long.
    async fn prompt_agent(
        &self,
        conn: &WorkerConnection,
        text: &str,
        role: &str,
    ) -> anyhow::Result<(StopReason, String)> {
        let timeout_secs = self.config.orchestration.stall_timeout_secs;
        if timeout_secs == 0 {
            return conn.prompt(text).await;
        }

        let timeout = std::time::Duration::from_secs(timeout_secs);
        let max_nudges = self.config.orchestration.max_nudges;
        let event_tx = self.event_tx.clone();
        let role_owned = role.to_string();

        conn.prompt_with_nudge(text, timeout, max_nudges, move |attempt| {
            let _ = event_tx.send(OrchestratorEvent::Log(
                LogLevel::Warn,
                format!(
                    "{role_owned} stalled ({timeout_secs}s no activity) — \
                     sending nudge ({attempt}/{max_nudges})",
                ),
            ));
        })
        .await
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
             RESEARCH FOLDER:\n\
             When you perform any codebase exploration, analysis, or subagent research, \
             you MUST write the results into markdown files under `.ratio/research/` in \
             the working directory. This is CRITICAL — research that is not persisted \
             will be lost and must be redone from scratch.\n\n\
             Rules:\n\
             - Create `.ratio/research/` if it does not exist.\n\
             - Use descriptive filenames: e.g. `architecture-overview.md`, \
               `call-graph-analysis.md`, `ssa-propagation-bug.md`.\n\
             - Each file should be self-contained: include the question asked, \
               the findings, relevant code snippets, and conclusions.\n\
             - When handing off work to the other agent (reviewer → worker, or \
               worker → reviewer via summary), explicitly reference which research \
               files are relevant: e.g. \"See `.ratio/research/call-graph-analysis.md` \
               for the full analysis of the call target issue.\"\n\
             - Before starting new research, CHECK if a relevant file already exists \
               in `.ratio/research/`. Read it first — do not redo work that has \
               already been done.\n\
             - Update existing research files if your findings extend or correct them.\n\n\
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
             3. Keep reading and analyzing until you are ABSOLUTELY CERTAIN \
                that the task is irrefutably understood. Leave NO open questions.\n\
             4. Once you are confident, write a comprehensive `agents.md` file with your \
                analysis and create an initial todo list with the TodoWrite tool.\n\
             5. THEN produce a DETAILED, PRECISE work instruction for the worker agent.\n\n\
             ═══ CRITICAL: YOUR ROLE BOUNDARIES ═══\n\
             You are the REVIEWER, not the worker. During this planning phase:\n\
             - You MAY read files to understand the codebase\n\
             - You MAY write agents.md and use TodoWrite\n\
             - You MAY write research files to `.ratio/research/` (and you MUST do so \
               for any significant analysis — see RESEARCH FOLDER above)\n\
             - You MUST NOT edit or create source code files\n\
             - You MUST NOT run builds, tests, or any shell commands that modify state\n\
             - You MUST NOT implement any part of the solution yourself\n\
             - Your ONLY deliverable is a work instruction for the worker agent\n\
             If you catch yourself writing code or running builds, STOP — that is \
             the worker's job, not yours.\n\
             ═══ END ROLE BOUNDARIES ═══\n\n\
             The worker agent has access to the filesystem, can run commands, and can \
             edit files. Be specific about:\n\
             - What files to examine, modify, or create\n\
             - What tools/commands the worker must run\n\
             - All constraints that must be followed\n\
             - Expected outcomes and acceptance criteria\n\
             - Which `.ratio/research/*.md` files contain relevant analysis\n\n\
             Tell the worker:\n\
             - It MUST read the research files you reference before starting work\n\
             - It MUST use TodoWrite to keep the shared todo list updated\n\
             - It MUST keep agents.md updated with implementation notes\n\
             - It MUST write its own research/analysis to `.ratio/research/` files\n\
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

        // Build the custom rules block. These are the user's explicit quality
        // standards and must be evaluated as hard requirements for approval.
        let custom_rules_block = if self.config.constraints.custom_rules.is_empty() {
            String::new()
        } else {
            let rules = self
                .config
                .constraints
                .custom_rules
                .iter()
                .enumerate()
                .map(|(i, r)| format!("  {}. {r}", i + 1))
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "\n═══ MANDATORY QUALITY RULES ═══\n\
                 The user has defined the following rules. ALL of them are \
                 hard requirements. You MUST NOT approve unless EVERY rule is \
                 satisfied. If ANY rule is violated, the verdict MUST be \
                 NEEDS_REVISION.\n\n\
                 {rules}\n\
                 ═══ END MANDATORY QUALITY RULES ═══\n"
            )
        };

        format!(
            "{system_prefix}\n\n\
             ═══ ORIGINAL GOAL ═══\n\
             {goal}\n\
             ═══ END GOAL ═══\n\n\
             {constraints}\n\n\
             {shared}\n\n\
             {custom_rules_block}\n\n\
             ═══ INSTRUCTION SENT TO WORKER ═══\n\
             {instruction}\n\
             ═══ END INSTRUCTION ═══\n\n\
             ═══ WORKER OUTPUT (cycle {cycle}) ═══\n\
             {output}\n\
             ═══ END WORKER OUTPUT ═══\n\n\
             ═══ CRITICAL: YOUR ROLE BOUNDARIES ═══\n\
             You are the REVIEWER, not the worker. During this review phase:\n\
             - You MAY read files to verify the worker's changes\n\
             - You MAY run tests or builds to check correctness\n\
             - You MAY update agents.md and use TodoWrite\n\
             - You MAY write research/analysis to `.ratio/research/` files\n\
             - You MUST NOT edit or create source code files\n\
             - You MUST NOT fix issues yourself — report them as NEEDS_REVISION feedback\n\
             - You MUST NOT implement any part of the solution yourself\n\
             If you find a bug, do NOT fix it — tell the worker to fix it.\n\
             When providing NEEDS_REVISION feedback, reference specific \
             `.ratio/research/*.md` files that contain your analysis so the \
             worker does not have to redo the investigation.\n\
             ═══ END ROLE BOUNDARIES ═══\n\n\
             ═══ REVIEW INSTRUCTIONS ═══\n\n\
             IMPORTANT: Do NOT trust the worker's summary above. You MUST \
             independently verify by reading the actual files on disk, running \
             builds, and running tests. The worker's summary is self-reported \
             and may be inaccurate, incomplete, or optimistic.\n\n\
             Review the worker's output INDEPENDENTLY and THOROUGHLY:\n\
             1. Does the actual output on disk meet the goal? (READ the files yourself)\n\
             2. Were all required tools used?\n\
             3. Were all constraints respected?\n\
             4. Is the code correct, idiomatic, and complete?\n\
             5. Are there any issues, omissions, or violations?\n\
             6. Were ALL mandatory quality rules (above) satisfied?\n\
             7. Is the todo list accurately reflecting the current state?\n\
             8. Is agents.md up to date with implementation notes?\n\n\
             ═══ VERDICT RULES ═══\n\n\
             Your default verdict should be NEEDS_REVISION. You should ONLY issue \
             APPROVED if ALL of the following are true:\n\
             - The goal is FULLY accomplished (not partially, not \"mostly\")\n\
             - ALL required tools were used\n\
             - ALL constraints were respected\n\
             - ALL mandatory quality rules are satisfied\n\
             - There are ZERO remaining issues, bugs, or violations\n\
             - You have independently verified the output (not just trusted the worker)\n\n\
             If you have ANY doubt, ANY unverified claim, or ANY remaining issue, \
             the verdict MUST be NEEDS_REVISION. Partial progress is NOT approval. \
             \"Good enough\" is NOT approval. Only COMPLETE, VERIFIED success is approval.\n\n\
             ═══ END VERDICT RULES ═══\n\n\
             Update the todo list with your findings using TodoWrite.\n\
             Update agents.md with your review notes.\n\n\
             You MUST end your response with a verdict block in EXACTLY this format \
             (on its own line, no other text on the verdict line):\n\n\
             VERDICT: NEEDS_REVISION\n\n\
             ASSESSMENT:\n\
             <your detailed reasoning>\n\n\
             FEEDBACK:\n\
             <specific, actionable instructions for the worker to fix the issues>\n\n\
             --- or, ONLY if everything is perfect ---\n\n\
             VERDICT: APPROVED\n\n\
             ASSESSMENT:\n\
             <your detailed reasoning for why everything passes>\n\n\
             --- or, if fundamentally broken ---\n\n\
             VERDICT: REJECTED\n\n\
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
             - Read any `.ratio/research/*.md` files referenced in the feedback above — \
               the reviewer's analysis is already there, do NOT redo that research\n\
             - Update the todo list (TodoWrite) to reflect what needs to be fixed\n\
             - Update agents.md with notes about the revision\n\
             - Write your own analysis/findings to `.ratio/research/` if you discover \
               anything new during the revision\n\
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
    ///
    /// IMPORTANT: This parser is intentionally biased toward NEEDS_REVISION.
    /// An APPROVED verdict requires an unambiguous, standalone "VERDICT: APPROVED"
    /// line. Any ambiguity defaults to NEEDS_REVISION. This prevents premature
    /// approval when the reviewer mentions "APPROVED" in passing, echoes the
    /// format instructions, or produces ambiguous output.
    fn parse_reviewer_response(&self, response: &str) -> ReviewVerdict {
        // Strategy: find lines that look like "VERDICT: <word>" and extract
        // the LAST such line (if the reviewer echoed the format instructions
        // first and then wrote their actual verdict, we want the final one).
        //
        // We skip lines that contain the format template
        // "APPROVED|NEEDS_REVISION|REJECTED" since those are the reviewer
        // echoing the instructions, not stating a verdict.

        let mut found_verdict: Option<&str> = None;

        for line in response.lines() {
            let trimmed = line.trim();
            let upper = trimmed.to_uppercase();

            // Skip format template echoes like "VERDICT: APPROVED|NEEDS_REVISION|REJECTED"
            if upper.contains("APPROVED|") || upper.contains("|NEEDS_REVISION|") {
                continue;
            }

            // Match lines that start with "VERDICT:" (possibly with markdown bold/etc.)
            // Strip common markdown decorations: **, ##, >, etc.
            let stripped = upper
                .trim_start_matches(['*', '#', '>', ' ']);

            if let Some(after) = stripped.strip_prefix("VERDICT:") {
                let after = after.trim();
                if after.starts_with("APPROVED") {
                    found_verdict = Some("APPROVED");
                } else if after.starts_with("NEEDS_REVISION")
                    || after.starts_with("NEEDS REVISION")
                {
                    found_verdict = Some("NEEDS_REVISION");
                } else if after.starts_with("REJECTED") {
                    found_verdict = Some("REJECTED");
                }
            }
        }

        // If no structured VERDICT line was found, use a conservative fallback.
        // CRITICAL: The fallback NEVER produces APPROVED. If the reviewer
        // didn't write a clear "VERDICT: APPROVED" line, they haven't approved.
        let verdict_str = found_verdict.unwrap_or_else(|| {
            let upper = response.to_uppercase();
            if upper.contains("REJECT") {
                "REJECTED"
            } else {
                // Default: if we can't tell, assume revision is needed.
                // The reviewer must explicitly and unambiguously approve.
                "NEEDS_REVISION"
            }
        });

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
        stakeholders: &mut [LiveStakeholder],
        stakeholder_event_rxs: Vec<mpsc::UnboundedReceiver<AgentEvent>>,
        mut worker_event_rx: mpsc::UnboundedReceiver<AgentEvent>,
        mut reviewer_event_rx: mpsc::UnboundedReceiver<AgentEvent>,
        mut abort_rx: mpsc::UnboundedReceiver<()>,
    ) -> anyhow::Result<Phase> {
        // Report stakeholder count to the UI.
        if stakeholders.is_empty() {
            self.log(LogLevel::Info, "No stakeholders configured.");
        } else {
            let names: Vec<&str> = stakeholders.iter().map(|s| s.name.as_str()).collect();
            self.log(
                LogLevel::Info,
                format!(
                    "{} stakeholder(s) active: {}",
                    stakeholders.len(),
                    names.join(", "),
                ),
            );
        }

        // Ensure the shared research directory exists.
        let research_dir = self.config.cwd.join(".ratio").join("research");
        if let Err(e) = std::fs::create_dir_all(&research_dir) {
            self.log(
                LogLevel::Warn,
                format!("Failed to create research dir {}: {e}", research_dir.display()),
            );
        }

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

        // Forward stakeholder events.
        for (i, mut rx) in stakeholder_event_rxs.into_iter().enumerate() {
            let tx = self.event_tx.clone();
            let name = stakeholders.get(i).map(|s| s.name.clone()).unwrap_or_default();
            tokio::task::spawn_local(async move {
                while let Some(evt) = rx.recv().await {
                    let _ = tx.send(OrchestratorEvent::StakeholderEvent(i, name.clone(), evt));
                }
            });
        }

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
            result = self.prompt_agent(reviewer_conn, &planning_prompt, "Reviewer") => result,
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

        // ── Stakeholder consultation (planning) ─────────────────────────
        let stakeholder_input = self
            .consult_stakeholders(
                stakeholders,
                StakeholderPhase::Planning,
                &format!(
                    "GOAL: {}\n\nDRAFT WORK INSTRUCTION:\n{}",
                    self.config.goal, work_instruction
                ),
            )
            .await;

        // If stakeholders provided input, send it back to the reviewer
        // to produce a refined work instruction.
        let work_instruction = if stakeholder_input.is_empty() {
            work_instruction
        } else {
            self.log(
                LogLevel::Info,
                "Stakeholders provided input — reviewer is synthesizing final plan...",
            );

            let synthesis_prompt = format!(
                "The following stakeholders have reviewed your draft work instruction \
                 and provided input from their perspectives. You must now produce the \
                 FINAL work instruction that incorporates their feedback where \
                 appropriate.\n\n\
                 Your original draft:\n{work_instruction}\n\n\
                 Stakeholder input:\n{stakeholder_input}\n\n\
                 Produce the final, refined work instruction now. It should be \
                 complete and self-contained — the worker will only see this final \
                 version, not the stakeholder input directly. Reference any \
                 `.ratio/research/*.md` files the stakeholders created."
            );

            let synth_result = tokio::select! {
                result = self.prompt_agent(reviewer_conn, &synthesis_prompt, "Reviewer") => result,
                _ = abort_rx.recv() => {
                    return self.abort(reviewer_conn, worker_conn, reviewer_proc, worker_proc).await;
                }
            };

            match synth_result {
                Ok((_, refined)) if !refined.trim().is_empty() => refined,
                Ok(_) => {
                    self.log(LogLevel::Warn, "Reviewer synthesis was empty, using original plan.");
                    work_instruction
                }
                Err(e) => {
                    self.log(LogLevel::Warn, format!("Reviewer synthesis failed ({e}), using original plan."));
                    work_instruction
                }
            }
        };

        self.log(
            LogLevel::Info,
            format!(
                "Work instruction finalized ({} chars). Sending to worker.",
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
                result = self.prompt_agent(worker_conn, &current_instruction, "Worker") => result,
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
                result = self.prompt_agent(reviewer_conn, &review_prompt, "Reviewer") => result,
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

            // ── Stakeholder consultation (review) ────────────────────────
            let stakeholder_review_input = self
                .consult_stakeholders(
                    stakeholders,
                    StakeholderPhase::Review,
                    &format!(
                        "GOAL: {goal}\n\n\
                         WORKER OUTPUT SUMMARY (cycle {cycle}):\n{worker_output}\n\n\
                         REVIEWER'S DRAFT ASSESSMENT:\n{reviewer_response}",
                        goal = self.config.goal,
                    ),
                )
                .await;

            // If stakeholders provided input, let the reviewer re-evaluate.
            let reviewer_response = if stakeholder_review_input.is_empty() {
                reviewer_response
            } else {
                self.log(
                    LogLevel::Info,
                    "Stakeholders provided review input — reviewer is synthesizing final verdict...",
                );

                let synth_prompt = format!(
                    "Stakeholders have reviewed the worker's output and your draft \
                     assessment. Consider their input and produce your FINAL verdict.\n\n\
                     Your draft assessment:\n{reviewer_response}\n\n\
                     Stakeholder input:\n{stakeholder_review_input}\n\n\
                     You MUST respond with the same VERDICT format as before. \
                     Write your verdict on its own line (e.g. \"VERDICT: NEEDS_REVISION\").\n\n\
                     IMPORTANT: If ANY stakeholder raised a valid concern that has \
                     not been addressed, you MUST issue NEEDS_REVISION. Stakeholder \
                     concerns are not optional feedback — they are requirements.\n\n\
                     ASSESSMENT:\n<your final reasoning, incorporating stakeholder feedback>\n\n\
                     If NEEDS_REVISION, add:\n\
                     FEEDBACK:\n<specific instructions, referencing stakeholder concerns and \
                     their `.ratio/research/*.md` files where relevant>"
                );

                match self.prompt_agent(reviewer_conn, &synth_prompt, "Reviewer").await {
                    Ok((_, refined)) if !refined.trim().is_empty() => refined,
                    Ok(_) => {
                        self.log(LogLevel::Warn, "Reviewer review synthesis was empty, using original.");
                        reviewer_response
                    }
                    Err(e) => {
                        self.log(LogLevel::Warn, format!("Reviewer review synthesis failed ({e}), using original."));
                        reviewer_response
                    }
                }
            };

            // ── Parse verdict ────────────────────────────────────────────

            let verdict = self.parse_reviewer_response(&reviewer_response);

            // Log the parsed verdict for debuggability.
            match &verdict {
                ReviewVerdict::Approved { summary } => {
                    self.log(
                        LogLevel::Info,
                        format!(
                            "Parsed verdict: APPROVED (summary: {} chars)",
                            summary.len()
                        ),
                    );
                }
                ReviewVerdict::NeedsRevision { feedback } => {
                    self.log(
                        LogLevel::Info,
                        format!(
                            "Parsed verdict: NEEDS_REVISION (feedback: {} chars)",
                            feedback.len()
                        ),
                    );
                }
                ReviewVerdict::Rejected { reason } => {
                    self.log(
                        LogLevel::Info,
                        format!("Parsed verdict: REJECTED (reason: {} chars)", reason.len()),
                    );
                }
            }

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

    /// Consult all stakeholders that participate in the given phase.
    ///
    /// Each stakeholder receives a prompt with their persona, the current
    /// context, and an ask for their perspective. Returns a consolidated
    /// block of all stakeholder input, or an empty string if there are
    /// no stakeholders for this phase.
    async fn consult_stakeholders(
        &self,
        stakeholders: &[LiveStakeholder],
        phase: StakeholderPhase,
        context: &str,
    ) -> String {
        let relevant: Vec<&LiveStakeholder> = stakeholders
            .iter()
            .filter(|s| {
                self.config.stakeholders[s.index]
                    .phases
                    .contains(&phase)
            })
            .collect();

        if relevant.is_empty() {
            return String::new();
        }

        let phase_label = match phase {
            StakeholderPhase::Planning => "planning",
            StakeholderPhase::Review => "review",
        };

        self.log(
            LogLevel::Info,
            format!(
                "Consulting {} stakeholder(s) for {phase_label} phase...",
                relevant.len()
            ),
        );

        let mut all_input = String::new();

        for stakeholder in relevant {
            let persona = &self.config.stakeholders[stakeholder.index].persona;
            let name = &stakeholder.name;

            let prompt = format!(
                "{persona}\n\n\
                 You are participating as a stakeholder named \"{name}\" in a \
                 software development process. Your role is to provide input from \
                 your unique perspective — you are NOT the implementer.\n\n\
                 ═══ CURRENT CONTEXT ═══\n\
                 {context}\n\
                 ═══ END CONTEXT ═══\n\n\
                 ═══ YOUR TASK ═══\n\
                 Based on your expertise and perspective as {name}:\n\
                 1. What are your requirements, concerns, or suggestions?\n\
                 2. What would you want to see prioritized or changed?\n\
                 3. Are there risks or opportunities the team might be missing?\n\n\
                 Be concise and direct. Focus on what matters from YOUR perspective. \
                 Write your findings to `.ratio/research/{stakeholder_file}.md` so \
                 the team can reference them.\n\
                 ═══ END TASK ═══",
                stakeholder_file = name.to_lowercase().replace(' ', "-"),
            );

            self.log(
                LogLevel::Info,
                format!("Stakeholder \"{name}\" is providing input..."),
            );

            match self.prompt_agent(&stakeholder.conn, &prompt, name).await {
                Ok((_, response)) => {
                    if !response.trim().is_empty() {
                        all_input.push_str(&format!(
                            "\n═══ STAKEHOLDER INPUT: {name} ═══\n\
                             {response}\n\
                             ═══ END {name} ═══\n"
                        ));
                        self.log(
                            LogLevel::Info,
                            format!("Stakeholder \"{name}\" provided {} chars of input.", response.len()),
                        );
                    }
                }
                Err(e) => {
                    self.log(
                        LogLevel::Warn,
                        format!("Stakeholder \"{name}\" failed: {e}"),
                    );
                }
            }
        }

        all_input
    }

    /// Emergency-stop all agents (reviewer, worker, stakeholders).
    async fn abort(
        &mut self,
        reviewer_conn: &WorkerConnection,
        worker_conn: &WorkerConnection,
        reviewer_proc: &mut AgentProcess,
        worker_proc: &mut AgentProcess,
    ) -> anyhow::Result<Phase> {
        self.log(LogLevel::Warn, "Abort signal received — killing all agents.");
        reviewer_conn.cancel().await.ok();
        worker_conn.cancel().await.ok();
        reviewer_proc.kill();
        worker_proc.kill();
        // Note: stakeholder processes are killed by their Drop impls or
        // when the parent process exits. The orchestrator doesn't hold
        // mutable refs to them during abort since it only borrows them
        // in the main flow.
        self.set_phase(Phase::Aborted);
        let _ = self
            .event_tx
            .send(OrchestratorEvent::Finished(Phase::Aborted));
        Ok(Phase::Aborted)
    }
}
