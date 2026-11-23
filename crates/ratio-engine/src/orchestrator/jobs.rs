use std::{collections::BTreeSet, time::Instant};

use futures_util::{FutureExt, future::LocalBoxFuture};
use tokio::sync::RwLock;

use super::*;
use crate::{
    api::ToolCall,
    jobs::{self, Delegate, Job, Jobs, Permissions, Profile, Specialist, StartTool, State, Work},
    tools::mcp,
};

#[derive(Clone, Copy)]
pub(super) struct JobScope<'a> {
    pub book: &'a Jobs,
    pub workers: &'a [Worker],
    pub goal: &'a str,
}

#[derive(Clone, Copy)]
pub(super) struct JobPeer<'a> {
    pub scope: JobScope<'a>,
    pub parent: Option<&'a Job>,
    pub shared_candidate: bool,
}

impl JobPeer<'_> {
    pub fn permits(&self, tool: &str) -> bool {
        self.profile().is_none_or(|profile| profile.allows(tool))
            && self
                .parent
                .is_none_or(|parent| parent.permissions.permits(tool))
    }

    pub fn profile(&self) -> Option<Profile> {
        match &self.parent?.work {
            Work::Specialist(specialist) => Some(specialist.profile),
            Work::Tool(_) => None,
        }
    }
}

pub(super) struct JobScheduler<'a> {
    orchestrator: &'a Orchestrator<'a>,
    scope: Option<JobScope<'a>>,
    memory: &'a SharedMemory,
    candidate: Option<&'a RwLock<()>>,
    tools: FuturesUnordered<LocalBoxFuture<'a, (u64, Option<ToolOutcome>)>>,
    specialists: FuturesUnordered<LocalBoxFuture<'a, (u64, Specialist, Result<Step>)>>,
    running: BTreeSet<u64>,
    last_specialist: u64,
    prefer_specialist: bool,
}

struct ToolOutcome {
    state: State,
    output: String,
    full: String,
    elapsed: u64,
}

impl<'a> JobScheduler<'a> {
    pub fn new(
        orchestrator: &'a Orchestrator<'a>,
        scope: Option<JobScope<'a>>,
        memory: &'a SharedMemory,
        candidate: Option<&'a RwLock<()>>,
    ) -> Self {
        Self {
            orchestrator,
            scope,
            memory,
            candidate,
            tools: FuturesUnordered::new(),
            specialists: FuturesUnordered::new(),
            running: BTreeSet::new(),
            last_specialist: 0,
            prefer_specialist: false,
        }
    }

    pub fn is_idle(&self) -> bool {
        self.tools.is_empty() && self.specialists.is_empty()
    }

    pub fn specialist_first(&mut self) -> bool {
        self.prefer_specialist = !self.prefer_specialist;
        self.prefer_specialist
    }

    pub fn agent_slots(&self) -> usize {
        self.specialists.len()
    }

    pub fn start_tools(&mut self, width: usize) -> bool {
        let Some(scope) = self.scope else {
            return false;
        };
        let orchestrator = self.orchestrator;
        let mut changed = false;
        while self.tools.len() < width {
            let Some(job) = scope.book.next_tool() else {
                break;
            };
            scope
                .book
                .mark_running(job.id, |text| orchestrator.api.redact(text));
            let candidate = self.candidate;
            self.tools.push(
                async move {
                    let candidate = candidate.filter(|_| job.shared_candidate);
                    let _workspace = match candidate {
                        Some(candidate) => Some(candidate.read().await),
                        None => None,
                    };
                    (job.id, orchestrator.run_background_tool(&job).await)
                }
                .boxed_local(),
            );
            changed = true;
        }
        changed
    }

