use std::time::Duration;

use anyhow::{Context, Result, ensure};
use reqwest::{Client, Url, header};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    budget::Budget,
    config::Config,
    context::estimate,
    events::{Kind, Reporter},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: Function,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Function {
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self::System {
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::User {
            content: content.into(),
        }
    }
}

#[derive(Deserialize)]
struct Response {
    choices: Vec<Choice>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    prompt_tokens: Option<usize>,
    completion_tokens: Option<usize>,
}

pub struct Completion {
    pub message: Message,
    pub prompt_tokens: Option<usize>,
    pub finish_reason: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("provider rejected context (HTTP {status}): {detail}")]
    Context { status: u16, detail: String },
    #[error("provider returned HTTP {status}: {detail}")]
    Status {
        status: u16,
        detail: String,
        retry_after: Option<Duration>,
    },
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

pub struct Api {
    client: Client,
    endpoint: Url,
    key: Option<String>,
    legacy_max_tokens: bool,
    debug: bool,
    reporter: Reporter,
    budget: Budget,
}

impl Api {
    pub fn new(config: &Config, reporter: Reporter, budget: Budget) -> Result<Self> {
        let endpoint = completions_url(&config.endpoint)?;
        let mut headers = header::HeaderMap::new();
        if let Some(key) = &config.api_key {
            let mut value = header::HeaderValue::from_str(&format!("Bearer {key}"))
                .context("invalid API key header")?;
            value.set_sensitive(true);
            headers.insert(header::AUTHORIZATION, value);
        }
        let client = Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(
                config.orchestration.request_timeout_secs,
            ))
            .build()?;
        Ok(Self {
            client,
            endpoint,
            key: config.api_key.clone(),
            legacy_max_tokens: config.legacy_max_tokens,
            debug: config.debug,
            reporter,
            budget,
        })
    }

    pub async fn complete(
        &self,
        model: &str,
        messages: &[&Message],
        tools: &[Value],
        output_tokens: usize,
    ) -> Result<Completion, ApiError> {
        let reserved = estimate(messages, tools).saturating_add(output_tokens) as u64;
        let mut last_http = None;
        for attempt in 0..=2 {
            self.budget.reserve(reserved)?;
            let result = tokio::time::timeout(
                self.budget.remaining(),
                self.complete_once(model, messages, tools, output_tokens, reserved),
            )
            .await;
            let error = match result {
                Ok(Ok(completion)) => return Ok(completion),
                Ok(Err(error)) => error,
                Err(_) => return Err(anyhow::anyhow!("run budget exhausted: active time").into()),
            };
            let backoff = Duration::from_millis(if attempt == 0 { 250 } else { 1000 });
            let wait = match &error {
                ApiError::Status {
                    status: 408 | 429 | 500 | 502 | 503 | 504,
                    retry_after,
                    ..
                } => backoff.max(retry_after.unwrap_or_default()),
                ApiError::Other(error)
                    if error.downcast_ref::<reqwest::Error>().is_some_and(|error| {
                        error.is_timeout()
                            || error.is_connect()
                            || error.is_body()
                            || error.is_request()
                    }) =>
                {
                    backoff
                }
                _ => return Err(error),
            };
            if attempt == 2 || wait > Duration::from_secs(30) || wait >= self.budget.remaining() {
                return Err(if matches!(error, ApiError::Status { .. }) {
                    error
                } else {
                    last_http.unwrap_or(error)
                });
            }
            self.reporter.emit(
                Kind::Info,
                "Provider retry",
                format!("Waiting {} ms after {error}", wait.as_millis()),
            );
            if matches!(error, ApiError::Status { .. }) {
                last_http = Some(error);
            }
            tokio::time::sleep(wait).await;
        }
        unreachable!("request retry loop is bounded")
    }

    async fn complete_once(
        &self,
        model: &str,
        messages: &[&Message],
        tools: &[Value],
        output_tokens: usize,
        reserved: u64,
    ) -> Result<Completion, ApiError> {
        let mut payload = serde_json::json!({
            "model": model,
            "messages": messages,
            "stream": false
        });
        if !tools.is_empty() {
            payload["tools"] = serde_json::json!(tools);
        }
        let limit = if self.legacy_max_tokens {
            "max_tokens"
        } else {
            "max_completion_tokens"
        };
        payload[limit] = serde_json::json!(output_tokens);
        if self.debug {
            self.reporter.emit(
                Kind::Info,
                "API request",
                format!(
                    "{model}, {} messages, {} tools, {output_tokens} output tokens",
                    messages.len(),
                    tools.len()
                ),
            );
        }

        let mut response = self
            .client
            .post(self.endpoint.clone())
            .json(&payload)
            .send()
            .await
            .map_err(|error| {
                anyhow::Error::new(error.without_url()).context("send chat completion")
            })?;
        let status = response.status();
        let retry_after = response.headers().get(header::RETRY_AFTER).map(|value| {
            value
                .to_str()
                .ok()
                .and_then(retry_delay)
                .unwrap_or(Duration::MAX)
        });
        let limit_bytes = if status.is_success() {
            16 * 1024 * 1024
        } else {
            32 * 1024
        };
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            anyhow::Error::new(error.without_url()).context("read chat completion")
        })? {
            let remaining = limit_bytes - bytes.len();
            if chunk.len() > remaining {
                if status.is_success() {
                    return Err(
                        anyhow::anyhow!("provider response exceeds {limit_bytes} bytes").into(),
                    );
                }
                bytes.extend_from_slice(&chunk[..remaining]);
                break;
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let code = body
                .pointer("/error/code")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let raw = String::from_utf8_lossy(&bytes);
            let detail = body
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or(&raw);
            let lower = detail.to_ascii_lowercase();
            let detail = match &self.key {
                Some(key) => detail.replace(key, "[redacted]"),
                None => detail.to_owned(),
            };
            let detail = detail
                .chars()
                .take(1000)
                .filter(|c| !c.is_control())
                .collect();
            let status = status.as_u16();
            return if code == "context_length_exceeded"
                || status == 413
                || (matches!(status, 400 | 422)
                    && (lower.contains("maximum context length")
                        || lower.contains("context window")
                        || lower.contains("context length exceeded")
                        || lower.contains("context size")))
            {
                Err(ApiError::Context { status, detail })
            } else {
                Err(ApiError::Status {
                    status,
                    detail,
                    retry_after,
                })
            };
        }

        let response: Response =
            serde_json::from_slice(&bytes).context("decode chat completion")?;
        let choice = response
            .choices
            .into_iter()
            .next()
            .context("provider returned no choices")?;
        let prompt_tokens = response
            .usage
            .as_ref()
            .and_then(|usage| usage.prompt_tokens);
        if let Some((prompt, completion)) = response
            .usage
            .as_ref()
            .and_then(|usage| usage.prompt_tokens.zip(usage.completion_tokens))
        {
            self.budget
                .reconcile(reserved, prompt.saturating_add(completion) as u64)?;
        }
        if self.debug {
            self.reporter.emit(
                Kind::Info,
                "API usage",
                format!(
                    "prompt={prompt_tokens:?}, completion={:?}, finish={:?}",
                    response.usage.and_then(|usage| usage.completion_tokens),
                    choice.finish_reason
                ),
            );
        }
        Ok(Completion {
            message: choice.message,
            prompt_tokens,
            finish_reason: choice.finish_reason,
        })
    }
}

