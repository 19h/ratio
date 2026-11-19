use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

#[derive(Clone, Debug)]
pub(crate) struct Observation {
    pub key: String,
    pub kind: &'static str,
    pub actor: String,
    pub data: Value,
}

#[derive(Clone, Default)]
pub(crate) struct Outbox(RefCell<Vec<Rc<OutboxEntry>>>);

struct OutboxEntry {
    observation: Observation,
    committed: Cell<bool>,
}

impl Outbox {
    pub fn push(&self, observation: Observation) {
        let mut queue = self.0.borrow_mut();
        queue.retain(|entry| !entry.committed.get());
        queue.push(Rc::new(OutboxEntry {
            observation,
            committed: Cell::new(false),
        }));
    }

    pub fn pending(&self) -> Vec<Observation> {
        self.0
            .borrow()
            .iter()
            .filter(|entry| !entry.committed.get())
            .map(|entry| entry.observation.clone())
            .collect()
    }

    pub fn committed(&self) {
        for entry in self.0.borrow_mut().drain(..) {
            entry.committed.set(true);
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AuditEvent {
    pub sequence: u64,
    pub checkpoint: u64,
    pub timestamp_ms: u64,
    pub phase: String,
    pub cycle: usize,
    pub key: String,
    pub kind: String,
    pub actor: String,
    pub data: Value,
}

#[derive(Default)]
pub struct AuditFilter<'a> {
    pub before: Option<u64>,
    pub after: Option<u64>,
    pub actor: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub cycle: Option<usize>,
    pub phase: Option<&'a str>,
    pub query: Option<&'a str>,
    pub from: Option<&'a str>,
    pub to: Option<&'a str>,
    pub worker_request: Option<i64>,
    pub path: Option<&'a str>,
}

pub(crate) fn bounded(text: &str) -> Value {
    let limit = 8 * 1024 * 1024;
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    serde_json::json!({"text": &text[..end], "bytes": text.len(), "omitted": text.len() - end})
}