    pub fn start_specialists(&mut self, slots: usize) -> Result<bool> {
        let Some(scope) = self.scope else {
            return Ok(false);
        };
        if slots == 0 {
            return Ok(false);
        }
        let orchestrator = self.orchestrator;
        let mut changed = false;
        let ready = scope.book.ready_specialists(&self.running);
        let pivot = ready.partition_point(|&id| id <= self.last_specialist);
        let mut started = 0;
        for &id in ready[pivot..].iter().chain(&ready[..pivot]) {
            if started == slots {
                break;
            }
            let job = scope.book.specialist(id);
            let Work::Specialist(specialist) = &job.work else {
                unreachable!()
            };
            let mut specialist = specialist.as_ref().clone();
            let actor = job.key();
            if (specialist.dialogue.pending()?.is_none() || specialist.failed)
                && scope
                    .book
                    .deliver(&actor, &mut specialist.dialogue, orchestrator.job_limit())
            {
                if !specialist.failed {
                    reopen_specialist(&mut specialist, orchestrator.job_limit());
                }
                scope.book.save_specialist(job.id, specialist.clone());
                changed = true;
            }
            if specialist.failed {
                if !scope.book.has_outstanding(&actor) && !scope.book.has_receipts(&actor) {
                    let report = specialist
                        .held_report
                        .as_deref()
                        .unwrap_or("Specialist failed.");
                    scope.book.finish(
                        job.id,
                        State::Failed,
                        (report, report),
                        None,
                        orchestrator.job_limit(),
                        |text| orchestrator.api.redact(text),
                    );
                    changed = true;
                }
                continue;
            }
            if specialist.held_report.is_some()
                || scope.book.blocked(&actor, &specialist.dialogue)?
            {
                continue;
            }
            scope
                .book
                .mark_running(job.id, |text| orchestrator.api.redact(text));
            self.running.insert(job.id);
            self.last_specialist = job.id;
            let memory = self.memory;
            let candidate = self.candidate;
            self.specialists.push(
                async move {
                    let candidate = candidate.filter(|_| {
                        job.shared_candidate
                            && specialist.dialogue.pending().ok().flatten().is_some()
                    });
                    let _workspace = match candidate {
                        Some(candidate) => Some(candidate.read().await),
                        None => None,
                    };
                    let result = orchestrator
                        .step_specialist(&job, scope, memory, &mut specialist)
                        .await;
                    (job.id, specialist, result)
                }
                .boxed_local(),
            );
            started += 1;
            changed = true;
        }
        Ok(changed)
    }

    pub async fn settle_next(&mut self) -> Result<()> {
        let scope = self.scope.expect("active jobs");
        let orchestrator = self.orchestrator;
        let limit = orchestrator.job_limit();
        let redact = |text: &str| orchestrator.api.redact(text);
        tokio::select! {
            Some((id, outcome)) = self.tools.next(), if !self.tools.is_empty() => {
                if let Some(outcome) = outcome {
                    scope.book.finish(id, outcome.state, (&outcome.output, &outcome.full),
                        Some(outcome.elapsed), limit, redact);
                } else {
                    scope.book.defer(id);
                }
            }
            Some((id, mut specialist, outcome)) = self.specialists.next(), if !self.specialists.is_empty() => {
                self.running.remove(&id);
                let actor = format!("job-{id}");
                match outcome {
                    Ok(Step::Continue) => {}
                    Ok(Step::Finished(report)) if scope.book.has_outstanding(&actor) || scope.book.has_receipts(&actor) => {
                        specialist.held_report = Some(clip(&report, limit));
                    }
                    Ok(Step::Finished(report)) => {
                        scope.book.finish(id, State::Completed, (&report, &report), None, limit, redact);
                    }
                    Ok(Step::Plan(_) | Step::Verdict(_, _) | Step::Repair(_)) => {
                        unreachable!("specialist has no decision tool")
                    }
                    Err(error) => {
                        let report = format!("Specialist unavailable: {error:#}");
                        specialist.dialogue.audit_note(&actor, "error", &redact(&report));
                        specialist.failed = true;
                        specialist.held_report = Some(report);
                        if !scope.book.has_outstanding(&actor) && !scope.book.has_receipts(&actor) {
                            let report = specialist.held_report.as_deref().expect("failure report");
                            scope.book.finish(id, State::Failed, (report, report), None, limit, redact);
                        }
                    }
                }
                scope.book.save_specialist(id, specialist);
            }
        }
        Ok(())
    }
}

fn reopen_specialist(specialist: &mut Specialist, limit: usize) {
    specialist.dialogue.job_report = specialist.held_report.take();
    specialist.dialogue.reopen_job_report(limit);
}

