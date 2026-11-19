use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::Value;

use super::{Choice, Function, Message, Response, ToolCall, Usage, responses};
use crate::config::ProviderApi;

#[derive(Default)]
pub(super) struct Decoder {
    api: ProviderApi,
    line: Vec<u8>,
    data: Vec<u8>,
    skip_lf: bool,
    started: bool,
    done: bool,
    choice_seen: bool,
    provider_error: Option<Value>,
    content: Option<String>,
    reasoning: Option<String>,
    calls: BTreeMap<usize, ToolCall>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    terminal: Option<Value>,
    response_progress: Option<&'static str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_utf8_and_multiline_events_across_every_byte_boundary() {
        let fixture = concat!(
            "\u{feff}: heartbeat\r\n\r\n",
            "data: {\"choices\":[\r",
            "data: {\"delta\":{\"content\":\"λ\"},\"finish_reason\":\"stop\"}]}\r\r",
            "data: {\"choices\":null,\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1}}\n\n",
            "data: [DONE]\r\n\r\n",
        );
        let mut decoder = Decoder::default();
        for byte in fixture.as_bytes() {
            decoder.push(std::slice::from_ref(byte)).unwrap();
            if decoder.done() {
                break;
            }
        }
        let response = decoder.finish().unwrap();
        assert_eq!(response.usage.unwrap().total(), Some(3));
        assert!(matches!(
            &response.choices[0].message,
            Message::Assistant { content: Some(text), .. } if text == "λ"
        ));
    }
}

#[derive(Deserialize)]
struct Packet {
    choices: Option<Vec<StreamChoice>>,
    usage: Option<Usage>,
    error: Option<Value>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    delta: Option<Delta>,
    finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
struct Delta {
    role: Option<String>,
    content: Option<String>,
    #[serde(alias = "reasoning")]
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<CallDelta>>,
}

#[derive(Deserialize)]
struct CallDelta {
    index: usize,
    id: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    function: Option<FunctionDelta>,
}

#[derive(Deserialize)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

impl Decoder {
    pub(super) fn new(api: ProviderApi) -> Self {
        Self {
            api,
            ..Self::default()
        }
    }

    pub(super) fn push(&mut self, chunk: &[u8]) -> Result<()> {
        for &byte in chunk {
            if std::mem::take(&mut self.skip_lf) && byte == b'\n' {
                continue;
            }
            if matches!(byte, b'\r' | b'\n') {
                self.skip_lf = byte == b'\r';
                self.line()?;
                self.line.clear();
                if self.done() {
                    break;
                }
            } else {
                self.line.push(byte);
            }
        }
        Ok(())
    }

    fn line(&mut self) -> Result<()> {
        let mut line = self.line.as_slice();
        if !std::mem::replace(&mut self.started, true) {
            line = line.strip_prefix(b"\xef\xbb\xbf").unwrap_or(line);
        }
        if line.is_empty() {
            return self.dispatch();
        }
        let field_end = line
            .iter()
            .position(|&byte| byte == b':')
            .unwrap_or(line.len());
        if &line[..field_end] == b"data" {
            let value = line.get(field_end + 1..).unwrap_or_default();
            self.data
                .extend_from_slice(value.strip_prefix(b" ").unwrap_or(value));
            self.data.push(b'\n');
        }
        Ok(())
    }

    fn dispatch(&mut self) -> Result<()> {
        if self.data.is_empty() {
            return Ok(());
        }
        self.data.pop(); // Remove the final SSE field separator.
        if self.data == b"[DONE]" {
            self.done = true;
            self.data.clear();
            return Ok(());
        }
        if self.api == ProviderApi::Responses {
            let packet =
                serde_json::from_slice(&self.data).context("decode Responses stream event")?;
            self.data.clear();
            return self.responses_event(packet);
        }
        let packet: Packet =
            serde_json::from_slice(&self.data).context("decode chat completion stream event")?;
        self.data.clear();
        if let Some(usage) = packet.usage {
            self.usage = Some(usage);
        }
        if let Some(error) = packet.error {
            self.provider_error = Some(error);
            return Ok(());
        }
        for choice in packet
            .choices
            .into_iter()
            .flatten()
            .filter(|choice| choice.index == 0)
        {
            self.choice_seen = true;
            let delta = choice.delta.unwrap_or_default();
            ensure!(
                delta.role.as_deref().is_none_or(|role| role == "assistant"),
                "provider streamed a non-assistant message"
            );
            ensure!(
                self.finish_reason.is_none(),
                "provider sent a choice after finishing the completion"
            );
            if let Some(text) = delta.content {
                self.content.get_or_insert_default().push_str(&text);
            }
            if let Some(text) = delta.reasoning_content {
                self.reasoning.get_or_insert_default().push_str(&text);
            }
            for fragment in delta.tool_calls.unwrap_or_default() {
                ensure!(
                    fragment.index < 128,
                    "provider streamed an invalid tool-call index"
                );
                let call = self
                    .calls
                    .entry(fragment.index)
                    .or_insert_with(|| ToolCall {
                        id: String::new(),
                        kind: "function".into(),
                        function: Function {
                            name: String::new(),
                            arguments: String::new(),
                        },
                    });
                if let Some(id) = fragment.id {
                    call.id.push_str(&id);
                }
                if let Some(kind) = fragment.kind {
                    call.kind = kind;
                }
                if let Some(function) = fragment.function {
                    call.function
                        .name
                        .push_str(function.name.as_deref().unwrap_or_default());
                    call.function
                        .arguments
                        .push_str(function.arguments.as_deref().unwrap_or_default());
                }
            }
            self.finish_reason = choice.finish_reason;
        }
        Ok(())
    }

