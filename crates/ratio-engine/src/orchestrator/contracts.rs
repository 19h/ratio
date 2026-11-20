use std::collections::BTreeMap;

use serde::Deserialize;

use super::*;

#[derive(Deserialize)]
struct Assignment {
    task: ratio_protocol::Task,
    paths: Vec<WritePath>,
}

impl Orchestrator<'_> {
    pub(super) async fn recover_plan(
        &self,
        session: &Session,
        workspace: &Path,
        workers: &[Worker],
        error: &str,
    ) -> Result<FrozenSubmission> {
        let schema = plan_schema(workers.len(), true);
        let parameters = &schema["function"]["parameters"]["properties"];
        let paths = parameters["ownership"]["properties"]["worker_1"].clone();
        // Future slots temporarily have no edges, so each contribution can be
        // checked for invalid references and cycles against the complete roster.
        let mut assignments = (1..=workers.len())
            .map(|index| {
                let id = format!("worker_{index}");
                ratio_protocol::Task {
                    id: id.clone(),
                    worker: id,
                    description: "Pending assignment".into(),
                    depends_on: Vec::new(),
                    acceptance: vec!["Pending assignment".into()],
                }
            })
            .collect::<Vec<_>>();
        let mut ownership = BTreeMap::new();
        let mut submission_call = None;
        for (index, worker) in workers.iter().enumerate() {
            let id = format!("worker_{}", index + 1);
            let mut task = parameters["tasks"]["items"].clone();
            task["properties"]["worker"]["enum"] = json!([id]);
            let schemas = [
                json!({"type":"function","function":{"name":"submit_worker_assignment","description":"Submit one worker assignment.","parameters":{
                "type":"object","properties":{"task":task,"paths":paths},"required":["task","paths"],"additionalProperties":false}}}),
            ];
            let system = Message::system(format!(
                "Recover a multi-worker plan one assignment at a time. Submit only {id} with submit_worker_assignment. Assign disjoint write paths. Use worker_N as each task ID and for predecessor IDs. Reference only the following workers: {}. No prose.",
                (1..=workers.len())
                    .map(|n| format!("worker_{n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            let output = self.config.output_tokens;
            let input = self.config.context_window - output;
            let task = Message::user(format!(
                "Worker: {} / {}\nGoal: {}\nCoordination: {}\nExisting ownership: {}\nPrevious validation: {}",
                clip(&worker.name, 240),
                clip(&worker.persona, input / 2),
                clip(&session.goal, input / 2),
                clip(&session.dialogue.current_task(), input),
                clip(&serde_json::to_string(&ownership)?, input / 2),
                clip(error, 512)
            ));
            let mut correction = Message::user("Submit this assignment.");
            let mut repairs = 0;
            loop {
                self.api.wait_for_generation().await?;
                let messages = [&system, &task, &correction];
                ensure!(
                    estimate(&messages, &schemas) <= input,
                    "assignment recovery contract exceeds context budget"
                );
                let completed = match self
                    .api
                    .complete_as(
                        "lead_reviewer",
                        "Recover worker assignment",
                        self.config.reviewer_spec(),
                        &messages,
                        &schemas,
                        output,
                    )
                    .await
                {
                    Err(ApiError::Cancelled) => continue,
                    result => result?,
                };
                let parsed = (|| -> Result<_> {
                    ensure!(
                        !matches!(
                            completed.finish_reason.as_deref(),
                            Some("length" | "content_filter")
                        ),
                        "return a complete concise assignment"
                    );
                    let Message::Assistant { tool_calls, .. } = completed.message else {
                        anyhow::bail!("expected assistant")
                    };
                    let [call] = tool_calls.as_slice() else {
                        anyhow::bail!("call submit_worker_assignment alone")
                    };
                    ensure!(
                        call.kind == "function"
                            && call.function.name == "submit_worker_assignment"
                            && !call.id.is_empty(),
                        "expected submit_worker_assignment"
                    );
                    let slot: Assignment = crate::api::tool_arguments(&call.function.arguments)?;
                    ensure!(
                        slot.task.id == id && slot.task.worker == id,
                        "assignment must use {id} as both task ID and owner"
                    );
                    let mut tasks = assignments.clone();
                    tasks[index] = slot.task.clone();
                    TaskGraph(tasks)
                        .dependencies(workers.len())
                        .map_err(anyhow::Error::msg)?;
                    let mut check = call.clone();
                    let mut paths = ownership.clone();
                    paths.insert(id.clone(), slot.paths.clone());
                    for remaining in index + 2..=workers.len() {
                        paths.insert(format!("worker_{remaining}"), Vec::new());
                    }
                    check.function.name = "submit_plan".into();
                    check.function.arguments =
                        json!({"plan":"Validate assignment ownership","ownership":paths})
                            .to_string();
                    parse_plan(&check, workers, workspace, false)?;
                    Ok((call.clone(), slot))
                })();
                match parsed {
                    Ok((call, slot)) => {
                        submission_call = Some(call);
                        ownership.insert(id.clone(), slot.paths);
                        assignments[index] = slot.task;
                        break;
                    }
                    Err(error) if repairs < 2 => {
                        repairs += 1;
                        correction = Message::user(format!("Correct {id}: {error:#}"))
                    }
                    Err(error) => {
                        return Err(error).with_context(|| format!("recover assignment for {id}"));
                    }
                }
            }
        }
        let mut call = submission_call.context("no recovered assignments")?;
        let text = assignments
            .iter()
            .map(|task| {
                format!(
                    "{}: {}\nAcceptance: {}",
                    task.worker,
                    task.description,
                    task.acceptance.join(" | ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        call.function.name = "submit_plan".into();
        call.function.arguments =
            json!({"plan":text,"ownership":ownership,"tasks":assignments}).to_string();
        parse_plan(&call, workers, workspace, true).context("validate assembled recovery plan")
    }
}
