use std::collections::BTreeMap;

use anyhow::{Context, Result};
use ratio_protocol::{Delivery, Effect, EffectState, Evaluation, IpcEvent, RunEvent, RunMetrics};
use serde_json::{Value, json};

use crate::RunReader;

impl RunReader {
    pub fn metrics(&self, run_id: &str) -> Result<RunMetrics> {
        let run = self.run(run_id)?.context("unknown run")?;
        let mut metrics = RunMetrics::default();
        let mut after = 0;
        while self.advance_metrics(run_id, &mut after, run.last_sequence, &mut metrics, 64)? {}
        self.refresh_metric_budget(run_id, &mut metrics)?;
        Ok(metrics)
    }

    pub fn advance_metrics(
        &self,
        run: &str,
        after: &mut u64,
        through: u64,
        metrics: &mut RunMetrics,
        limit: u32,
    ) -> Result<bool> {
        let limit = limit.clamp(1, 256);
        let events = self.metric_events(run, *after, through, limit)?;
        let more = events.len() == limit as usize;
        for event in events {
            count_metrics(metrics, &event.effects, &event.messages);
            *after = event.sequence;
        }
        Ok(more)
    }

    pub fn refresh_metric_budget(&self, run: &str, metrics: &mut RunMetrics) -> Result<()> {
        let ledger = self.ledger(run)?;
        metrics.provider_attempts = ledger.requests;
        metrics.metered_tokens = ledger.tokens;
        metrics.token_breakdown = ledger.token_breakdown(0);
        metrics.active_ms = ledger.elapsed_ms;
        Ok(())
    }

    pub fn evaluate(&self, run_id: &str, rules: &Evaluation) -> Result<Value> {
        let event = self.latest_event(run_id)?.context("unknown run")?;
        let metrics = self.metrics(run_id)?;
        let passed_verification = event
            .workflow
            .verification
            .as_ref()
            .is_some_and(|receipt| receipt.passed);
        let checks = [
            ("phase", rules.phase == event.phase),
            (
                "verified_candidate",
                !rules.require_verified_candidate || passed_verification,
            ),
            (
                "min_ipc_deliveries",
                metrics.ipc_deliveries >= rules.min_ipc_deliveries,
            ),
            (
                "max_unknown_tool_outcomes",
                metrics.unknown_tool_outcomes <= rules.max_unknown_tool_outcomes,
            ),
            (
                "max_provider_attempts",
                rules
                    .max_provider_attempts
                    .is_none_or(|max| metrics.provider_attempts <= max),
            ),
            (
                "max_metered_tokens",
                rules
                    .max_metered_tokens
                    .is_none_or(|max| metrics.metered_tokens <= max),
            ),
            (
                "max_active_ms",
                rules
                    .max_active_ms
                    .is_none_or(|max| metrics.active_ms <= max),
            ),
        ];
        let failed: Vec<_> = checks
            .into_iter()
            .filter_map(|(name, passed)| (!passed).then_some(name))
            .collect();
        Ok(
            json!({"run_id":run_id, "phase":event.phase, "passed":failed.is_empty(),
            "failed":failed, "metrics":metrics}),
        )
    }

    pub fn otlp_trace(&self, run_id: &str) -> Result<Value> {
        let run = self.run(run_id)?.context("unknown run")?;
        let trace_id = identifier(run_id, 16);
        let parent = identifier(&format!("{run_id}:run"), 8);
        let mut operations = BTreeMap::new();
        let mut spans = Vec::new();
        let mut milestones = Vec::new();
        let mut previous = (String::new(), 0);
        let mut range = None::<(u64, u64)>;
        self.walk_events(run_id, |event| {
            let time = event.timestamp_ms.saturating_mul(1_000_000);
            let (first, last) = range.get_or_insert((time, time));
            *first = (*first).min(time);
            *last = (*last).max(time);
            if previous.0 != event.phase || previous.1 != event.cycle {
                milestones.push(json!({
                    "timeUnixNano": time.to_string(), "name": "ratio.phase",
                    "attributes":[attribute("ratio.phase", &event.phase), attribute("ratio.cycle", event.cycle.to_string())]
                }));
                previous = (event.phase.clone(), event.cycle);
            }
            for effect in &event.effects {
                let key = format!("{}:{}:{}:{}:{}:{}", event.phase, event.cycle, effect.actor, effect.attempt, effect.round, effect.index);
                if effect.state == EffectState::Intended {
                    operations.entry(key).or_insert(time);
                    continue;
                }
                let begin = operations.remove(&key).unwrap_or(time).min(time);
                let span_id = identifier(&format!("{run_id}:{key}"), 8);
                let outcome = if effect.state == EffectState::Recorded { "recorded" } else { "unknown" };
                spans.push(json!({
                    "traceId":trace_id,"spanId":span_id,"parentSpanId":parent,
                    "name":"ratio.tool","kind":1,"startTimeUnixNano":begin.to_string(),"endTimeUnixNano":time.to_string(),
                    "attributes":[attribute("ratio.phase",&event.phase),attribute("ratio.actor",&effect.actor),
                        attribute("ratio.tool",&effect.tool),attribute("ratio.outcome",outcome)]
                }));
            }
        })?;
        let (begin, end) = range.context("run has no committed checkpoints")?;
        spans.insert(0, json!({
            "traceId":trace_id,"spanId":parent,"name":"ratio.run","kind":1,
            "startTimeUnixNano":begin.to_string(),"endTimeUnixNano":end.to_string(),
            "attributes":[attribute("ratio.run_id",run_id),attribute("ratio.last_phase", &run.phase),
                attribute("ratio.open_effects",operations.len().to_string())],
            "events":milestones
        }));
        Ok(
            json!({"resourceSpans":[{"resource":{"attributes":[attribute("service.name","ratio")]},
            "scopeSpans":[{"scope":{"name":"ratio.journal"},"spans":spans}]}]}),
        )
    }

    fn walk_events(&self, run_id: &str, mut visit: impl FnMut(&RunEvent)) -> Result<()> {
        let mut after = 0;
        loop {
            let events = self.events(run_id, after, 64)?;
            if events.is_empty() {
                return Ok(());
            }
            for event in events {
                after = event.sequence;
                visit(&event);
            }
        }
    }
}

fn count_metrics(metrics: &mut RunMetrics, effects: &[Effect], messages: &[IpcEvent]) {
    for effect in effects {
        match effect.state {
            EffectState::Intended => metrics.tool_intents += 1,
            EffectState::Recorded => metrics.tool_receipts += 1,
            EffectState::Unknown => metrics.unknown_tool_outcomes += 1,
        }
    }
    for message in messages {
        match message.delivery {
            Delivery::Queued => {}
            Delivery::Delivered => metrics.ipc_deliveries += message.to.len(),
            Delivery::Dropped => metrics.ipc_drops += message.to.len(),
        }
    }
}

fn attribute(key: &str, value: impl AsRef<str>) -> Value {
    json!({"key":key,"value":{"stringValue":value.as_ref()}})
}

fn identifier(key: &str, bytes: usize) -> String {
    ring::digest::digest(&ring::digest::SHA256, key.as_bytes()).as_ref()[..bytes]
        .iter()
        .map(|value| format!("{value:02x}"))
        .collect()
}
