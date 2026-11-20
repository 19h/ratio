use serde::Deserialize;
use serde_json::json;

use super::*;
use crate::{
    api::{Message, ToolCall},
    session::WorkerOnboarding,
};
use ratio_protocol::Task;

#[derive(Deserialize)]
struct Extension {
    name: String,
    persona: String,
    task: Task,
    paths: Vec<WritePath>,
    mentor: String,
    stakeholder: Option<String>,
}

impl Orchestrator<'_> {
    pub(super) async fn onboard_worker(
        &self,
        session: &mut Session,
        workspace: &std::path::Path,
    ) -> Result<()> {
        let (id, request) = self
            .store
            .pending_worker(session.run_id())?
            .context("missing queued worker request")?;
        session.active_worker_request = Some(id);
        self.reporter.emit(
            Kind::Phase,
            format!("Cycle {} / {}", session.cycle, session.phase.label()),
            format!("Worker request {id}"),
        );
        let result = self.try_onboard(session, workspace, &request).await;
        let state = if result.is_ok() {
            "admitted"
        } else {
            "declined"
        };
        match result {
            Ok(worker) => {
                session.worker_addition_status =
                    Some(format!("Worker request {id} / admitted {worker}"));
            }
            Err(error)
                if self.budget.exhaustion().is_some()
                    || error.is::<crate::events::CheckpointExit>() =>
            {
                return Err(error);
            }
            Err(error) => {
                session.worker_addition_status =
                    Some(format!("Worker request {id} / declined: {error:#}"));
            }
        }
        self.progress(
            session
                .worker_addition_status
                .as_deref()
                .unwrap_or_default(),
        );
        session.onboarding = None;
        session.panel = None;
        session.phase = Phase::Work;
        session.last_worker_request = Some(id);
        session.active_worker_request = None;
        self.store.save(session)?;
        self.store.journal.borrow().worker_stage(
            id,
            state,
            &self.api.redact(
                session
                    .worker_addition_status
                    .as_deref()
                    .unwrap_or_default(),
            ),
            None,
        )?;
        self.store.finish_worker_request(id)?;
        self.report_roster(session);
        Ok(())
    }

    async fn try_onboard(
        &self,
        session: &mut Session,
        workspace: &std::path::Path,
        request: &str,
    ) -> Result<String> {
        let mut workers = session
            .workers
            .clone()
            .context("missing incumbent roster")?;
        let stakeholders = session.stakeholders.clone().unwrap_or_default();
        let newcomer = format!("worker_{}", workers.len() + 1);
        if session.onboarding.is_none() {
            session.onboarding = Some(
                self.propose_extension(session, workspace, request, &workers, &stakeholders)
                    .await?,
            );
            self.store.save(session)?;
        }
        let candidate = session
            .onboarding
            .clone()
            .context("missing proposed addition")?;
        if let Some(id) = session.active_worker_request {
            let state = if candidate.briefed {
                "awaiting_ack"
            } else {
                "mentoring"
            };
            self.store.journal.borrow().worker_stage(
                id,
                state,
                "Read-only worker admission",
                Some(&serde_json::to_value(&candidate.plan)?),
            )?;
        }
        workers.push(candidate.worker);
        let mentor_id = format!("worker_{}", candidate.mentor + 1);
        let adviser_id = format!(
            "stakeholder_{}",
            candidate.stakeholder.unwrap_or(stakeholders.len()) + 1
        );
        let adviser = candidate
            .stakeholder
            .and_then(|index| stakeholders.get(index));
        self.reporter.network(NetworkEvent::Roster(vec![
            AgentNode {
                id: newcomer.clone(),
                name: workers
                    .last()
                    .context("missing incoming worker")?
                    .name
                    .clone(),
            },
            AgentNode {
                id: adviser_id.clone(),
                name: adviser
                    .map_or("Onboarding adviser", |agent| agent.name.as_str())
                    .into(),
            },
        ]));
        let memory = session.memory.clone();
        let limit = ipc::inbox_limit(self.config.context_window);
        let mut brief = format!(
            "Operator request:\n{request}\n\nProposed plan:\n{}\n\nOwnership:\n{}",
            candidate.plan.text,
            serde_json::to_string(&candidate.plan.ownership)?
        );
        if !candidate.briefed {
            let mentor = &workers[candidate.mentor];
            let member = &workers[workers.len() - 1];
            let roles = [
                (
                    newcomer.as_str(),
                    member.name.as_str(),
                    member.spec(self.config.worker_spec()),
                    format!(
                        "You are the incoming worker. Ask {mentor_id} about repository interfaces and {adviser_id} about requirements using send_message. Read the responses and confirm any constraints before finishing.\n{}",
                        member.persona
                    ),
                ),
                (
                    mentor_id.as_str(),
                    mentor.name.as_str(),
                    mentor.spec(self.config.worker_spec()),
                    format!(
                        "You are the incumbent worker mentor. Use send_message to brief {newcomer} on interfaces, progress, and integration. Answer follow-up questions.\n{}",
                        mentor.persona
                    ),
                ),
                (
                    adviser_id.as_str(),
                    adviser.map_or("Onboarding adviser", |agent| agent.name.as_str()),
                    adviser.map_or(self.config.reviewer_spec(), |agent| {
                        agent.spec(self.config.reviewer_spec())
                    }),
                    format!(
                        "You are the read-only stakeholder for the addition. Use send_message to brief {newcomer} on requirements, acceptance, and uncertainties. Respond to clarifications.\n{}",
                        adviser.map_or("", |agent| agent.persona.as_str())
                    ),
                ),
            ];
            let mailbox = Mailbox::new(
                roles
                    .iter()
                    .map(|(id, name, _, _)| Member {
                        id: (*id).into(),
                        name: (*name).into(),
                    })
                    .collect(),
                limit,
            )
            .for_workers(2)
            .observe(&self.reporter, traffic_scope(session));
            let saved = restore_panel(session, &mailbox, None, roles.len())?;
            let mentor_context = session
                .worker_progress
                .get(candidate.mentor)
                .map(|progress| {
                    format!(
                        "\nIncumbent report:\n{}\nPrior summary:\n{}\nRecent tool exchanges:\n{}",
                        clip(progress.assessment.as_deref().unwrap_or_default(), 1024),
                        clip(&progress.dialogue.summary, 1024),
                        clip(
                            &serde_json::to_string(&progress.dialogue.groups).unwrap_or_default(),
                            2048
                        )
                    )
                })
                .unwrap_or_default();
            let mut panel = roles
                .iter()
                .enumerate()
                .map(|(index, (id, name, model, role))| {
                    self.onboarding_agent(
                        session,
                        workspace,
                        saved,
                        index,
                        id,
                        name,
                        *model,
                        &format!(
                            "{role}\nRead-only onboarding. Roster:\n{}\n{}{}",
                            mailbox.roster(),
                            ipc::BWP,
                            if index == 1 { &mentor_context } else { "" }
                        ),
                        &brief,
                    )
                })
                .collect::<Result<Vec<_>>>()?;
            self.drive_panel(
                &mut panel,
                &mailbox,
                self.onboarding_options(&memory, workspace, None, true),
                |panel| self.checkpoint_panel(session, panel, &mailbox, None),
            )
            .await?;
            ensure!(
                mailbox.exchanged(0, 1) && mailbox.exchanged(0, 2),
                "incoming worker did not exchange delivered IPC messages with both mentors"
            );
            let incoming = panel[0].snapshot();
            session
                .onboarding
                .as_mut()
                .expect("onboarding active")
                .briefed = true;
            session
                .onboarding
                .as_mut()
                .expect("onboarding active")
                .brief = incoming.assessment;
            session.panel = None;
            self.store.save(session)?;
            if let Some(id) = session.active_worker_request {
                self.store.journal.borrow().worker_stage(
                    id,
                    "awaiting_ack",
                    "All workers must acknowledge the amended plan",
                    None,
                )?;
            }
        }
        if let Some(handoff) = session
            .onboarding
            .as_ref()
            .and_then(|state| state.brief.as_deref())
        {
            brief.push_str(&format!("\n\nOnboarding handoff:\n{}", clip(handoff, 4096)));
        }

        let mailbox = Mailbox::new(
            workers
                .iter()
                .enumerate()
                .map(|(index, worker)| Member {
                    id: format!("worker_{}", index + 1),
                    name: worker.name.clone(),
                })
                .collect(),
            limit,
        )
        .for_workers(workers.len())
        .observe(&self.reporter, traffic_scope(session));
        let gate = PlanGate::new(&candidate.plan.id, workers.len());
        let saved = restore_panel(session, &mailbox, Some(&gate), workers.len())?;
        let mut panel = workers.iter().enumerate().map(|(index, worker)| {
            self.onboarding_agent(session, workspace, saved, index, mailbox.id(index), &worker.name,
                worker.spec(self.config.worker_spec()),
                &format!("You are {}. Review this amended plan. All incumbent assignments stay frozen. Call ack_plan alone with the exact ID {} or use object_to_plan.\n{}", worker.name, candidate.plan.id, worker.persona),
                &brief)
        }).collect::<Result<Vec<_>>>()?;
        self.drive_panel(
            &mut panel,
            &mailbox,
            self.onboarding_options(&memory, workspace, Some(&gate), false),
            |panel| self.checkpoint_panel(session, panel, &mailbox, Some(&gate)),
        )
        .await?;
        ensure!(
            gate.objections().is_empty() && gate.acknowledgements().len() == workers.len(),
            "the amended plan did not receive unanimous acknowledgement"
        );
        let mut plan = candidate.plan.clone();
        plan.acknowledgements = gate.acknowledgements();
        ensure!(
            plan.verified(session.cycle, &workers),
            "amended plan digest mismatch"
        );
        let mut dialogue = Dialogue::start(format!(
            "Frozen plan ID: {}\n{}\n\nOnboarding handoff:\n{}",
            plan.id,
            plan.text,
            session
                .onboarding
                .as_ref()
                .and_then(|state| state.brief.as_deref())
                .unwrap_or_default()
        ));
        dialogue.guidance =
            format!("Your identity is {newcomer}. Work only on your new frozen assignment.");
        if let Some(ipc) = &mut session.team_ipc {
            ipc.mailbox.insert_worker(workers.len() - 1);
        }
        for progress in &mut session.worker_progress {
            self.append_guidance(&mut progress.dialogue.guidance, &format!("Active amended plan ID: {}. {newcomer} was added. Your prior assignment, write paths and dependencies are unchanged.", plan.id));
        }
        session.worker_progress.push(WorkerProgress {
            dialogue,
            assessment: None,
            artifact_sha256: None,
        });
        session.workers = Some(workers);
        session.worker_plan = Some(plan);
        Ok(newcomer)
    }

    #[allow(clippy::too_many_arguments)]
    fn onboarding_agent<'a>(
        &self,
        session: &Session,
        workspace: &std::path::Path,
        saved: &[WorkerProgress],
        index: usize,
        id: &str,
        name: &str,
        model: crate::config::ModelSpec<'a>,
        role: &str,
        task: &str,
    ) -> Result<PanelAgent<'a>> {
        let mut dialogue = saved.get(index).map_or_else(
            || Dialogue::start(task.into()),
            |state| state.dialogue.clone(),
        );
        dialogue.recover_interrupted_tool()?;
        Ok(PanelAgent {
            model,
            scope: format!("{id} / {name}"),
            system: self.system(&session.goal, role, workspace),
            dialogue,
            assessment: saved.get(index).and_then(|state| state.assessment.clone()),
            artifact_sha256: None,
            failed: false,
        })
    }

    fn onboarding_options<'a>(
        &self,
        memory: &'a SharedMemory,
        workspace: &'a std::path::Path,
        gate: Option<&'a PlanGate<'a>>,
        ipc: bool,
    ) -> PanelOptions<'a> {
        PanelOptions {
            access: Access::Inspect,
            width: self.config.orchestration.max_parallel_agents,
            ipc,
            tolerate_failures: false,
            gate,
            stakeholder_start: ipc.then_some(2),
            memory,
            guidance: None,
            ownership: None,
            dependencies: None,
            workspace,
            staging: None,
            admission_run: None,
            triage_run: None,
            jobs: None,
        }
    }

    async fn propose_extension(
        &self,
        session: &mut Session,
        workspace: &std::path::Path,
        request: &str,
        workers: &[Worker],
        stakeholders: &[crate::config::Stakeholder],
    ) -> Result<WorkerOnboarding> {
        let request_id = session
            .active_worker_request
            .context("missing active worker request")?;
        let previous = session
            .worker_plan
            .as_ref()
            .context("missing incumbent plan")?;
        let worker_id = format!("worker_{}", workers.len() + 1);
        let mentors = workers
            .iter()
            .enumerate()
            .map(|(index, worker)| (format!("worker_{}", index + 1), worker))
            .collect::<std::collections::BTreeMap<_, _>>();
        let advisers = stakeholders
            .iter()
            .enumerate()
            .map(|(index, adviser)| (format!("stakeholder_{}", index + 1), adviser))
            .collect::<std::collections::BTreeMap<_, _>>();
        let system = Message::system(format!(
            "Propose an append-only worker assignment. Call submit_worker_extension alone. Existing tasks and ownership are frozen. The new task owner is {worker_id}. For mentor, return an incumbent worker ID. For stakeholder, return a stakeholder ID or null for a temporary adviser. Use the IDs from the two rosters. Paths must be disjoint. Only use predecessor task IDs from the prior graph, or no predecessors when the graph is absent. Original goal:\n{}",
            session.goal
        ));
        let task = Message::user(format!(
            "Operator request:\n{request}\nExisting frozen plan:\n{}\nExisting ownership by worker index:\n{}\nTask graph:\n{}\nIncumbent workers by ID (mentor):\n{}\nStakeholders by ID (stakeholder):\n{}",
            previous.text,
            serde_json::to_string(&previous.ownership)?,
            serde_json::to_string(&previous.graph)?,
            serde_json::to_string(&mentors)?,
            serde_json::to_string(&advisers)?
        ));
        let schema = [
            json!({"type":"function","function":{"name":"submit_worker_extension","description":"Propose one worker without changing existing assignments.","parameters":{
            "type":"object","properties":{"name":{"type":"string"},"persona":{"type":"string"},"mentor":{"type":"string","enum":mentors.keys().collect::<Vec<_>>()},
            "stakeholder":{"type":["string","null"],"enum":advisers.keys().map(Some).chain(std::iter::once(None)).collect::<Vec<_>>()},"paths":{"type":"array","items":{"type":"object","properties":{"path":{"type":"string"},"directory":{"type":"boolean"}},"required":["path","directory"]}},
            "task":{"type":"object","properties":{"id":{"type":"string"},"worker":{"type":"string","enum":[worker_id]},"description":{"type":"string"},"depends_on":{"type":"array","items":{"type":"string"}},"acceptance":{"type":"array","items":{"type":"string"}}},"required":["id","worker","description","depends_on","acceptance"]}},
            "required":["name","persona","mentor","stakeholder","paths","task"]}}}),
        ];
        let mut correction = Message::user("Submit the extension now.");
        let mut repairs = 0;
        loop {
            self.api.wait_for_generation().await?;
            let messages = [&system, &task, &correction];
            ensure!(
                estimate(&messages, &schema) + self.config.output_tokens
                    <= self.config.context_window,
                "worker extension contract exceeds the configured context window"
            );
            let completion = match self
                .api
                .complete_as(
                    "lead_reviewer",
                    "Worker extension",
                    self.config.reviewer_spec(),
                    &messages,
                    &schema,
                    self.config.output_tokens,
                )
                .await
            {
                Err(ApiError::Cancelled) => continue,
                result => result?,
            };
            let extension = (|| -> Result<WorkerOnboarding> {
                ensure!(
                    !matches!(
                        completion.finish_reason.as_deref(),
                        Some("length" | "content_filter")
                    ),
                    "submit a complete concise extension"
                );
                let Message::Assistant { tool_calls, .. } = &completion.message else {
                    anyhow::bail!("expected assistant")
                };
                let [call] = tool_calls.as_slice() else {
                    anyhow::bail!("call submit_worker_extension alone")
                };
                ensure!(
                    call.function.name == "submit_worker_extension",
                    "call submit_worker_extension"
                );
                let input: Extension = crate::api::tool_arguments(&call.function.arguments)?;
                ensure!(
                    !input.name.trim().is_empty()
                        && !input.persona.trim().is_empty()
                        && workers
                            .iter()
                            .all(|worker| !worker.name.eq_ignore_ascii_case(input.name.trim())),
                    "new worker needs a unique name and assignment"
                );
                let mentor = if workers.len() == 1 {
                    0
                } else {
                    member_index(
                        &input.mentor,
                        "worker",
                        workers.iter().map(|worker| worker.name.as_str()),
                    )?
                };
                let stakeholder = input
                    .stakeholder
                    .as_deref()
                    .map(|id| {
                        member_index(
                            id,
                            "stakeholder",
                            stakeholders.iter().map(|agent| agent.name.as_str()),
                        )
                    })
                    .transpose()?;
                let worker = Worker {
                    name: input.name,
                    persona: input.persona,
                    model: None,
                    reasoning_effort: None,
                };
                ensure!(
                    input.task.worker == worker_id,
                    "new task must be assigned to {worker_id}"
                );
                let mut graph = previous.graph.clone();
                if let Some(graph) = &mut graph {
                    graph.0.push(input.task.clone());
                } else {
                    ensure!(
                        input.task.depends_on.is_empty(),
                        "graphless plans require an independent addition"
                    );
                    let mut task = input.task.clone();
                    task.worker = "worker_1".into();
                    TaskGraph(vec![task])
                        .dependencies(1)
                        .map_err(anyhow::Error::msg)?;
                }
                let mut ownership = previous.ownership.clone();
                ownership.push(input.paths);
                let ownership: std::collections::BTreeMap<_, _> = ownership
                    .into_iter()
                    .enumerate()
                    .map(|(index, paths)| (format!("worker_{}", index + 1), paths))
                    .collect();
                let mut roster = workers.to_vec();
                roster.push(worker.clone());
                let text = format!(
                    "{}\n\nAppended assignment:\n{}",
                    previous.text,
                    serde_json::to_string_pretty(&input.task)?
                );
                let mut synthetic: ToolCall = call.clone();
                synthetic.function.name = "submit_plan".into();
                synthetic.function.arguments =
                    json!({"plan":text,"ownership":ownership,"tasks":graph}).to_string();
                let frozen = parse_plan(&synthetic, &roster, workspace, graph.is_some())?;
                let plan = WorkerPlan::freeze(
                    frozen.text,
                    session.cycle,
                    session.plan_attempt,
                    &roster,
                    frozen.ownership,
                )
                .bind_graph(frozen.graph, session.cycle, &roster);
                Ok(WorkerOnboarding {
                    worker,
                    plan,
                    mentor,
                    stakeholder,
                    briefed: false,
                    brief: None,
                })
            })();
            match extension {
                Ok(extension) => {
                    let adviser = extension.stakeholder.map_or_else(
                        || "temporary adviser".into(),
                        |index| format!("stakeholder_{} ({})", index + 1, stakeholders[index].name),
                    );
                    session.dialogue.audit_note(
                        "lead_reviewer",
                        "proposal",
                        &self.api.redact(&format!(
                            "Worker request #{request_id} proposed {} ({worker_id}) with mentor worker_{} ({}) and {adviser}",
                            extension.worker.name,
                            extension.mentor + 1,
                            workers[extension.mentor].name,
                        )),
                    );
                    return Ok(extension);
                }
                Err(error) => {
                    let response = match &completion.message {
                        Message::Assistant {
                            content,
                            tool_calls,
                            ..
                        } => {
                            json!({"text": content, "tool_calls": tool_calls})
                        }
                        _ => json!(null),
                    };
                    session.dialogue.audit_note(
                        "lead_reviewer",
                        "contract_error",
                        &self.api.redact(&format!(
                            "Worker request #{request_id}, proposal attempt {}: {error:#}\n\n{response}",
                            repairs + 1
                        )),
                    );
                    self.store.save(session)?;
                    if repairs == 2 {
                        return Err(error);
                    }
                    repairs += 1;
                    correction = Message::user(format!("Correct the extension: {error:#}"));
                }
            }
        }
    }
}

fn member_index<'a>(
    id: &str,
    role: &str,
    names: impl ExactSizeIterator<Item = &'a str>,
) -> Result<usize> {
    let id = id.trim();
    let count = names.len();
    let index = id
        .strip_prefix(role)
        .and_then(|suffix| suffix.strip_prefix('_'))
        .and_then(|suffix| suffix.parse::<usize>().ok())
        .and_then(|index| index.checked_sub(1))
        .filter(|index| *index < count);
    if let Some(index) = index {
        return Ok(index);
    }
    let mut matching = names
        .enumerate()
        .filter_map(|(index, name)| name.eq_ignore_ascii_case(id).then_some(index));
    let first = matching.next();
    ensure!(
        matching.next().is_none(),
        "ambiguous {role} name {id}: use the ID"
    );
    first.with_context(|| match count {
        0 => format!("unknown {role} ID {id}: no configured {role}s"),
        1 => format!("unknown {role} ID {id}: use {role}_1"),
        _ => format!("unknown {role} ID {id}: use {role}_1 through {role}_{count}"),
    })
}
