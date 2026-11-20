use std::collections::HashSet;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    api::{Api, ApiError, Message},
    config::{Config, Stakeholder, Worker},
    context::estimate,
    session::Session,
};

#[derive(Clone, Deserialize, Serialize)]
pub struct Draft {
    pub goal: String,
    pub stakeholders: Vec<Stakeholder>,
    #[serde(default)]
    pub reviewers: Vec<Stakeholder>,
    #[serde(default)]
    pub workers: Vec<Worker>,
    #[serde(default)]
    pub acceptance: Vec<String>,
    pub message: String,
}

impl Draft {
    pub fn seed(config: &Config) -> Self {
        Self {
            goal: config.goal.clone().unwrap_or_default(),
            stakeholders: config.stakeholders.clone(),
            reviewers: config.reviewers.clone(),
            workers: config.workers.clone(),
            acceptance: Vec::new(),
            message: String::new(),
        }
    }

    pub fn into_session(self) -> Session {
        let mut session = Session::new(self.goal);
        session.stakeholders = Some(self.stakeholders);
        session.reviewers = Some(self.reviewers);
        session.require_task_graph = !self.acceptance.is_empty() && !self.workers.is_empty();
        if !self.acceptance.is_empty() {
            session.dialogue.task.push_str(&format!(
                "\nStartup acceptance criteria:\n{}",
                self.acceptance.join("\n")
            ));
        }
        session.workers = Some(self.workers);
        session.team_acceptance = self.acceptance;
        session
    }
}

pub async fn refine(config: &Config, api: &Api, draft: &Draft, user: &str) -> Result<Draft> {
    let generate = !config.fixed_team && config.workers.is_empty();
    let worker_prompt = if generate {
        format!(
            "Include workers, an array of 1 to {} objects with name and persona. The persona assigns independent work and preliminary write scope. Include acceptance, an array of explicit project acceptance criteria. Prefer a single worker for tightly coupled tasks.",
            config.orchestration.max_parallel_agents
        )
    } else {
        "Retain the configured workers. Include project acceptance criteria in an acceptance array."
            .into()
    };
    let instructions = Message::system(format!(
        "Help the user plan a Ratio coding task. Refine the goal to describe the requested outcome, scope and acceptance criteria. A lead reviewer is built in. Add specialist reviewers and stakeholders when distinct expertise would help. Reviewers check evidence and report findings. Stakeholders advise on domain requirements. They can participate in planning and/or review. Preserve configured reviewers and stakeholders. Never perform the work.\n\
         Include goal, reviewers, stakeholders, and message. goal and message are strings. reviewers and stakeholders are arrays, which may be empty, of objects with name, persona (the participant description), phases (an array of planning and/or review), and optional agent with a model string. Return the full current proposal after each refinement. Put your response or question in message.\n\
         {worker_prompt}\n{}\nWorkspace: {}\nConfigured constraints:\n{}{}",
        if generate {
            "Return the proposal by calling submit_team alone."
        } else {
            "Return the proposal as JSON text."
        },
        config.cwd.display(),
        config.constraints.prompt(),
        config
            .project_instructions()
            .map(|text| crate::instructions::prompt(&text))
            .unwrap_or_default()
    ));
    let mut previous = draft.clone();
    let mut length_retries = 0;
    let mut correction = None;
    let mut repairs = 0;
    let schemas = if generate {
        vec![team_schema(config.orchestration.max_parallel_agents)]
    } else {
        Vec::new()
    };
    loop {
        let prompt = Message::user(format!(
            "Current proposal (JSON):\n{}\n\nUser input:\n{user}{}",
            serde_json::to_string(&previous)?,
            if length_retries > 0 {
                "\nUse concise descriptions and return complete JSON."
            } else {
                ""
            }
        ));
        let mut messages = vec![&instructions, &prompt];
        if let Some(message) = &correction {
            messages.push(message);
        }
        ensure!(
            estimate(&messages, &schemas).saturating_add(config.output_tokens)
                <= config.context_window,
            "setup proposal and feedback exceed the context budget"
        );
        api.wait_for_generation().await?;
        let completion = match api
            .complete_for(
                "Setup agent",
                config.reviewer_spec(),
                &messages,
                &schemas,
                config.output_tokens,
            )
            .await
        {
            Err(ApiError::Cancelled) => continue,
            result => result?,
        };
        match completion.finish_reason.as_deref() {
            Some("length") if length_retries < 2 => {
                length_retries += 1;
                previous.message =
                    crate::context::clip(&previous.message, previous.message.len() / 3);
                continue;
            }
            Some("length" | "content_filter") => {
                anyhow::bail!("provider stopped before returning the full proposal");
            }
            _ => {}
        }
        match decode(completion, generate).and_then(|mut draft| {
            if generate {
                let configured_names: HashSet<_> = config
                    .reviewers
                    .iter()
                    .chain(&config.stakeholders)
                    .map(|agent| agent.name.trim().to_lowercase())
                    .collect();
                for (agents, configured) in [
                    (&mut draft.reviewers, &config.reviewers),
                    (&mut draft.stakeholders, &config.stakeholders),
                ] {
                    agents.retain(|agent| !configured_names.contains(&agent.name.to_lowercase()));
                    agents.splice(0..0, configured.iter().cloned());
                }
                ensure!(
                    draft.reviewers.len() + draft.stakeholders.len() <= 64,
                    "proposal plus configured participants exceeds the 64-person panel limit"
                );
                ensure!(
                    (1..=config.orchestration.max_parallel_agents).contains(&draft.workers.len()),
                    "return a worker roster within the requested count"
                );
                ensure!(
                    !draft.acceptance.is_empty()
                        && draft
                            .acceptance
                            .iter()
                            .all(|criterion| !criterion.trim().is_empty()),
                    "include project acceptance criteria"
                );
                let mut names = HashSet::new();
                for worker in &mut draft.workers {
                    worker.name = worker.name.trim().to_owned();
                    worker.persona = worker.persona.trim().to_owned();
                    worker.model = None;
                    worker.reasoning_effort = None;
                    ensure!(
                        !worker.name.is_empty()
                            && !worker.persona.is_empty()
                            && names.insert(worker.name.to_lowercase()),
                        "each worker needs a unique name and assignment"
                    );
                }
            } else {
                draft.workers.clone_from(&config.workers);
            }
            Ok(draft)
        }) {
            Ok(draft) => return Ok(draft),
            Err(error) if repairs < 2 => {
                repairs += 1;
                correction = Some(Message::user(format!(
                    "Correct the proposal and use the requested response contract. Validation error: {error:#}"
                )));
            }
            Err(error) => return Err(error),
        }
    }
}