    fn responses_event(&mut self, packet: Value) -> Result<()> {
        match packet["type"]
            .as_str()
            .context("missing Responses event type")?
        {
            "response.completed" | "response.incomplete" => {
                self.terminal = Some(packet["response"].clone());
                self.done = true;
            }
            "response.failed" | "error" => {
                let error = packet
                    .pointer("/response/error")
                    .filter(|value| !value.is_null())
                    .or_else(|| packet.get("error"))
                    .unwrap_or(&packet);
                self.provider_error = Some(error.clone());
                self.usage = packet
                    .pointer("/response/usage")
                    .and_then(|usage| serde_json::from_value(usage.clone()).ok());
            }
            "response.output_text.delta" => {
                let text = packet["delta"]
                    .as_str()
                    .context("invalid Responses text delta")?;
                self.content.get_or_insert_default().push_str(text);
                self.choice_seen = true;
            }
            "response.function_call_arguments.delta" => {
                self.response_progress = Some("Receiving tool calls…");
                self.choice_seen = true;
            }
            "response.reasoning_summary_text.delta" => {
                self.response_progress = Some("Receiving reasoning…");
                self.choice_seen = true;
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn done(&self) -> bool {
        self.done || self.provider_error.is_some()
    }

    pub(super) fn finish_input(&mut self) -> Result<()> {
        if !self.done() {
            // Some gateways close after the final data field without the
            // normal SSE separator. finish still requires a terminal marker.
            if !self.line.is_empty() {
                self.line()?;
                self.line.clear();
            }
            self.dispatch()?;
        }
        Ok(())
    }

    pub(super) fn take_error(&mut self) -> Option<(Value, Option<Usage>, bool)> {
        self.provider_error
            .take()
            .map(|error| (error, self.usage.take(), self.choice_seen))
    }

    pub(super) fn content(&self) -> &str {
        self.content.as_deref().unwrap_or_default()
    }

    pub(super) fn progress(&self) -> &'static str {
        if let Some(progress) = self.response_progress {
            progress
        } else if !self.calls.is_empty() {
            "Receiving tool calls…"
        } else if self.reasoning.is_some() {
            "Receiving reasoning…"
        } else {
            "Waiting for provider…"
        }
    }

    pub(super) fn finish(self) -> Result<Response> {
        if self.api == ProviderApi::Responses {
            let value = self
                .terminal
                .context("Responses stream ended without a terminal response")?;
            return responses::decode(value);
        }
        ensure!(
            self.line.is_empty()
                && self.data.is_empty()
                && self.choice_seen
                && (self.done || self.finish_reason.is_some()),
            "provider stream ended before completing its response"
        );
        if !matches!(
            self.finish_reason.as_deref(),
            Some("length" | "content_filter")
        ) {
            ensure!(
                self.calls.is_empty()
                    || (self.finish_reason.is_some()
                        && self
                            .calls
                            .iter()
                            .enumerate()
                            .all(|(index, (&key, call))| key == index
                                && !call.id.is_empty()
                                && !call.function.name.is_empty())),
                "provider streamed incomplete tool calls"
            );
        }
        Ok(Response {
            choices: vec![Choice {
                message: Message::Assistant {
                    content: self.content,
                    reasoning_content: self.reasoning,
                    tool_calls: self.calls.into_values().collect(),
                    responses_items: Vec::new(),
                },
                finish_reason: self.finish_reason,
            }],
            usage: self.usage,
        })
    }
}
