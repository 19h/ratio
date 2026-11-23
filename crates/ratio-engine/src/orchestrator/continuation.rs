use super::*;
use crate::session::{ContinuationChoice, ContinuationDecision};

impl Orchestrator<'_> {
    pub(super) async fn review_continuation(
        &self,
        session: &mut Session,
        guidance: Option<&Receiver<String>>,
    ) -> Result<bool> {
        if !matches!(
            session.phase,
            Phase::ContinuationReview | Phase::ContinuationDiscuss | Phase::ContinuationDecision
        ) {
            return Ok(false);
        }
        let request = session
            .continuation
            .as_ref()
            .context("Missing continuation request")?;
        let approved = self
            .store
            .load_checkpoint(session.run_id(), request.approved_checkpoint)?;
        ensure!(
            approved.phase == Phase::Approved,
            "Continuation requires an approved baseline"
        );
        let base = format!(
            "Approved checkpoint #{}\nApproved goal: {}\nApproved assessment: {}\nVerification: {}\n\nOperator follow-up:\n{}",
            request.approved_checkpoint,
            clip(&approved.goal, 1024),
            clip(&approved.assessment, 1536),
            approved.verification.as_ref().map_or_else(
                || "No receipt".into(),
                |receipt| {
                    format!(
                        "passed={}, digest={}, {}",
                        receipt.passed,
                        receipt.digest,
                        clip(&receipt.report, 512)
                    )
                }
            ),
            request.prompt,
        );
        while matches!(
            session.phase,
            Phase::ContinuationReview | Phase::ContinuationDiscuss
        ) {
            self.receive_guidance(session, guidance)?;
            self.reporter.emit(Kind::Phase, session.phase.label(), "");
            let request = session
                .continuation
                .as_ref()
                .context("Missing continuation request")?;
            let mut brief = base.clone();
            if session.phase == Phase::ContinuationDiscuss {
                brief.push_str(&self.continuation_digest(
                    &base,
                    &[("Initial findings", &request.initial_reports)],
                )?);
            }
            session.dialogue.task = brief;
            self.consult(session, &self.config.cwd).await?;
            session.phase = if session.phase == Phase::ContinuationReview {
                Phase::ContinuationDiscuss
            } else {
                Phase::ContinuationDecision
            };
            session.panel = None;
            self.store.save(session)?;
        }
        self.reporter.emit(Kind::Phase, session.phase.label(), "");
        let mut request = session
            .continuation
            .clone()
            .context("Missing continuation request")?;
        let evidence = self.continuation_digest(
            &base,
            &[
                ("Initial findings", &request.initial_reports),
                ("Discussion", &request.discussion),
            ],
        )?;
        self.receive_guidance(session, guidance)?;
        session.dialogue.task = format!("{base}{evidence}");
        let decision = self
            .continuation_decision(
                &clip(&approved.goal, 1024),
                &session.dialogue.current_task(),
            )
            .await?;
        let proceed = decision.decision == ContinuationChoice::Continue;
        let label = if proceed { "Continue" } else { "Decline" };
        let detail = format!(
            "{label}\n{}{}",
            decision.reason,
            decision
                .plan
                .as_ref()
                .map_or_else(String::new, |plan| format!("\n\nContinuation plan\n{plan}"))
        );
        if proceed {
            let plan = decision
                .plan
                .as_deref()
                .context("Missing continuation plan")?;
            let has_team = session
                .workers
                .as_ref()
                .is_some_and(|workers| !workers.is_empty());
            session.request.get_or_insert_with(|| approved.goal.clone());
            session.goal.clone_from(&request.prompt);
            session.team_acceptance.clear();
            session.phase = if has_team {
                Phase::Coordinate
            } else {
                Phase::Work
            };
            session.dialogue = Dialogue::start(format!(
                "{}\nOperator follow-up:\n{}\n\nAccepted continuation plan:\n{plan}",
                if has_team {
                    "Coordinate worker assignments and verification."
                } else {
                    "Implement and verify this plan."
                },
                request.prompt,
            ));
            session.workspace_prepared = Some(false);
            session.verdict = None;
            session.verdict_tool_call_id = None;
            session.assessment.clear();
            session.verification = None;
            session.worker_plan = None;
            session.worker_progress.clear();
            session.team_ipc = None;
            session.jobs = Default::default();
            session.plan_attempt = 1;
            session.work_intake_closed = false;
            session.worker_addition_status = None;
            session.active_worker_request = None;
            session.onboarding = None;
        } else {
            *session = approved;
        }
        session.dialogue.audit.push(crate::audit::Observation {
            key: format!("continuation-decision-{}", request.id),
            kind: "continuation_decision",
            actor: "lead_reviewer".into(),
            data: json!({
                "text": self.api.redact(&detail), "continuation_cycle": request.cycle,
                "decision": decision.decision,
            }),
        });
        request.decision = Some(decision);
        session.continuation = Some(request);
        self.store.save(session)?;
        Ok(!proceed)
    }

    fn continuation_digest(
        &self,
        base: &str,
        rounds: &[(&str, &[(String, String)])],
    ) -> Result<String> {
        let count: usize = rounds.iter().map(|(_, reports)| reports.len()).sum();
        if count == 0 {
            return Ok(String::new());
        }
        let quota = self
            .config
            .context_window
            .saturating_sub(self.config.output_tokens)
            .min(32 * 1024)
            .saturating_sub(base.len())
            / 2
            / count;
        ensure!(
            quota >= 64,
            "Context window is too small for all continuation reports. Raise it and resume."
        );
        let mut digest = String::new();
        for (label, reports) in rounds {
            digest.push_str(&format!("\n\n{label}\n"));
            for (actor, report) in *reports {
                digest.push_str(&format!("{actor}: {}\n", clip(report, quota)));
            }
        }
        Ok(digest)
    }

    async fn continuation_decision(&self, goal: &str, brief: &str) -> Result<ContinuationDecision> {
        let system = Message::system(self.system(goal,
            "You are the lead reviewer. Decide whether this follow-up can proceed in this run. Address stakeholder disagreements and use the approved result as context. Use submit_continuation alone. If continuing, provide a scoped plan with deliverables, constraints, steps and verifiable acceptance criteria. For a decline, return null for plan.",
            &self.config.cwd));
        let schema = json!({"type":"function","function":{"name":"submit_continuation","description":"Record the decision and any continuation plan",
            "parameters":{"type":"object","additionalProperties":false,"properties":{
                "decision":{"type":"string","enum":["continue","decline"]},
                "reason":{"type":"string"},
                "plan":{"type":["string","null"]}
            },"required":["decision","reason","plan"]}}});
        let mut task = Message::user(brief);
        let mut correction = None;
        let mut attempts = 0;
        while attempts < 3 {
            let messages = [&system, &task];
            ensure!(
                estimate(&messages, std::slice::from_ref(&schema))
                    <= self.config.context_window - self.config.output_tokens,
                "Context window is too small for the continuation decision. Raise it and resume."
            );
            self.api.wait_for_generation().await?;
            let response = self
                .api
                .complete_as(
                    "lead_reviewer",
                    "Continuation decision",
                    self.config.reviewer_spec(),
                    &messages,
                    std::slice::from_ref(&schema),
                    self.config.output_tokens,
                )
                .await;
            let completion = match response {
                Err(ApiError::Cancelled) => continue,
                result => result?,
            };
            attempts += 1;
            let decision = if completion.finish_reason.as_deref() == Some("length") {
                Err(anyhow::anyhow!(
                    "The decision was truncated. Return a shorter, complete decision."
                ))
            } else {
                parse_decision(completion.message)
            };
            match decision {
                Ok(decision) => return Ok(decision),
                Err(error) => {
                    task = Message::user(format!(
                        "{brief}\n\nCorrect the previous response: {error:#}"
                    ));
                    correction = Some(error);
                }
            }
        }
        Err(correction.expect("decision repairs exhausted"))
            .context("Invalid continuation decision")
    }
}

