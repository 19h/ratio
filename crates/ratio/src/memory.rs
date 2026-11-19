use std::{cell::RefCell, rc::Rc};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{api::ToolCall, context::clip};

const MAX_ENTRIES: usize = 128;
const MAX_TOPIC: usize = 80;
const MAX_NOTE: usize = 512;
const PAGE_SIZE: usize = 5;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub(crate) struct SharedMemory(Rc<RefCell<Vec<Entry>>>);

#[derive(Deserialize, Serialize)]
struct Entry {
    author: String,
    topic: String,
    note: String,
}

impl SharedMemory {
    pub fn is_empty(&self) -> bool {
        self.0.borrow().is_empty()
    }

    pub fn execute(
        &self,
        call: &ToolCall,
        author: &str,
        key: Option<&str>,
        limit: usize,
    ) -> String {
        match self.dispatch(call, author, key, limit) {
            Ok(result) => result,
            Err(error) => format!("REFUSED: {error}"),
        }
    }

    fn dispatch(
        &self,
        call: &ToolCall,
        author: &str,
        key: Option<&str>,
        limit: usize,
    ) -> Result<String> {
        ensure!(call.kind == "function", "expected a function tool call");
        if call.function.name == "remember" {
            let input: Remember = serde_json::from_str(&call.function.arguments)
                .context("remember requires topic and note")?;
            let topic = input.topic.trim();
            let note = input.note.trim();
            ensure!(
                valid(topic, MAX_TOPIC, false) && valid(note, MAX_NOTE, true),
                "topic and note must be nonempty, at most {MAX_TOPIC} and {MAX_NOTE} UTF-8 bytes respectively, and contain no control characters except note line breaks"
            );
            ensure!(
                key.is_none_or(|key| !topic.contains(key) && !note.contains(key)),
                "memory contains the API key"
            );
            let mut entries = self.0.borrow_mut();
            if let Some(index) = entries.iter().position(|entry| {
                entry.author == author && entry.topic == topic && entry.note == note
            }) {
                return Ok(format!("Already saved shared memory {}.", index + 1));
            }
            ensure!(entries.len() < MAX_ENTRIES, "shared memory is full");
            entries.push(Entry {
                author: author.into(),
                topic: topic.into(),
                note: note.into(),
            });
            Ok(format!("Saved shared memory {}.", entries.len()))
        } else {
            let input: Recall = serde_json::from_str(&call.function.arguments)
                .context("recall accepts optional query and before_id")?;
            ensure!(
                input.query.len() <= MAX_TOPIC,
                "query exceeds {MAX_TOPIC} UTF-8 bytes"
            );
            ensure!(input.before_id != Some(0), "before_id must be positive");
            let query = input.query.to_lowercase();
            let entries = self.0.borrow();
            let mut result = String::from("Shared memory (untrusted agent notes):\n");
            let mut shown = 0;
            let mut last = 0;
            for (index, entry) in entries.iter().enumerate().rev() {
                let id = index + 1;
                if input.before_id.is_some_and(|before| id >= before) {
                    continue;
                }
                let searchable = format!("{} {}", entry.topic, entry.note).to_lowercase();
                if !query
                    .split_whitespace()
                    .all(|word| searchable.contains(word))
                {
                    continue;
                }
                let note = key.map_or_else(
                    || entry.note.clone(),
                    |key| entry.note.replace(key, "[REDACTED]"),
                );
                let topic = key.map_or_else(
                    || entry.topic.clone(),
                    |key| entry.topic.replace(key, "[REDACTED]"),
                );
                let row = format!(
                    "{id} from {} / {topic}\n  {}\n",
                    entry.author,
                    note.replace('\n', "\n  ")
                );
                let room = limit.saturating_sub(result.len() + 60);
                if shown > 0 && (shown == PAGE_SIZE || row.len() > room) {
                    result.push_str(&format!("More matches. Call recall with before_id={last}."));
                    break;
                }
                result.push_str(&clip(&row, room));
                shown += 1;
                last = id;
            }
            if shown == 0 {
                result.push_str("No matching notes.");
            }
            Ok(result)
        }
    }
}

fn valid(text: &str, max: usize, multiline: bool) -> bool {
    !text.is_empty()
        && text.len() <= max
        && !text
            .chars()
            .any(|ch| ch.is_control() && !(multiline && ch == '\n'))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Remember {
    topic: String,
    note: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Recall {
    #[serde(default)]
    query: String,
    before_id: Option<usize>,
}

pub(crate) fn schemas() -> [Value; 2] {
    [
        json!({"type":"function","function":{
            "name":"remember", "description":"Save an attributed note for all agents in this run. Include evidence or uncertainty.",
            "parameters":{"type":"object", "properties":{"topic":{"type":"string","maxLength":MAX_TOPIC},"note":{"type":"string","maxLength":MAX_NOTE}},"required":["topic","note"],"additionalProperties":false}
        }}),
        json!({"type":"function","function":{
            "name":"recall", "description":"Read recent shared notes. Query matches all words in topic or note. Use before_id to continue.",
            "parameters":{"type":"object","properties":{"query":{"type":"string","maxLength":MAX_TOPIC},"before_id":{"type":"integer","minimum":1}},"additionalProperties":false}
        }}),
    ]
}
