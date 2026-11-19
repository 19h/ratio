use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use super::{Choice, Function, Message, Response, ToolCall};

pub(super) fn payload(
    model: &str,
    messages: &[&Message],
    tools: &[Value],
    max_output_tokens: usize,
    stream: bool,
) -> Value {
    let mut input = Vec::new();
    for &message in messages {
        match message {
            Message::System { content } => input.push(json!({"role":"system","content":content})),
            Message::User { content } => input.push(json!({"role":"user","content":content})),
            Message::Tool {
                tool_call_id,
                content,
            } => {
                input.push(
                    json!({"type":"function_call_output","call_id":tool_call_id,"output":content}),
                );
            }
            Message::Assistant {
                responses_items, ..
            } if !responses_items.is_empty() => {
                input.extend(responses_items.iter().cloned());
            }
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => {
                if let Some(content) = content {
                    input.push(json!({"role":"assistant","content":content}));
                }
                input.extend(tool_calls.iter().map(|call| {
                    json!({"type":"function_call","call_id":call.id,"name":call.function.name,
                        "arguments":call.function.arguments})
                }));
            }
        }
    }
    let tools = tools
        .iter()
        .map(|tool| {
            let function = &tool["function"];
            json!({"type":"function","name":function["name"],"description":function["description"],
            "parameters":function["parameters"],"strict":false})
        })
        .collect::<Vec<_>>();
    json!({
        "model":model, "input":input, "tools":tools, "store":false, "stream":stream,
        "include":["reasoning.encrypted_content"], "max_output_tokens":max_output_tokens
    })
}

pub(super) fn decode(value: Value) -> Result<Response> {
    let finish_reason = match value["status"].as_str() {
        Some("completed") => None,
        Some("incomplete") => match value
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
        {
            Some("max_output_tokens") => Some("length"),
            Some("content_filter") => Some("content_filter"),
            _ => anyhow::bail!("Responses provider returned an unknown incomplete reason"),
        },
        _ => anyhow::bail!("Responses provider did not return a completed response"),
    };
    let usage = if value["usage"].is_null() {
        None
    } else {
        Some(serde_json::from_value(value["usage"].clone()).context("decode Responses usage")?)
    };
    let mut calls = Vec::new();
    let mut content = String::new();
    let mut refused = false;
    let mut items = Vec::new();
    if finish_reason.is_none() {
        let output = value["output"]
            .as_array()
            .context("Responses output must be an array")?;
        ensure!(output.len() <= 128, "Responses output has too many items");
        for item in output {
            ensure!(
                item["status"]
                    .as_str()
                    .is_none_or(|status| status == "completed"),
                "Responses output item did not complete"
            );
            match item["type"].as_str() {
                Some("message") => {
                    ensure!(
                        item["role"] == "assistant",
                        "provider returned a non-assistant message"
                    );
                    for part in item["content"]
                        .as_array()
                        .context("invalid Responses message content")?
                    {
                        match part["type"].as_str() {
                            Some("output_text") => content.push_str(field(part, "text")?),
                            Some("refusal") => refused = true,
                            _ => anyhow::bail!("unexpected Responses message content"),
                        }
                    }
                }
                Some("function_call") => calls.push(ToolCall {
                    id: field(item, "call_id")?.into(),
                    kind: "function".into(),
                    function: Function {
                        name: field(item, "name")?.into(),
                        arguments: field(item, "arguments")?.into(),
                    },
                }),
                Some("reasoning") => {}
                _ => anyhow::bail!("unexpected Responses output item type"),
            }
            items.push(item.clone());
        }
    }
    let reason = finish_reason.unwrap_or({
        if refused {
            "content_filter"
        } else if calls.is_empty() {
            "stop"
        } else {
            "tool_calls"
        }
    });
    Ok(Response {
        choices: vec![Choice {
            message: Message::Assistant {
                content: (!content.is_empty()).then_some(content),
                reasoning_content: None,
                tool_calls: calls,
                responses_items: items,
            },
            finish_reason: Some(reason.into()),
        }],
        usage,
    })
}

fn field<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value[field]
        .as_str()
        .with_context(|| format!("Responses item is missing string field {field}"))
}
