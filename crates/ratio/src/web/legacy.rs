use anyhow::{Context, Result};
use ratio_engine::{RunReader, audit::AuditEvent};
use ratio_protocol::{EffectState, RunEvent};
use serde_json::{Value, json};

use super::Query;

pub(super) fn history(reader: &RunReader, run: &str, query: &Query) -> Result<Value> {
    let before = query.get("before").and_then(|value| value.parse().ok());
    let events = reader.events_before(run, before, 25)?;
    let matches = |item: &AuditEvent| {
        let get = |key| {
            query
                .get(key)
                .filter(|value| !value.is_empty())
                .map(String::as_str)
        };
        get("actor").is_none_or(|id| item.actor == id || value_recipients(&item.data, id))
            && get("kind").is_none_or(|kind| {
                item.kind == kind
                    || (kind == "mcp"
                        && matches!(
                            item.data["tool"].as_str(),
                            Some("mcp_call" | "mcp_list" | "tool_search")
                        ))
            })
            && get("phase").is_none_or(|phase| item.phase == phase)
            && get("cycle")
                .and_then(|cycle| cycle.parse::<usize>().ok())
                .is_none_or(|cycle| item.cycle == cycle)
            && get("from").is_none_or(|id| id == item.actor)
            && get("to").is_none_or(|id| value_recipients(&item.data, id))
            && get("path").is_none_or(|path| {
                item.data["path"]
                    .as_str()
                    .is_some_and(|value| path == "*" || value == path)
            })
            && get("q").is_none_or(|q| {
                format!("{} {}", item.actor, item.data)
                    .to_lowercase()
                    .contains(&q.to_lowercase())
            })
    };
    let mut rows = Vec::new();
    for event in &events {
        for (index, ipc) in event.ipc.iter().enumerate().rev() {
            let item = observation(
                event,
                format!("legacy-{}-ipc-{index}", event.sequence),
                "ipc",
                &ipc.from,
                serde_json::to_value(ipc)?,
            );
            if matches(&item) {
                rows.push(item);
            }
        }
        if !event.effects.is_empty() {
            let checkpoint = snapshot(reader, run, event.sequence)?;
            for index in (0..event.effects.len()).rev() {
                if let Some(item) = effect(event, &checkpoint, index) {
                    if matches(&item) {
                        rows.push(item);
                    }
                }
            }
        }
    }
    for row in &mut rows {
        if let Some(fields) = row.data.as_object_mut() {
            fields.remove("arguments");
            fields.remove("output");
        }
    }
    let more = (events.len() == 25)
        .then(|| events.last().map(|event| event.sequence))
        .flatten();
    Ok(json!({"rows":rows, "legacy":true,"next_before":more}))
}

pub(super) fn entry(reader: &RunReader, run: &str, key: &str) -> Result<Value> {
    let tokens = key.split('-').collect::<Vec<_>>();
    let [_, sequence, kind, index] = tokens.as_slice() else {
        anyhow::bail!("Invalid historical link")
    };
    let sequence = sequence.parse()?;
    let index: usize = index.parse()?;
    let event = reader
        .event_at(run, sequence)?
        .context("Historical checkpoint not found")?;
    if *kind == "ipc" {
        let ipc = event
            .ipc
            .get(index)
            .context("Historical IPC entry not found")?;
        return Ok(
            json!({"event":observation(&event,key.into(),"ipc",&ipc.from,serde_json::to_value(ipc)?)}),
        );
    }
    let snapshot = snapshot(reader, run, sequence)?;
    let tool = effect(&event, &snapshot, index).context("Historical tool arguments unavailable")?;
    if *kind == "chat" {
        let actor = &event.effects[index].actor;
        let group = group(&snapshot, actor, event.effects[index].round)
            .context("Historical turn unavailable")?;
        let heading = &group[0];
        let calls = heading["tool_calls"].as_array().into_iter().flatten().enumerate().filter_map(|(call, raw)| {
            let effect_index = event.effects.iter().position(|effect| effect.actor == *actor && effect.round == event.effects[index].round && effect.index == call)?;
            Some(json!({"key":format!("legacy-{sequence}-tool-{effect_index}"),"tool":raw["function"]["name"]}))
        }).collect::<Vec<_>>();
        return Ok(
            json!({"event":observation(&event,key.into(),"chat",&tool.actor,json!({
                "text":heading["content"],"calls":calls,"legacy":true
            }))}),
        );
    }
    let output = tool.data["output"].clone();
    let state = tool.data["state"].clone();
    Ok(json!({"event":tool,"outcome":{"data":{"state":state,"output":output}}}))
}

