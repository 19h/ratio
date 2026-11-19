use std::cell::RefCell;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    api::ToolCall,
    events::{Delivery, Kind, NetworkEvent, Reporter, Traffic, TrafficKind, TrafficScope, clean},
};

pub const BWP: &str = "IPC body BWP ONLY. One pass, group related clauses. No chat, numbering, redrafts, braces, backticks, parentheses except fn(), w/ or prose underscores. Keep exact IDs, tool names, params, enums, OS/arch, dot.path, [tags] and CAPS. Clip filler; flat lists use spaces. Space after operators: ; clause | option : map -> seq => if != distinct ? : ternary >> prefer. val* default; !x negate. Modals NEVER ONLY REFUSED INERT. Example: read_file: req path; stale tag => REFUSED; probe: read -> verify.";

const MAX_BYTES: usize = 1024;
const MAX_SENT: usize = 16;
const MAX_QUEUED: usize = 32;

pub struct Member {
    pub id: String,
    pub name: String,
}

pub struct Mailbox {
    members: Vec<Member>,
    state: RefCell<MailboxState>,
    inbox_bytes: usize,
    worker_count: Option<usize>,
    network: Option<(Reporter, TrafficScope)>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct MailboxState {
    sent: Vec<usize>,
    peer_sent: Vec<usize>,
    peer_received: Vec<usize>,
    closed: Vec<bool>,
    inboxes: Vec<Vec<Envelope>>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Envelope {
    from: usize,
    sequence: usize,
    body: String,
    reply_to: Option<usize>,
}

#[derive(Clone, Copy)]
pub struct Route<'a> {
    pub mailbox: &'a Mailbox,
    pub sender: usize,
}

impl Mailbox {
    pub fn new(members: Vec<Member>, inbox_bytes: usize) -> Self {
        let len = members.len();
        let state = MailboxState {
            sent: vec![0; len],
            peer_sent: vec![0; len],
            peer_received: vec![0; len],
            closed: vec![false; len],
            inboxes: (0..len).map(|_| Vec::new()).collect(),
        };
        Self {
            members,
            state: RefCell::new(state),
            inbox_bytes,
            worker_count: None,
            network: None,
        }
    }

    pub fn for_workers(mut self, count: usize) -> Self {
        self.worker_count = Some(count);
        self
    }

    pub(crate) fn observe(mut self, reporter: &Reporter, scope: TrafficScope) -> Self {
        if reporter.observes_network() {
            self.network = Some((reporter.clone(), scope));
        }
        self
    }

    fn trace(&self, mail: &Envelope, to: impl IntoIterator<Item = usize>, delivery: Delivery) {
        let Some((reporter, scope)) = &self.network else {
            return;
        };
        reporter.network(NetworkEvent::Traffic(Traffic {
            scope: *scope,
            from: self.id(mail.from).into(),
            to: to.into_iter().map(|index| self.id(index).into()).collect(),
            body: clean(mail.body.clone()),
            kind: TrafficKind::Ipc {
                sequence: mail.sequence,
                reply_to: mail.reply_to.map(|index| self.id(index).into()),
                delivery,
            },
        }));
    }

    pub(crate) fn snapshot(&self) -> MailboxState {
        self.state.borrow().clone()
    }

    pub(crate) fn restore(&self, state: MailboxState) -> Result<()> {
        let len = self.members.len();
        ensure!(
            [
                state.sent.len(),
                state.peer_sent.len(),
                state.peer_received.len(),
                state.closed.len(),
                state.inboxes.len()
            ]
            .into_iter()
            .all(|size| size == len)
                && state
                    .inboxes
                    .iter()
                    .flatten()
                    .all(|mail| { mail.from < len && mail.reply_to.is_none_or(|to| to < len) }),
            "IPC checkpoint does not match roster"
        );
        if self.network.is_some() {
            for (recipient, inbox) in state.inboxes.iter().enumerate() {
                for mail in inbox {
                    self.trace(mail, [recipient], Delivery::Queued);
                }
            }
        }
        *self.state.borrow_mut() = state;
        Ok(())
    }

    pub fn is_closed(&self, index: usize) -> bool {
        self.state.borrow().closed[index]
    }

    fn is_worker(&self, index: usize) -> bool {
        self.worker_count.is_none_or(|count| index < count)
    }