fn retry_delay(value: &str) -> Option<Duration> {
    use std::time::{SystemTime, UNIX_EPOCH};
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    // Retry-After permits an IMF-fixdate in UTC.
    let mut fields = value.split_ascii_whitespace();
    fields.next()?.strip_suffix(',')?;
    let day = fields.next()?.parse::<u64>().ok()?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|month| Some(*month) == fields.clone().next())?;
    fields.next();
    let year = fields.next()?.parse::<u64>().ok()?;
    let mut clock = fields.next()?.split(':');
    let hours = clock.next()?.parse::<u64>().ok()?;
    let minutes = clock.next()?.parse::<u64>().ok()?;
    let seconds = clock.next()?.parse::<u64>().ok()?;
    if !(1970..=9999).contains(&year)
        || hours >= 24
        || minutes >= 60
        || seconds >= 60
        || clock.next().is_some()
        || fields.next()? != "GMT"
        || fields.next().is_some()
    {
        return None;
    }
    let leap = |year: u64| year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_before_year = |year: u64| {
        let last = year - 1;
        365 * last + last / 4 - last / 100 + last / 400
    };
    let lengths = [
        31,
        if leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1..=lengths[month]).contains(&day) {
        return None;
    }
    let days = days_before_year(year) - days_before_year(1970)
        + lengths[..month].iter().sum::<u64>()
        + day
        - 1;
    let date =
        UNIX_EPOCH + Duration::from_secs(days * 86400 + hours * 3600 + minutes * 60 + seconds);
    Some(date.duration_since(SystemTime::now()).unwrap_or_default())
}

fn completions_url(endpoint: &str) -> Result<Url> {
    let mut url = Url::parse(endpoint).context("invalid API endpoint URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "API endpoint must be an HTTP(S) URL"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none() && url.fragment().is_none(),
        "API endpoint cannot contain user info or a fragment"
    );
    let path = url.path().trim_end_matches('/');
    if !path.ends_with("/chat/completions") {
        let path = format!("{path}/chat/completions");
        url.set_path(&path);
    } else if path.len() != url.path().len() {
        let path = path.to_owned();
        url.set_path(&path);
    }
    Ok(url)
}