impl Orchestrator<'_> {
    pub(super) fn job_limit(&self) -> usize {
        ipc::inbox_limit(self.config.context_window)
    }

    pub(super) fn job_call(
        &self,
        call: &ToolCall,
        dialogue: &Dialogue,
        peer: PeerTools<'_>,
        access: Access,
    ) -> Result<String> {
        let context = peer
            .jobs
            .filter(|_| !peer.repairing)
            .context("jobs are unavailable in this turn")?;
        ensure!(call.kind == "function", "expected a function tool call");
        let call = &jobs::expand(call)?;
        let book = context.scope.book;
        match call.function.name.as_str() {
            "list_specialists" => {
                let input: serde_json::Map<String, Value> =
                    crate::api::tool_arguments(&call.function.arguments)?;
                ensure!(input.is_empty(), "list_specialists takes no arguments");
                let roles = jobs::roles(context.scope.workers);
                let result = roles
                    .iter()
                    .map(|role| json!({"id":role.id,"name":role.name,"tools":role.profile,"description":if role.id.starts_with("worker_") {
                        "Consult using the configured worker persona.".into()
                    } else {
                        clip(&role.instructions, 240)
                    }}))
                    .collect::<Vec<_>>();
                return Ok(serde_json::to_string(&result)?);
            }
            "await_jobs" | "job_status" => return book.reply(call, peer.author),
            _ => {}
        }
        let policy_actor = peer.policy_actor();
        let mut job = Job {
            id: 0,
            owner: peer.author.into(),
            policy_actor: policy_actor.into(),
            mcp_actor: peer.mcp_actor().into(),
            workspace: peer.workspace.into(),
            access,
            permissions: context
                .parent
                .map_or_else(Permissions::all, |parent| parent.permissions),
            shared_candidate: context.shared_candidate,
            ancestry: context
                .parent
                .map_or_else(BTreeSet::new, |job| job.ancestry.clone()),
            origin: dialogue.current_audit_turn(),
            work: Work::Tool(call.clone()),
            state: State::Queued,
            result: None,
            delivered: false,
        };
        if call.function.name == "delegate" {
            let input: Delegate = crate::api::tool_arguments(&call.function.arguments)?;
            ensure!(
                !input.task.trim().is_empty() && input.task.len() <= self.job_limit(),
                "provide a nonempty task within {} UTF-8 bytes",
                self.job_limit()
            );
            let role = jobs::roles(context.scope.workers)
                .into_iter()
                .find(|role| role.id == input.role)
                .context("unknown specialist role; use list_specialists")?;
            ensure!(
                job.ancestry.insert(role.id.clone()),
                "specialist role already occurs in this job ancestry"
            );
            let mut sources = Vec::new();
            let mut evidence = String::new();
            let mut seen = BTreeSet::new();
            let share = self.job_limit().min(4 * 1024);
            let per_source = share / input.evidence.len().max(1);
            for reference in input.evidence {
                let mut source = dialogue
                    .evidence_for(&reference, peer.author, per_source)
                    .with_context(|| format!("unavailable evidence reference {reference:?}"))?;
                if !seen.insert((source.actor.clone(), source.call_id.clone())) {
                    continue;
                }
                source.id = format!("source_{}", sources.len() + 1);
                source.excerpt = self.api.redact(&clip(&source.excerpt, per_source));
                let header = format!(
                    "{} from {} ({} {}, SHA-256 {})",
                    source.id, source.actor, source.tool, source.call_id, source.digest
                );
                ensure!(
                    header.len() + evidence.len() + 2 <= share,
                    "evidence references exceed the context allowance"
                );
                evidence.push_str(&header);
                evidence.push('\n');
                let remaining = share.saturating_sub(evidence.len() + 1);
                evidence.push_str(&clip(&source.excerpt, remaining));
                evidence.push('\n');
                sources.push(source);
            }
            let mut dialogue = Dialogue::start(input.task);
            if !evidence.is_empty() {
                dialogue.peer_context = format!("Delegated source excerpts:\n{evidence}");
                dialogue.job_evidence = sources;
            }
            job.permissions = job.permissions.restricted(role.profile);
            job.mcp_actor = if role.profile == Profile::Operator {
                format!("operator:{}", peer.author)
            } else {
                String::new()
            };
            job.work = Work::Specialist(Box::new(Specialist {
                role: role.id,
                name: role.name,
                model: role.model,
                effort: role.effort,
                instructions: role.instructions,
                profile: role.profile,
                dialogue,
                held_report: None,
                failed: false,
            }));
        } else {
            let input: StartTool = crate::api::tool_arguments(&call.function.arguments)?;
            let call = input.call();
            self.validate_background(&job, &call, false)?;
            job.work = Work::Tool(call);
        }
        let id = book.submit(job, |text| self.api.redact(text));
        Ok(json!({"job":id,"state":"queued"}).to_string())
    }

    fn validate_background(&self, job: &Job, call: &ToolCall, dispatch: bool) -> Result<()> {
        match call.function.name.as_str() {
            "shell" => {
                ensure!(
                    job.permissions.permits("shell")
                        && !self
                            .config
                            .constraints
                            .shell_disabled_for(&job.policy_actor)
                        && (matches!(job.access, Access::Work)
                            || self.config.orchestration.allow_inspection_shell),
                    "shell is disabled for this agent or phase"
                );
                Tools::validate_shell(call)
            }
            "mcp_call" => {
                ensure!(
                    job.permissions.permits("mcp_call"),
                    "MCP is disabled for this specialist"
                );
                if dispatch {
                    mcp::validate_call_policy(&self.config.mcp_servers, job.access, call)
                } else {
                    mcp::validate_call(
                        &self.mcp,
                        &job.mcp_actor,
                        &self.config.mcp_servers,
                        job.access,
                        call,
                        &job.workspace,
                    )
                }
            }
            _ => anyhow::bail!("only shell and mcp_call can run in the background"),
        }
    }

    async fn step_specialist(
        &self,
        job: &Job,
        scope: JobScope<'_>,
        memory: &SharedMemory,
        specialist: &mut Specialist,
    ) -> Result<Step> {
        let actor = job.key();
        let profile = specialist.profile;
        let access = if profile == Profile::Operator {
            job.access
        } else {
            Access::Inspect
        };
        let system = self.system(scope.goal, &format!(
            "{}\nYou are a temporary specialist. Work on the supplied task and reply with a compact result for the requesting agent. Cite concrete source references and report uncertainty.",
            specialist.instructions
        ), &job.workspace);
        self.step(
            &mut specialist.dialogue,
            self.config
                .reviewer_spec()
                .with(specialist.model.as_deref(), specialist.effort.as_deref()),
            access,
            &system,
            &format!("{} / {}", actor, specialist.name),
            PeerTools {
                passive_inspection: false,
                route: None,
                ack: None,
                memory,
                author: &actor,
                final_review: false,
                plan_workers: None,
                campaign_execution: None,
                ownership: None,
                workspace: &job.workspace,
                allow_approval: false,
                require_task_graph: false,
                repairing: false,
                jobs: Some(JobPeer {
                    scope,
                    parent: Some(job),
                    shared_candidate: job.shared_candidate,
                }),
            },
        )
        .await
    }

    async fn run_background_tool(&self, job: &Job) -> Option<ToolOutcome> {
        let Work::Tool(call) = &job.work else {
            unreachable!()
        };
        let mut control = self.reporter.generation_control();
        let generation = control.as_ref().map(|control| *control.borrow());
        if generation.is_some_and(|state| state.paused) {
            return None;
        }
        let started = Instant::now();
        let tools = self.toolset(job.access, &job.workspace, None);
        let (output, full) = match self.validate_background(job, call, true) {
            Err(error) => {
                let text = format!("REFUSED: {error:#}");
                (text.clone(), text)
            }
            Ok(()) if call.function.name == "shell" => {
                if let Some((control, generation)) = control.as_mut().zip(generation) {
                    tokio::select! {
                        biased;
                        _ = control.wait_for(|current| current.epoch != generation.epoch) => {
                            let text = "Tool outcome unknown: background shell was interrupted by operator pause. Verify its effects before retrying.".to_owned();
                            return Some(ToolOutcome { state: State::Unknown, output: text.clone(), full: text, elapsed: started.elapsed().as_millis() as u64 });
                        }
                        result = tools.execute_audited(call) => result,
                    }
                } else {
                    tools.execute_audited(call).await
                }
            }
            Ok(()) => {
                mcp::dispatch_background(
                    &self.mcp,
                    &job.mcp_actor,
                    &self.config.mcp_servers,
                    job.access,
                    call,
                    &job.workspace,
                    tools.timeout,
                    tools.output_bytes,
                    tools.key,
                )
                .await
            }
        };
        Some(ToolOutcome {
            state: tool_outcome(&output, &full),
            output,
            full,
            elapsed: started.elapsed().as_millis() as u64,
        })
    }

    pub(super) async fn step_lead_with_jobs(
        &self,
        session: &mut Session,
        model: crate::config::ModelSpec<'_>,
        access: Access,
        system: &str,
        peer: PeerTools<'_>,
        scheduler: &mut JobScheduler<'_>,
    ) -> Result<Step> {
        let label = session.phase.label();
        let Some(context) = peer.jobs else {
            return self
                .step(&mut session.dialogue, model, access, system, label, peer)
                .await;
        };
        let book = context.scope.book;
        let actor = peer.author;
        let mut active = FuturesUnordered::new();
        let width = self.parallel_limit.get();
        let control = self.reporter.generation_control();
        let paused = || control.as_ref().is_some_and(|state| state.borrow().paused);
        loop {
            if active.is_empty()
                && scheduler.is_idle()
                && let Err(error) = self.api.wait_for_generation().await
            {
                session.dialogue.in_flight = false;
                book.checkpoint_exit();
                self.store.save(session)?;
                return Err(error);
            }
            if !paused() {
                let mut changed = scheduler.start_tools(width);
                if scheduler.specialist_first() {
                    let free = width.saturating_sub(active.len() + scheduler.agent_slots());
                    changed |= scheduler.start_specialists(free.min(1))?;
                }
                if active.is_empty()
                    && scheduler.agent_slots() < width
                    && session.dialogue.pending()?.is_none()
                    && book.deliver(actor, &mut session.dialogue, self.job_limit())
                {
                    session.dialogue.reopen_job_report(self.job_limit());
                    changed = true;
                }
                if active.is_empty()
                    && scheduler.agent_slots() < width
                    && session.dialogue.job_report.is_none()
                    && !book.blocked(actor, &session.dialogue)?
                {
                    let mut dialogue = session.dialogue.clone();
                    active.push(async move {
                        let step = self
                            .step(&mut dialogue, model, access, system, label, peer)
                            .await;
                        (dialogue, step)
                    });
                }
                changed |= scheduler.start_specialists(
                    width.saturating_sub(active.len() + scheduler.agent_slots()),
                )?;
                if changed {
                    self.store.save(session)?;
                }
                ensure!(
                    !active.is_empty() || !scheduler.is_idle() || changed,
                    "job scheduler has no runnable work"
                );
            }
            tokio::select! {
                Some((dialogue, result)) = active.next(), if !active.is_empty() => {
                    session.dialogue = dialogue;
                    return result;
                }
                result = scheduler.settle_next(), if !scheduler.is_idle() => {
                    result?;
                    self.store.save(session)?;
                }
                else => continue,
            }
        }
    }
}

pub(super) fn tool_outcome(output: &str, full: &str) -> State {
    let mcp_error = full
        .strip_prefix("Untrusted MCP tool output\n")
        .and_then(|payload| serde_json::from_str::<Value>(payload).ok())
        .is_some_and(|payload| payload["isError"] == true);
    if output.starts_with(mcp::UNKNOWN_CALL) {
        State::Unknown
    } else if output.starts_with("REFUSED")
        || (output.starts_with("Tool error: tool ") && output.contains("is unavailable"))
    {
        State::Refused
    } else if output.starts_with("Tool error:")
        || full.starts_with("Succeeded: false\n")
        || mcp_error
    {
        State::Failed
    } else {
        State::Completed
    }
}