fn parse_decision(message: Message) -> Result<ContinuationDecision> {
    if let Message::Assistant { tool_calls, .. } = message
        && let [call] = tool_calls.as_slice()
        && call.kind == "function"
        && call.function.name == "submit_continuation"
        && !call.id.is_empty()
    {
        let arguments: Value = crate::api::tool_arguments(&call.function.arguments)?;
        ensure!(
            arguments.get("plan").is_some(),
            "The decision must include plan or null"
        );
        let mut decision: ContinuationDecision =
            serde_json::from_value(arguments).context("Invalid continuation decision arguments")?;
        decision.reason = decision.reason.trim().to_owned();
        decision.plan = decision.plan.map(|plan| plan.trim().to_owned());
        ensure!(
            (1..=4096).contains(&decision.reason.len()),
            "Give a rationale of up to 4096 UTF-8 bytes"
        );
        ensure!(
            match decision.decision {
                ContinuationChoice::Continue => decision
                    .plan
                    .as_ref()
                    .is_some_and(|plan| (1..=8192).contains(&plan.len())),
                ContinuationChoice::Decline => decision.plan.is_none(),
            },
            "Supply a plan of up to 8192 UTF-8 bytes when continuing, or null when declining"
        );
        return Ok(decision);
    }
    anyhow::bail!("Call submit_continuation alone")
}