fn team_schema(max_workers: usize) -> Value {
    let participant = json!({"type":"object","properties":{
        "name":{"type":"string"},"persona":{"type":"string"},"phases":{"type":"array","minItems":1,
        "items":{"type":"string","enum":["planning","review"]}}},"required":["name","persona","phases"]});
    json!({"type":"function","function":{"name":"submit_team","description":"Submit the complete startup roster and acceptance criteria as the sole call.","parameters":{
        "type":"object","properties":{
        "goal":{"type":"string"},"message":{"type":"string"},
        "stakeholders":{"type":"array","items":participant},
        "reviewers":{"type":"array","items":participant},
        "workers":{"type":"array","minItems":1,"maxItems":max_workers,"items":{"type":"object","properties":{"name":{"type":"string"},"persona":{"type":"string"}},"required":["name","persona"]}},
        "acceptance":{"type":"array","minItems":1,"items":{"type":"string"}}},
        "required":["goal","message","stakeholders","reviewers","workers","acceptance"]}}})
}

fn decode(completion: crate::api::Completion, typed: bool) -> Result<Draft> {
    let Message::Assistant {
        content,
        tool_calls,
        ..
    } = completion.message
    else {
        anyhow::bail!("setup model did not return proposal text");
    };
    let mut encoded: serde_json::Value = if typed {
        let [call] = tool_calls.as_slice() else {
            anyhow::bail!("call submit_team alone")
        };
        ensure!(
            call.kind == "function" && call.function.name == "submit_team" && !call.id.is_empty(),
            "expected submit_team"
        );
        crate::api::tool_arguments(&call.function.arguments)?
    } else {
        ensure!(
            tool_calls.is_empty(),
            "setup model returned unexpected tool calls"
        );
        serde_json::from_str(unfence(
            content.as_deref().context("missing setup proposal text")?,
        ))
        .context("decode setup proposal JSON")?
    };
    for field in ["stakeholders", "reviewers"] {
        if let Some(list) = encoded
            .get_mut(field)
            .and_then(serde_json::Value::as_array_mut)
        {
            for entry in list {
                if let Some(entry) = entry.as_object_mut() {
                    entry.retain(|key, _| {
                        matches!(
                            key.as_str(),
                            "name" | "description" | "persona" | "phases" | "agent"
                        )
                    });
                }
            }
        }
    }
    let mut draft: Draft =
        serde_json::from_value(encoded).context("validate setup proposal fields")?;
    draft.goal = draft.goal.trim().to_owned();
    ensure!(!draft.goal.is_empty(), "setup model returned an empty goal");
    let mut names = HashSet::new();
    ensure!(
        draft.reviewers.len() + draft.stakeholders.len() <= 64,
        "configure at most 64 reviewers and stakeholders combined"
    );
    for stakeholder in draft.reviewers.iter_mut().chain(&mut draft.stakeholders) {
        stakeholder.name = stakeholder.name.trim().to_owned();
        stakeholder.persona = stakeholder.persona.trim().to_owned();
        ensure!(
            !stakeholder.name.is_empty()
                && !stakeholder.persona.is_empty()
                && !stakeholder.phases.is_empty(),
            "each stakeholder needs a name, description, and at least one phase"
        );
        ensure!(
            names.insert(stakeholder.name.to_lowercase()),
            "stakeholder names must be unique"
        );
    }
    Ok(draft)
}

fn unfence(text: &str) -> &str {
    let text = text.trim();
    text.strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .and_then(|text| text.strip_suffix("```"))
        .map_or(text, str::trim)
}