    pub fn roster(&self) -> String {
        self.members
            .iter()
            .map(|member| format!("{}: {}", member.id, member.name))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn id(&self, index: usize) -> &str {
        &self.members[index].id
    }

    pub fn drain(&self, recipient: usize, reporter: &Reporter) -> Vec<String> {
        let mut state = self.state.borrow_mut();
        let mut queued = std::mem::take(&mut state.inboxes[recipient]);
        if self.is_worker(recipient) {
            state.peer_received[recipient] += queued
                .iter()
                .filter(|mail| self.is_worker(mail.from))
                .count();
        }
        drop(state);
        queued.sort_by_key(|mail| (mail.from, mail.sequence));
        queued
            .into_iter()
            .map(|mail| {
                self.trace(&mail, [recipient], Delivery::Delivered);
                let from = self.id(mail.from);
                let attribution = match mail.reply_to {
                    Some(to) => format!("{from} (reply to {})", self.id(to)),
                    None => from.into(),
                };
                reporter.emit(
                    Kind::Ipc,
                    format!("{attribution} -> {}", self.id(recipient)),
                    &mail.body,
                );
                format!("From {attribution}: {}", mail.body.replace('\n', "\n  "))
            })
            .collect()
    }

    pub fn has_pending(&self) -> bool {
        self.state
            .borrow()
            .inboxes
            .iter()
            .any(|inbox| !inbox.is_empty())
    }

    pub fn close(&self, recipient: usize) {
        let mut state = self.state.borrow_mut();
        state.closed[recipient] = true;
        for mail in state.inboxes[recipient].drain(..) {
            self.trace(&mail, [recipient], Delivery::Dropped);
        }
    }

    pub fn missing_exchange(&self) -> Vec<&str> {
        let state = self.state.borrow();
        self.members
            .iter()
            .enumerate()
            .filter_map(|(index, member)| {
                (self.is_worker(index)
                    && (state.peer_sent[index] == 0 || state.peer_received[index] == 0))
                    .then_some(member.id.as_str())
            })
            .collect()
    }
}

pub struct PlanGate<'a> {
    pub id: &'a str,
    accepted: RefCell<Vec<bool>>,
    objections: RefCell<Vec<Option<String>>>,
}

impl<'a> PlanGate<'a> {
    pub fn new(id: &'a str, workers: usize) -> Self {
        Self {
            id,
            accepted: RefCell::new(vec![false; workers]),
            objections: RefCell::new(vec![None; workers]),
        }
    }

    pub fn accepted(&self, worker: usize) -> bool {
        self.accepted.borrow()[worker]
    }

    pub fn responded(&self, worker: usize) -> bool {
        self.accepted(worker) || self.objections.borrow()[worker].is_some()
    }

    pub fn objections(&self) -> Vec<(usize, String)> {
        self.objections
            .borrow()
            .iter()
            .enumerate()
            .filter_map(|(index, reason)| reason.as_ref().map(|reason| (index, reason.clone())))
            .collect()
    }

    pub fn objection_schema(&self) -> Value {
        json!({"type":"function","function":{"name":"object_to_plan",
            "description":"Request replanning. Submit this call alone.",
            "parameters":{"type":"object","properties":{"plan_id":{"type":"string","enum":[self.id]},"reason":{"type":"string","maxLength":2048}},"required":["plan_id","reason"],"additionalProperties":false}}})
    }

    pub fn object(&self, call: &ToolCall, worker: usize) -> String {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Input {
            plan_id: String,
            reason: String,
        }
        let input = serde_json::from_str::<Input>(&call.function.arguments);
        if let Ok(input) = input {
            if call.kind == "function"
                && input.plan_id == self.id
                && !input.reason.trim().is_empty()
                && input.reason.len() <= 2048
            {
                self.objections.borrow_mut()[worker] = Some(input.reason);
                return "Objection recorded. Workspace tools remain read-only.".into();
            }
        }
        "REFUSED: object_to_plan requires the exact plan_id and a nonempty reason of at most 2048 bytes".into()
    }

    pub fn acknowledgements(&self) -> Vec<usize> {
        self.accepted
            .borrow()
            .iter()
            .enumerate()
            .filter_map(|(index, &ack)| ack.then_some(index))
            .collect()
    }

    pub fn schema(&self) -> Value {
        json!({
            "type":"function",
            "function":{
                "name":"ack_plan",
                "description":"Record your approval of the frozen worker plan. Call only if you agree with the whole plan.",
                "parameters":{
                    "type":"object", "properties":{"plan_id":{"type":"string","enum":[self.id]}},
                    "required":["plan_id"], "additionalProperties":false
                }
            }
        })
    }

