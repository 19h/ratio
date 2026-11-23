use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    rc::Rc,
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    api::{Function, ToolCall, tool_arguments},
    audit::{Observation, Outbox, bounded},
    config::Worker,
    context::{Dialogue, clip},
    session::Phase,
    tools::{Access, schema},
};

pub(crate) const TOOLS: &[&str] = &[
    "jobs",
    "list_specialists",
    "delegate",
    "start_tool",
    "await_jobs",
    "job_status",
];

pub(crate) fn enabled(phase: Phase) -> bool {
    matches!(
        phase,
        Phase::Plan
            | Phase::PlanConsult
            | Phase::PlanSynthesize
            | Phase::Coordinate
            | Phase::FreezePlan
            | Phase::Work
            | Phase::Review
            | Phase::ReviewConsult
            | Phase::ReviewSynthesize
    )
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Profile {
    Inspect,
    Memory,
    Operator,
}

impl Profile {
    pub fn allows(self, tool: &str) -> bool {
        TOOLS.contains(&tool)
            || match self {
                Self::Inspect => matches!(
                    tool,
                    "read_file"
                        | "list_directory"
                        | "search_files"
                        | "find_files"
                        | "git_status"
                        | "git_diff"
                        | "read_skill"
                        | "mcp_list"
                        | "tool_search"
                        | "mcp_call"
                        | "recall"
                        | "remember"
                ),
                Self::Memory => matches!(tool, "remember" | "recall"),
                Self::Operator => matches!(tool, "mcp_list" | "tool_search" | "mcp_call"),
            }
    }
}

#[derive(Clone, Serialize)]
pub(crate) struct Role {
    pub id: String,
    pub name: String,
    pub instructions: String,
    pub profile: Profile,
    pub model: Option<String>,
    #[serde(skip)]
    pub effort: Option<String>,
}

pub(crate) fn roles(workers: &[Worker]) -> Vec<Role> {
    let mut roles = [
        ("research", "Research", Profile::Inspect,
            "Investigate the question. Verify findings against primary evidence available through inspection tools. Return findings, evidence references, and gaps."),
        ("synthesis", "Synthesis", Profile::Inspect,
            "Produce a compact synthesis, wrap-up, or TL;DR. Preserve references and disagreements. Consult another specialist when needed."),
        ("adviser", "Adviser", Profile::Inspect,
            "Provide a second opinion. Examine assumptions, options, and tradeoffs, then return a concise recommendation."),
        ("memory", "Memory", Profile::Memory,
            "Retrieve or organize shared run memory. Preserve authorship and evidence references. Use remember only for durable, evidence-backed notes."),
        ("tool_operator", "Tool operator", Profile::Operator,
            "Handle the requested operation using the configured MCP tools. Discover schemas first. Return the outcome and any handles needed by follow-up jobs."),
    ].into_iter().map(|(id, name, profile, instructions)| Role {
        id: id.into(), name: name.into(), profile, instructions: instructions.into(), model: None,
        effort: None,
    }).collect::<Vec<_>>();
    roles.extend(workers.iter().enumerate().map(|(index, worker)| Role {
        id: format!("worker_{}", index + 1),
        name: worker.name.clone(),
        instructions: format!("Consult using this worker persona:\n{}", worker.persona),
        profile: Profile::Inspect,
        model: worker.model.clone(),
        effort: worker.reasoning_effort.clone(),
    }));
    roles
}

pub(crate) fn schemas(shell: bool, mcp: bool) -> Vec<Value> {
    let mut schemas = vec![
        schema(
            "list_specialists",
            "Discover specialist roles and read-only worker consultations.",
            json!({}),
            &[],
        ),
        schema(
            "delegate",
            "Start a consultation. Worker personas advise only. The run assigns implementation in Work. Pass a self-contained task and optional evidence. Returns a job ID.",
            json!({
                "role":{"type":"string"}, "task":{"type":"string"},
                "evidence":{"type":"array","items":{"type":"string"},"description":"Exact tool name for the latest result, call ID, or inherited source ID."}
            }),
            &["role", "task"],
        ),
        schema(
            "await_jobs",
            "Wait for your jobs without occupying an agent slot. Results also arrive as untrusted context.",
            json!({
                "jobs":{"type":"array","items":{"type":"integer","minimum":1},"minItems":1}
            }),
            &["jobs"],
        ),
        schema(
            "job_status",
            "Get the state and compact result of one of your jobs.",
            json!({
                "job":{"type":"integer","minimum":1}
            }),
            &["job"],
        ),
    ];
    let tools = [shell.then_some("shell"), mcp.then_some("mcp_call")]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if !tools.is_empty() {
        schemas.push(schema("start_tool", "Run a shell or MCP call in the background. Use the same arguments as the direct tool. Returns a job ID.", json!({
            "tool":{"type":"string","enum":tools},"arguments":{"type":"object"}
        }), &["tool","arguments"]));
    }
    schemas
}

pub(crate) fn compact_schema() -> Value {
    schema(
        "jobs",
        "list_specialists ({}), delegate for consultation (role, task, evidence), start_tool (tool, arguments), await_jobs (jobs), job_status (job).",
        json!({"op":{"type":"string"}, "args":{"type":"object"}}),
        &["op"],
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompactCall {
    op: String,
    #[serde(default)]
    args: serde_json::Map<String, Value>,
}

pub(crate) fn expand(call: &ToolCall) -> Result<ToolCall> {
    if call.function.name != "jobs" {
        return Ok(call.clone());
    }
    let input: CompactCall = tool_arguments(&call.function.arguments)?;
    ensure!(
        input.op != "jobs" && TOOLS.contains(&input.op.as_str()),
        "unknown job action"
    );
    let mut expanded = call.clone();
    expanded.function.name = input.op;
    expanded.function.arguments = Value::Object(input.args).to_string();
    Ok(expanded)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Delegate {
    pub role: String,
    pub task: String,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StartTool {
    tool: String,
    arguments: serde_json::Map<String, Value>,
}

impl StartTool {
    pub fn call(self) -> ToolCall {
        ToolCall {
            id: String::new(),
            kind: "function".into(),
            function: Function {
                name: self.tool,
                arguments: Value::Object(self.arguments).to_string(),
            },
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Wait {
    jobs: BTreeSet<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Status {
    job: u64,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Source {
    pub id: String,
    pub actor: String,
    pub call_id: String,
    pub tool: String,
    pub digest: String,
    pub excerpt: String,
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum State {
    Queued,
    Running,
    Completed,
    Failed,
    Refused,
    Unknown,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Refused => "refused",
            Self::Unknown => "unknown",
        }
    }

    pub fn terminal(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) enum Work {
    Specialist(Box<Specialist>),
    Tool(ToolCall),
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Specialist {
    pub role: String,
    pub name: String,
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub instructions: String,
    pub profile: Profile,
    pub dialogue: Dialogue,
    pub held_report: Option<String>,
    pub failed: bool,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
pub(crate) struct Permissions {
    workspace: bool,
    memory: bool,
    mcp: bool,
    shell: bool,
}

impl Permissions {
    pub fn all() -> Self {
        Self {
            workspace: true,
            memory: true,
            mcp: true,
            shell: true,
        }
    }

    pub fn restricted(self, profile: Profile) -> Self {
        Self {
            workspace: self.workspace && profile == Profile::Inspect,
            memory: self.memory && profile != Profile::Operator,
            mcp: self.mcp && profile != Profile::Memory,
            shell: false,
        }
    }

    pub fn permits(self, tool: &str) -> bool {
        match tool {
            "mcp_list" | "tool_search" | "mcp_call" => self.mcp,
            "shell" => self.shell,
            "remember" | "recall" => self.memory,
            "read_file" | "list_directory" | "search_files" | "find_files" | "git_status"
            | "git_diff" | "read_skill" => self.workspace,
            _ => true,
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Job {
    pub id: u64,
    pub owner: String,
    pub policy_actor: String,
    pub mcp_actor: String,
    pub workspace: PathBuf,
    pub shared_candidate: bool,
    pub access: Access,
    pub permissions: Permissions,
    pub ancestry: BTreeSet<String>,
    pub origin: Option<String>,
    pub work: Work,
    pub state: State,
    pub result: Option<String>,
    pub delivered: bool,
}

impl Job {
    pub fn key(&self) -> String {
        format!("job-{}", self.id)
    }

    pub fn name(&self) -> &str {
        match &self.work {
            Work::Specialist(specialist) => &specialist.role,
            Work::Tool(call) => &call.function.name,
        }
    }

    fn receipt(&self) -> Value {
        json!({
            "job":self.id, "state":self.state, "name":self.name(),
            "result":self.result,
        })
    }

    fn compact_receipt(&self, limit: usize) -> String {
        let mut receipt = self.receipt();
        let mut text = receipt.to_string();
        if let Some(result) = &self.result {
            let mut budget = limit.saturating_sub(text.len().saturating_sub(result.len()));
            while text.len() > limit && budget > 0 {
                receipt["result"] = json!(clip(result, budget));
                text = receipt.to_string();
                budget /= 2;
            }
            if text.len() > limit {
                receipt["result"] = Value::Null;
                text = receipt.to_string();
            }
        }
        text
    }

    fn observation(
        &self,
        state: &str,
        output: Option<(&str, &str)>,
        elapsed_ms: Option<u64>,
        redact: impl Fn(&str) -> String,
    ) -> Observation {
        let key = self.key();
        let (specialist, name, server, parameters) = match &self.work {
            Work::Specialist(data) => (
                Some(key.as_str()),
                data.name.as_str(),
                None,
                data.dialogue.task.clone(),
            ),
            Work::Tool(call) => {
                let args: Value =
                    serde_json::from_str(&call.function.arguments).unwrap_or_default();
                (
                    None,
                    self.name(),
                    args["server"].as_str().map(&redact),
                    call.function.arguments.clone(),
                )
            }
        };
        let kind = if state == "queued" {
            "tool"
        } else {
            "tool_result"
        };
        let audit_key = if state == "queued" {
            key.clone()
        } else {
            format!("{key}-{state}")
        };
        Observation {
            key: audit_key,
            kind,
            actor: self.owner.clone(),
            data: json!({
                "call":key, "tool":name, "name": match &self.work {
                    Work::Tool(call) if server.is_some() => serde_json::from_str::<Value>(&call.function.arguments)
                        .ok().and_then(|args| args["name"].as_str().map(&redact)),
                    _ => None,
                },
                "server":server, "turn":self.origin,
                "state":state, "job":self.id, "specialist":specialist,
                "arguments": (state == "queued").then(|| bounded(&redact(&parameters))),
                "elapsed_ms":elapsed_ms,
                "output":output.map(|(visible, _)| bounded(&redact(visible))),
                "full":output.map(|(_, full)| bounded(&redact(full))),
            }),
        }
    }
}

#[derive(Default, Deserialize, Serialize)]
struct Book {
    next: u64,
    jobs: BTreeMap<u64, Job>,
    #[serde(skip)]
    audit: Outbox,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub(crate) struct Jobs(Rc<RefCell<Book>>);

impl Jobs {
    pub fn is_empty(&self) -> bool {
        self.0.borrow().next == 0
    }

    pub fn submit(&self, mut job: Job, redact: impl Fn(&str) -> String) -> u64 {
        let mut book = self.0.borrow_mut();
        book.next += 1;
        job.id = book.next;
        book.audit
            .push(job.observation("queued", None, None, redact));
        book.jobs.insert(job.id, job);
        book.next
    }

    pub fn has_jobs(&self, actor: &str) -> bool {
        self.0.borrow().jobs.values().any(|job| job.owner == actor)
    }

    pub fn has_outstanding(&self, actor: &str) -> bool {
        self.0
            .borrow()
            .jobs
            .values()
            .any(|job| job.owner == actor && !job.state.terminal())
    }

    pub fn has_receipts(&self, actor: &str) -> bool {
        self.0
            .borrow()
            .jobs
            .values()
            .any(|job| job.owner == actor && job.state.terminal() && !job.delivered)
    }

    pub fn active(&self) -> bool {
        self.0
            .borrow()
            .jobs
            .values()
            .any(|job| !job.state.terminal())
    }

    pub fn blocked(&self, actor: &str, dialogue: &Dialogue) -> Result<bool> {
        let Some(call) = dialogue.pending()? else {
            return Ok(false);
        };
        if matches!(
            call.function.name.as_str(),
            "submit_plan" | "submit_verdict"
        ) {
            return Ok(self.has_outstanding(actor));
        }
        let book = self.0.borrow();
        if matches!(call.function.name.as_str(), "await_jobs" | "jobs") {
            return Ok(expand(call)
                .ok()
                .filter(|call| call.function.name == "await_jobs")
                .and_then(|call| tool_arguments::<Wait>(&call.function.arguments).ok())
                .is_some_and(|wait| {
                    owned(&book, actor, &wait.jobs).is_ok()
                        && wait.jobs.iter().any(|id| !book.jobs[id].state.terminal())
                }));
        }
        let writes = matches!(
            call.function.name.as_str(),
            "shell" | "edit_file" | "replace_text" | "create_file" | "mcp_call"
        );
        Ok(writes
            && book.jobs.values().any(|job| {
                job.owner == actor
                    && !job.state.terminal()
                    && matches!(&job.work, Work::Tool(call) if call.function.name == "shell")
            }))
    }

    pub fn reply(&self, call: &ToolCall, actor: &str) -> Result<String> {
        let book = self.0.borrow();
        if call.function.name == "job_status" {
            let input: Status = tool_arguments(&call.function.arguments)?;
            let job = book
                .jobs
                .get(&input.job)
                .filter(|job| job.owner == actor)
                .context("unknown job for this caller")?;
            return Ok(job.receipt().to_string());
        }
        let input: Wait = tool_arguments(&call.function.arguments)?;
        owned(&book, actor, &input.jobs)?;
        let rows = input
            .jobs
            .iter()
            .map(|id| {
                let job = &book.jobs[id];
                json!({"job":id,"state":job.state})
            })
            .collect::<Vec<_>>();
        Ok(json!({"jobs":rows}).to_string())
    }

    pub fn deliver(&self, actor: &str, dialogue: &mut Dialogue, limit: usize) -> bool {
        let mut book = self.0.borrow_mut();
        let book = &mut *book;
        let mut batch = String::new();
        for job in book
            .jobs
            .values_mut()
            .filter(|job| job.owner == actor && job.state.terminal() && !job.delivered)
        {
            let result = job.compact_receipt(limit.min(4 * 1024).saturating_sub(1));
            if !batch.is_empty() && batch.len() + result.len() + 1 > limit {
                break;
            }
            batch.push_str(&result);
            batch.push('\n');
            job.delivered = true;
            book.audit.push(Observation {
                key: format!("{}-delivered", job.key()),
                kind: "job_delivery",
                actor: actor.into(),
                data: json!({"job":job.id, "call":job.key(), "name":job.name()}),
            });
        }
        if batch.is_empty() {
            return false;
        }
        let mut retained = &dialogue.job_context[..];
        while retained.len() + batch.len() > limit {
            retained = retained.split_once('\n').map_or("", |(_, rest)| rest);
        }
        dialogue.job_context = format!("{retained}{batch}");
        true
    }

    pub fn next_tool(&self) -> Option<Job> {
        let book = self.0.borrow();
        book.jobs
            .values()
            .find(|job| {
                if job.state != State::Queued {
                    return false;
                }
                let Work::Tool(call) = &job.work else {
                    return false;
                };
                call.function.name != "shell" || !book.jobs.values().any(|other| {
                    other.owner == job.owner
                        && other.state == State::Running
                        && matches!(&other.work, Work::Tool(call) if call.function.name == "shell")
                })
            })
            .cloned()
    }

    pub fn ready_specialists(&self, running: &BTreeSet<u64>) -> Vec<u64> {
        self.0
            .borrow()
            .jobs
            .values()
            .filter(|job| {
                !running.contains(&job.id)
                    && !job.state.terminal()
                    && matches!(job.work, Work::Specialist(_))
            })
            .map(|job| job.id)
            .collect()
    }

    pub fn specialist(&self, id: u64) -> Job {
        self.0.borrow().jobs[&id].clone()
    }

    pub fn mark_running(&self, id: u64, redact: impl Fn(&str) -> String) {
        let mut book = self.0.borrow_mut();
        let book = &mut *book;
        let job = book.jobs.get_mut(&id).expect("scheduled job");
        if job.state == State::Queued {
            job.state = State::Running;
            book.audit
                .push(job.observation("running", None, None, redact));
        }
    }

    pub fn save_specialist(&self, id: u64, specialist: Specialist) {
        self.0
            .borrow_mut()
            .jobs
            .get_mut(&id)
            .expect("scheduled specialist")
            .work = Work::Specialist(Box::new(specialist));
    }

    pub fn defer(&self, id: u64) {
        self.0
            .borrow_mut()
            .jobs
            .get_mut(&id)
            .expect("scheduled tool")
            .state = State::Queued;
    }

    pub fn finish(
        &self,
        id: u64,
        state: State,
        output: (&str, &str),
        elapsed: Option<u64>,
        limit: usize,
        redact: impl Fn(&str) -> String,
    ) {
        let mut book = self.0.borrow_mut();
        let job = book.jobs.get_mut(&id).expect("completed job");
        job.state = state;
        job.result = Some(clip(&redact(output.0), limit.min(4 * 1024)));
        let observation = job.observation(state.label(), Some(output), elapsed, redact);
        book.audit.push(observation);
    }

    pub fn recover(&self, limit: usize) -> Result<()> {
        let uncertain = {
            let mut book = self.0.borrow_mut();
            let mut uncertain = Vec::new();
            for job in book
                .jobs
                .values_mut()
                .filter(|job| job.state == State::Running)
            {
                match &mut job.work {
                    Work::Tool(_) => uncertain.push(job.id),
                    Work::Specialist(specialist) => {
                        specialist.dialogue.recover_interrupted_tool()?;
                    }
                }
            }
            uncertain
        };
        for id in uncertain {
            let result = "Tool outcome unknown. The runner stopped after dispatch intent was committed. Verify its effects before issuing another call.";
            self.finish(
                id,
                State::Unknown,
                (result, result),
                None,
                limit,
                str::to_owned,
            );
        }
        Ok(())
    }

    pub fn checkpoint_exit(&self) {
        for job in self
            .0
            .borrow_mut()
            .jobs
            .values_mut()
            .filter(|job| !job.state.terminal())
        {
            if let Work::Specialist(specialist) = &mut job.work {
                specialist.dialogue.in_flight = false;
            }
        }
    }

    pub fn clear_phase(&self) {
        debug_assert!(!self.active());
        self.0.borrow_mut().jobs.clear();
    }

    pub fn for_each_dialogue(&self, mut visit: impl FnMut(&str, &Dialogue)) {
        for job in self.0.borrow().jobs.values() {
            if let Work::Specialist(specialist) = &job.work {
                visit(&job.key(), &specialist.dialogue);
            }
        }
    }

    pub fn audit(&self) -> Vec<Observation> {
        self.0.borrow().audit.pending()
    }

    pub fn committed(&self) {
        self.0.borrow().audit.committed();
    }
}

fn owned(book: &Book, actor: &str, ids: &BTreeSet<u64>) -> Result<()> {
    ensure!(!ids.is_empty(), "provide at least one job ID");
    ensure!(
        ids.iter()
            .all(|id| book.jobs.get(id).is_some_and(|job| job.owner == actor)),
        "unknown job for this caller"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell() -> Job {
        let submission: StartTool = serde_json::from_value(json!({
            "tool":"shell", "arguments":{"command":"echo fixture","workdir":null}
        }))
        .unwrap();
        Job {
            id: 0,
            owner: "worker_1".into(),
            policy_actor: "worker_1".into(),
            mcp_actor: "worker_1".into(),
            workspace: ".".into(),
            shared_candidate: false,
            access: Access::Work,
            permissions: Permissions::all(),
            ancestry: BTreeSet::new(),
            origin: None,
            work: Work::Tool(submission.call()),
            state: State::Queued,
            result: None,
            delivered: false,
        }
    }

    #[test]
    fn recovery_distinguishes_queued_dispatched_and_committed_jobs() {
        let book = Jobs::default();
        let queued = book.submit(shell(), str::to_owned);
        let dispatched = book.submit(shell(), str::to_owned);
        let completed = book.submit(shell(), str::to_owned);
        book.mark_running(dispatched, str::to_owned);
        book.mark_running(completed, str::to_owned);
        book.finish(
            completed,
            State::Completed,
            ("fixture result", "fixture result"),
            None,
            4096,
            str::to_owned,
        );

        let restore = |jobs: &Jobs| {
            serde_json::from_slice::<Jobs>(&serde_json::to_vec(jobs).unwrap()).unwrap()
        };
        let book = restore(&book);
        book.recover(4096).unwrap();
        assert_eq!(book.next_tool().unwrap().id, queued);
        assert!(matches!(
            book.0.borrow().jobs[&dispatched].state,
            State::Unknown
        ));
        assert!(matches!(
            book.0.borrow().jobs[&completed].state,
            State::Completed
        ));

        let mut caller = Dialogue::default();
        assert!(!book.deliver("worker_2", &mut caller, 4096));
        assert!(book.deliver("worker_1", &mut caller, 4096));
        assert!(caller.job_context.contains("outcome unknown"));
        assert!(caller.job_context.contains("fixture result"));
        assert!(!restore(&book).deliver("worker_1", &mut caller, 4096));
    }
}