fn snapshot(reader: &RunReader, run: &str, sequence: u64) -> Result<Value> {
    Ok(serde_json::from_slice(
        &reader
            .snapshot(run, Some(sequence))?
            .context("Historical checkpoint unavailable")?,
    )?)
}

fn value_recipients(value: &Value, id: &str) -> bool {
    value["to"]
        .as_array()
        .is_some_and(|agents| agents.iter().any(|agent| agent == id))
}

fn observation(event: &RunEvent, key: String, kind: &str, actor: &str, data: Value) -> AuditEvent {
    AuditEvent {
        sequence: event.sequence,
        checkpoint: event.sequence,
        timestamp_ms: event.timestamp_ms,
        phase: event.phase.clone(),
        cycle: event.cycle,
        key,
        kind: kind.into(),
        actor: actor.into(),
        data,
    }
}

fn effect(event: &RunEvent, snapshot: &Value, index: usize) -> Option<AuditEvent> {
    let effect = event.effects.get(index)?;
    let actor = if let Some(index) = slot(&effect.actor, "panel_") {
        snapshot["panel"]["actor_ids"][index]
            .as_str()
            .unwrap_or(&effect.actor)
    } else {
        &effect.actor
    };
    let group = group(snapshot, &effect.actor, effect.round)?;
    let call = &group.first()?["tool_calls"][effect.index];
    let arguments = call["function"]["arguments"].as_str().unwrap_or_default();
    let parsed = serde_json::from_str::<Value>(arguments).unwrap_or_default();
    let output = group
        .get(effect.index + 1)
        .and_then(|value| value["content"].as_str());
    let state = match effect.state {
        EffectState::Recorded => "completed",
        EffectState::Intended => "running",
        EffectState::Unknown => "unknown",
    };
    Some(observation(
        event,
        format!("legacy-{}-tool-{index}", event.sequence),
        "tool",
        actor,
        json!({
            "legacy":true,"tool":effect.tool,"server":parsed["server"],"name":parsed["name"],"path":parsed["path"],
            "arguments":{"text":arguments,"bytes":arguments.len()},"output":output.map(|text| json!({"text":text,"bytes":text.len()})),
            "state":state,"turn":format!("legacy-{}-chat-{index}",event.sequence)
        }),
    ))
}

fn slot(actor: &str, prefix: &str) -> Option<usize> {
    actor
        .strip_prefix(prefix)?
        .parse::<usize>()
        .ok()?
        .checked_sub(1)
}

fn group<'a>(snapshot: &'a Value, actor: &str, round: usize) -> Option<&'a Vec<Value>> {
    let dialogue = if let Some(index) = slot(actor, "panel_") {
        &snapshot["panel"]["agents"][index]["dialogue"]
    } else if let Some(index) = slot(actor, "stakeholder_") {
        &snapshot["team_ipc"]["stakeholders"][index]["dialogue"]
    } else if let Some(index) = slot(actor, "worker_") {
        snapshot["worker_progress"]
            .get(index)
            .map_or(&snapshot["dialogue"], |worker| &worker["dialogue"])
    } else {
        &snapshot["dialogue"]
    };
    let groups = dialogue["groups"].as_array()?;
    let last = usize::try_from(dialogue["tool_rounds"].as_u64()?).ok()?;
    let relative = last.checked_sub(round)?;
    groups
        .get(groups.len().checked_sub(relative + 1)?)?
        .as_array()
}