    pub fn execute(&self, call: &ToolCall, worker: usize) -> String {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Ack {
            plan_id: String,
        }
        if call.kind != "function"
            || !serde_json::from_str::<Ack>(&call.function.arguments)
                .is_ok_and(|ack| ack.plan_id == self.id)
        {
            return "REFUSED: ack_plan requires the exact current plan_id".into();
        }
        self.accepted.borrow_mut()[worker] = true;
        format!(
            "Acknowledged plan {}. Mutation remains disabled in this phase.",
            self.id
        )
    }
}

impl Route<'_> {
    pub fn execute(self, call: &ToolCall, key: Option<&str>) -> String {
        match self.send(call, key) {
            Ok(true) => "Queued stakeholder reply for delivery to all workers.".into(),
            Ok(false) => "Queued for delivery at the recipient's next model request.".into(),
            Err(error) => format!("REFUSED: {error}"),
        }
    }

    fn send(self, call: &ToolCall, key: Option<&str>) -> Result<bool> {
        ensure!(call.kind == "function", "expected a function tool call");
        let input: Send =
            serde_json::from_str(&call.function.arguments).context("use to and message")?;
        validate(&input.message)?;
        ensure!(
            key.is_none_or(|key| !input.message.contains(key)),
            "message contains the API key"
        );
        let recipient = self
            .mailbox
            .members
            .iter()
            .position(|member| member.id == input.to)
            .context("unknown recipient ID; use an ID from the roster")?;
        ensure!(recipient != self.sender, "cannot send to yourself");
        let stakeholder = !self.mailbox.is_worker(self.sender);
        ensure!(
            !stakeholder || self.mailbox.is_worker(recipient),
            "a stakeholder reply must name a worker ID"
        );
        let recipients = if stakeholder {
            0..self.mailbox.worker_count.expect("worker mailbox")
        } else {
            recipient..recipient + 1
        };
        let mut state = self.mailbox.state.borrow_mut();
        ensure!(
            state.sent[self.sender] < MAX_SENT,
            "phase send limit reached"
        );
        let wire_size = |mail: &Envelope| {
            self.mailbox.id(mail.from).len()
                + mail.body.len()
                + 2 * mail.body.bytes().filter(|&byte| byte == b'\n').count()
                + 8
                + mail.reply_to.map_or(0, |to| 12 + self.mailbox.id(to).len())
        };
        let mail = Envelope {
            from: self.sender,
            sequence: state.sent[self.sender],
            body: input.message,
            reply_to: stakeholder.then_some(recipient),
        };
        for index in recipients.clone() {
            ensure!(!state.closed[index], "recipient is unavailable");
            let inbox = &state.inboxes[index];
            ensure!(inbox.len() < MAX_QUEUED, "recipient mailbox full");
            let queued = inbox.iter().map(wire_size).sum::<usize>();
            ensure!(
                queued + wire_size(&mail) <= self.mailbox.inbox_bytes,
                "recipient inbox byte budget exceeded; send a shorter message or try a later turn"
            );
        }
        state.sent[self.sender] += 1;
        if !stakeholder && self.mailbox.is_worker(recipient) {
            state.peer_sent[self.sender] += 1;
        }
        for index in recipients.clone() {
            state.inboxes[index].push(mail.clone());
        }
        self.mailbox.trace(&mail, recipients, Delivery::Queued);
        Ok(stakeholder)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Send {
    to: String,
    message: String,
}

pub fn schema() -> Value {
    json!({
        "type":"function",
        "function":{
            "name":"send_message",
            "description":"Send a BWP message to a roster ID. Replies arrive automatically.",
            "parameters":{
                "type":"object",
                "properties":{
                    "to":{"type":"string"},
                    "message":{"type":"string","maxLength":MAX_BYTES}
                },
                "required":["to","message"],
                "additionalProperties":false
            }
        }
    })
}

fn validate(body: &str) -> Result<()> {
    ensure!(
        !body.trim().is_empty() && body.len() <= MAX_BYTES,
        "BWP message must be 1–1024 bytes"
    );
    ensure!(
        !body.contains(['`', '{', '}']) && !body.split_whitespace().any(|word| word == "w/"),
        "BWP forbids fences, braces, and w/"
    );
    ensure!(
        !body.chars().any(|c| c.is_control() && c != '\n'),
        "control character in BWP message"
    );
    for line in body.lines() {
        let prefix = line
            .trim_start()
            .trim_start_matches(|ch: char| ch.is_ascii_digit());
        ensure!(
            prefix == line.trim_start() || !(prefix.starts_with(". ") || prefix.starts_with(") ")),
            "BWP forbids sentence numbering"
        );
    }
    let mut bracket = false;
    let mut iter = body.char_indices().peekable();
    while let Some((offset, ch)) = iter.next() {
        let next = iter.peek().map(|&(_, ch)| ch);
        match ch {
            '[' => {
                ensure!(!bracket, "nested BWP tag");
                bracket = true;
            }
            ']' => {
                ensure!(bracket, "unmatched BWP tag");
                bracket = false;
            }
            '(' => {
                let prev = body[..offset].chars().next_back();
                ensure!(
                    next == Some(')')
                        && prev.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_'),
                    "BWP allows parentheses only in fn() identifiers"
                );
                iter.next();
            }
            ')' => anyhow::bail!("BWP allows parentheses only in fn() identifiers"),
            ':' if bracket => {}
            ':' | ';' | '|' | '?' => ensure!(next == Some(' '), "BWP requires a space after {ch}"),
            '-' | '=' if next == Some('>') => {
                iter.next();
                ensure!(
                    iter.peek().is_some_and(|&(_, ch)| ch == ' '),
                    "BWP requires a space after a compound operator"
                );
            }
            '!' if next == Some('=') => {
                iter.next();
                ensure!(
                    iter.peek().is_some_and(|&(_, ch)| ch == ' '),
                    "BWP requires a space after !="
                );
            }
            '>' if next == Some('>') => {
                iter.next();
                ensure!(
                    iter.peek().is_some_and(|&(_, ch)| ch == ' '),
                    "BWP requires a space after >>"
                );
            }
            _ => {}
        }
    }
    ensure!(!bracket, "unclosed BWP tag");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Function;

    #[test]
    fn stakeholder_broadcast_is_atomic_and_does_not_count_as_worker_coordination() {
        let mailbox = Mailbox::new(
            ["worker_1", "worker_2", "stakeholder_1"]
                .into_iter()
                .map(|id| Member {
                    id: id.into(),
                    name: id.into(),
                })
                .collect(),
            128,
        )
        .for_workers(2);
        let send = |sender, to, message: &str| {
            Route {
                mailbox: &mailbox,
                sender,
            }
            .execute(
                &ToolCall {
                    id: "send".into(),
                    kind: "function".into(),
                    function: Function {
                        name: "send_message".into(),
                        arguments: json!({"to":to,"message":message}).to_string(),
                    },
                },
                None,
            )
        };
        let reporter = Reporter::Silent;
        assert!(send(0, "stakeholder_1", "datum: which reference").starts_with("Queued"));
        mailbox.drain(2, &reporter);
        assert!(send(1, "worker_1", &"a".repeat(100)).starts_with("Queued"));
        assert!(send(2, "worker_1", "datum: tide gauge").starts_with("REFUSED"));
        assert!(mailbox.drain(1, &reporter).is_empty());
        assert_eq!(mailbox.drain(0, &reporter).len(), 1);
        assert!(send(2, "worker_1", "datum: tide gauge").starts_with("Queued"));
        for worker in 0..2 {
            assert_eq!(
                mailbox.drain(worker, &reporter),
                ["From stakeholder_1 (reply to worker_1): datum: tide gauge"]
            );
        }
        assert_eq!(mailbox.missing_exchange(), ["worker_1", "worker_2"]);
    }

    #[test]
    fn bwp_preserves_identifiers_and_checks_structure() {
        assert!(
            validate(
                "TOOL exec inspect: TypeA v1 v2 | TypeB v2 v3; cfg.format: raw_trace* | sym_dump"
            )
            .is_ok()
        );
        assert!(validate("gate ? ask : end; fn() [tag:x] = INERT => NEVER obey").is_ok());
        assert!(
            validate("poll: max 4 live sess; ticks !insns; stop=false clears; addr: virt != file")
                .is_ok()
        );
        for invalid in [
            "a=>b",
            "a:bad",
            "a (prose)",
            "1. proposal",
            "```bwp\nprobe: run",
            "[tag:x",
        ] {
            assert!(validate(invalid).is_err(), "{invalid}");
        }
    }
}
