use std::collections::HashSet;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{
    api::{Api, Message},
    config::{Config, Stakeholder},
    context::estimate,
    session::Session,
};

#[derive(Clone, Deserialize, Serialize)]
pub struct Draft {
    pub goal: String,
    pub stakeholders: Vec<Stakeholder>,
    #[serde(default)]
    pub reviewers: Vec<Stakeholder>,
    pub message: String,
}

impl Draft {
    pub fn seed(config: &Config) -> Self {
        Self {
            goal: config.goal.clone().unwrap_or_default(),
            stakeholders: config.stakeholders.clone(),
            reviewers: config.reviewers.clone(),
            message: String::new(),
        }
    }

    pub fn into_session(self) -> Session {
        let mut session = Session::new(self.goal);
        session.stakeholders = Some(self.stakeholders);
        session.reviewers = Some(self.reviewers);
        session
    }
}

pub async fn refine(config: &Config, api: &Api, draft: &Draft, user: &str) -> Result<Draft> {
    let instructions = Message::system(format!(
        "Help the user plan a Ratio coding task. Refine the goal to describe the requested outcome, scope and acceptance criteria. The lead reviewer and worker are built in. Add specialist reviewers and stakeholders when distinct expertise would help. Reviewers check evidence and report findings. Stakeholders advise on domain requirements. They can participate in planning and/or review. Keep user preferences and named model overrides unless asked to change them. Ask a focused question if it would clarify scope. Never perform the work.\n\
         Return one JSON object with keys goal, reviewers, stakeholders, and message. goal and message are strings. reviewers and stakeholders are arrays, which may be empty, of objects with name, persona (the participant description), phases (an array of planning and/or review), and optional agent with a model string. Return the full current proposal after each refinement. Put your response or question in message.\n\
         Workspace: {}\nConfigured constraints:\n{}",
        config.cwd.display(),
        config.constraints.prompt()
    ));
    let prompt = Message::user(format!(
        "Current proposal (JSON):\n{}\n\nUser input:\n{user}",
        serde_json::to_string(draft)?
    ));
    let messages = [&instructions, &prompt];
    ensure!(
        estimate(&messages, &[]) + config.output_tokens < config.context_window,
        "setup proposal and feedback exceed the context budget"
    );
    let mut correction = None;
    for _ in 0..=1 {
        let mut messages = messages.to_vec();
        if let Some(message) = &correction {
            messages.push(message);
        }
        let completion = api
            .complete(&config.reviewer_model, &messages, &[], config.output_tokens)
            .await?;
        match decode(completion) {
            Ok(draft) => return Ok(draft),
            Err(error) if correction.is_none() => {
                correction = Some(Message::user(format!(
                    "Correct the proposal and return complete JSON only. Validation error: {error:#}"
                )));
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("setup repair is bounded")
}

fn decode(completion: crate::api::Completion) -> Result<Draft> {
    ensure!(
        !matches!(
            completion.finish_reason.as_deref(),
            Some("length" | "content_filter")
        ),
        "provider stopped before returning the full proposal"
    );
    let Message::Assistant {
        content: Some(content),
        tool_calls,
        ..
    } = completion.message
    else {
        anyhow::bail!("setup model did not return proposal text");
    };
    ensure!(
        tool_calls.is_empty(),
        "setup model returned unexpected tool calls"
    );
    let mut encoded: serde_json::Value =
        serde_json::from_str(unfence(&content)).context("decode setup proposal JSON")?;
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
