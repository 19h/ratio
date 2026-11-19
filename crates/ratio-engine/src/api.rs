use std::{
    future::pending,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use reqwest::{Client, Url, header};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::{
    budget::Budget,
    config::{Config, Provider, ProviderApi},
    context::{clip, estimate},
    events::{Generation, Kind, Reporter, clean},
};

mod responses;
mod stream;

const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
static GENERATION_IDS: AtomicUsize = AtomicUsize::new(0);

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

pub(crate) fn tool_arguments<T: DeserializeOwned>(arguments: &str) -> Result<T> {
    ensure!(
        arguments.trim_start().starts_with('{'),
        "arguments must be a JSON object"
    );
    serde_json::from_str(arguments).context("invalid tool arguments")
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
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        responses_items: Vec<Value>,
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

#[derive(Clone, Deserialize)]
struct Usage {
    #[serde(alias = "input_tokens")]
    prompt_tokens: Option<usize>,
    #[serde(alias = "output_tokens")]
    completion_tokens: Option<usize>,
}

pub struct Completion {
    pub message: Message,
    pub prompt_tokens: Option<usize>,
    pub finish_reason: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("model generation cancelled")]
    Cancelled,
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
    secrets: Vec<String>,
    provider: Provider,
    legacy_max_tokens: bool,
    debug: bool,
    reporter: Reporter,
    budget: Budget,
}

impl Api {
    pub fn new(config: &Config, reporter: Reporter, budget: Budget) -> Result<Self> {
        let endpoint = completion_url(&config.endpoint, config.provider.api)?;
        let mut headers = header::HeaderMap::new();
        let mut secrets = Vec::new();
        if let Some(key) = &config.api_key {
            let mut value = header::HeaderValue::from_str(&format!("Bearer {key}"))
                .context("invalid API key header")?;
            value.set_sensitive(true);
            headers.insert(header::AUTHORIZATION, value);
            secrets.push(key.clone());
        }
        for (name, value) in &config.provider.headers {
            let name = header::HeaderName::from_bytes(name.as_bytes())
                .context("invalid provider header name")?;
            ensure!(
                !matches!(
                    name.as_str(),
                    "accept"
                        | "connection"
                        | "content-length"
                        | "content-type"
                        | "expect"
                        | "host"
                        | "transfer-encoding"
                        | "upgrade"
                ),
                "provider header {name} is managed by Ratio or HTTP"
            );
            let mut encoded = header::HeaderValue::from_str(value)
                .with_context(|| format!("invalid value for provider header {name}"))?;
            encoded.set_sensitive(true);
            if redact_header_value(&name, value) {
                secrets.push(value.clone());
            }
            headers.insert(name, encoded);
        }
        secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
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
            secrets,
            provider: config.provider.clone(),
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
        self.complete_for(model, model, messages, tools, output_tokens)
            .await
    }

    pub(crate) async fn wait_for_generation(&self) -> Result<()> {
        let Some(mut control) = self.reporter.generation_control() else {
            return Ok(());
        };
        if control.borrow().paused {
            let _pause = self.budget.pause()?;
            control
                .wait_for(|state| !state.paused)
                .await
                .context("generation controls closed")?;
        }
        Ok(())
    }

    pub async fn complete_for(
        &self,
        label: &str,
        model: &str,
        messages: &[&Message],
        tools: &[Value],
        output_tokens: usize,
    ) -> Result<Completion, ApiError> {
        let reserved = estimate(messages, tools).saturating_add(output_tokens) as u64;
        let mut last_http = None;
        let mut attempt = 0;
        let mut control = self.reporter.generation_control();
        loop {
            let state = control
                .as_ref()
                .map(|control| *control.borrow())
                .unwrap_or_default();
            if state.paused {
                return Err(ApiError::Cancelled);
            }
            let epoch = state.epoch;
            self.budget.reserve(reserved)?;
            let mut preview = Preview::new(self, label);
            let cancelled = async {
                match &mut control {
                    Some(control) => {
                        let _ = control.wait_for(|state| state.epoch != epoch).await;
                    }
                    None => pending::<()>().await,
                }
            };
            let result = tokio::time::timeout(
                self.budget.remaining(),
                self.complete_once(
                    model,
                    messages,
                    tools,
                    output_tokens,
                    reserved,
                    &mut preview,
                ),
            );
            let result = tokio::select! {
                biased;
                () = cancelled => return Err(ApiError::Cancelled),
                result = result => result,
            };
            drop(preview);
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
            attempt += 1;
        }
    }

    async fn complete_once(
        &self,
        model: &str,
        messages: &[&Message],
        tools: &[Value],
        output_tokens: usize,
        reserved: u64,
        preview: &mut Preview<'_>,
    ) -> Result<Completion, ApiError> {
        let responses = self.provider.api == ProviderApi::Responses;
        let mut payload = if responses {
            responses::payload(model, messages, tools, output_tokens, self.provider.stream)
        } else {
            let messages = messages
                .iter()
                .map(|message| chat_message(message))
                .collect::<Vec<_>>();
            serde_json::json!({"model": model, "messages": messages, "stream": self.provider.stream})
        };
        if !responses && self.provider.stream && self.provider.stream_usage {
            payload["stream_options"] = serde_json::json!({"include_usage": true});
        }
        if let Some(value) = self.provider.temperature {
            payload["temperature"] = serde_json::json!(value);
        }
        if let Some(value) = self.provider.top_p {
            payload["top_p"] = serde_json::json!(value);
        }
        if let Some(value) = self.provider.seed {
            payload["seed"] = serde_json::json!(value);
        }
        if !tools.is_empty() {
            if !responses {
                payload["tools"] = serde_json::json!(tools);
            }
            if let Some(value) = self.provider.parallel_tool_calls {
                payload["parallel_tool_calls"] = serde_json::json!(value);
            }
        }
        if !responses {
            let limit = if self.legacy_max_tokens {
                "max_tokens"
            } else {
                "max_completion_tokens"
            };
            payload[limit] = serde_json::json!(output_tokens);
        }
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

        let response = self
            .client
            .post(self.endpoint.clone())
            .header(
                header::ACCEPT,
                if self.provider.stream {
                    "text/event-stream, application/json"
                } else {
                    "application/json"
                },
            )
            .json(&payload)
            .send()
            .await
            .map_err(|error| {
                anyhow::Error::new(error.without_url()).context("send model completion")
            })?;
        let status = response.status();
        if status.is_success()
            && response
                .headers()
                .get(header::CONTENT_TYPE)
                .is_some_and(|value| {
                    value.to_str().ok().is_some_and(|value| {
                        value.split(';').next().is_some_and(|mime| {
                            mime.trim().eq_ignore_ascii_case("text/event-stream")
                        })
                    })
                })
        {
            self.read_stream(response, reserved, preview).await
        } else {
            self.read_json(response, reserved).await
        }
    }

    async fn read_stream(
        &self,
        mut response: reqwest::Response,
        reserved: u64,
        preview: &mut Preview<'_>,
    ) -> Result<Completion, ApiError> {
        let status = response.status().as_u16();
        let mut decoder = stream::Decoder::new(self.provider.api);
        let mut received = 0usize;
        let mut dirty = false;
        loop {
            let flush = async {
                if preview.id.is_some() && dirty {
                    tokio::time::sleep(
                        Duration::from_millis(50).saturating_sub(preview.last.elapsed()),
                    )
                    .await;
                } else {
                    pending::<()>().await;
                }
            };
            let chunk = tokio::select! {
                biased;
                () = flush => {
                    preview.update(decoder.content(), decoder.progress());
                    dirty = false;
                    continue;
                }
                result = response.chunk() => result,
            }
            .map_err(|error| {
                anyhow::Error::new(error.without_url()).context("read model completion stream")
            })?;
            let Some(chunk) = chunk else { break };
            received = received.saturating_add(chunk.len());
            ensure_response_size(received)?;
            decoder
                .push(&chunk)
                .map_err(|error| self.safe_error(error))?;
            dirty = true;
            if decoder.done() {
                break;
            }
        }
        decoder
            .finish_input()
            .map_err(|error| self.safe_error(error))?;
        if let Some((error, usage, choice_seen)) = decoder.take_error() {
            let code = error
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let detail = error
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| error.as_str())
                .unwrap_or("unknown error");
            let context_rejection = is_context_error(code, detail);
            let detail = clean(clip(&self.redact(detail), 1024));
            if let Some(actual) = usage.as_ref().and_then(Usage::total) {
                self.budget.reconcile(reserved, actual)?;
            } else if context_rejection && !choice_seen {
                self.budget.reconcile(reserved, 0)?;
            }
            return Err(if context_rejection {
                ApiError::Context { status, detail }
            } else {
                anyhow::anyhow!("provider stream error: {detail}").into()
            });
        }
        let response = decoder.finish().map_err(|error| self.safe_error(error))?;
        self.finish(response, reserved)
    }

    async fn read_json(
        &self,
        mut response: reqwest::Response,
        reserved: u64,
    ) -> Result<Completion, ApiError> {
        let status = response.status();
        let retry_after = response.headers().get(header::RETRY_AFTER).map(|value| {
            value
                .to_str()
                .ok()
                .and_then(retry_delay)
                .unwrap_or(Duration::MAX)
        });
        let limit_bytes = if status.is_success() {
            MAX_RESPONSE_BYTES
        } else {
            32 * 1024
        };
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            anyhow::Error::new(error.without_url()).context("read model completion")
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
            let context_rejection = status.as_u16() == 413
                || code == "context_length_exceeded"
                || (matches!(status.as_u16(), 400 | 422) && is_context_error(code, detail));
            let detail = self
                .redact(detail)
                .chars()
                .take(1000)
                .filter(|c| !c.is_control())
                .collect();
            let status = status.as_u16();
            let usage = serde_json::from_value::<Usage>(body["usage"].clone()).ok();
            if let Some(total) = usage.as_ref().and_then(Usage::total) {
                self.budget.reconcile(reserved, total)?;
            } else if status == 429 || context_rejection {
                // The provider explicitly refused admission. Keep the attempt
                // charge, but release the token reservation.
                self.budget.reconcile(reserved, 0)?;
            }
            return if context_rejection {
                Err(ApiError::Context { status, detail })
            } else {
                Err(ApiError::Status {
                    status,
                    detail,
                    retry_after,
                })
            };
        }

        let response = match self.provider.api {
            ProviderApi::ChatCompletions => {
                serde_json::from_slice(&bytes).context("decode chat completion")
            }
            ProviderApi::Responses => serde_json::from_slice(&bytes)
                .context("decode Responses reply")
                .and_then(responses::decode),
        }
        .map_err(|error| self.safe_error(error))?;
        self.finish(response, reserved)
    }

    fn finish(&self, response: Response, reserved: u64) -> Result<Completion, ApiError> {
        let choice = response
            .choices
            .into_iter()
            .next()
            .context("provider returned no choices")?;
        let prompt_tokens = response
            .usage
            .as_ref()
            .and_then(|usage| usage.prompt_tokens);
        if let Some(actual) = response.usage.as_ref().and_then(Usage::total) {
            self.budget.reconcile(reserved, actual)?;
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

    fn redact(&self, text: &str) -> String {
        self.secrets.iter().fold(text.to_owned(), |text, secret| {
            text.replace(secret, "[redacted]")
        })
    }

    fn safe_error(&self, error: anyhow::Error) -> ApiError {
        anyhow::anyhow!("{}", clean(clip(&self.redact(&format!("{error:#}")), 1024))).into()
    }
}

fn redact_header_value(name: &header::HeaderName, value: &str) -> bool {
    !value.is_empty()
        && (value.len() >= 8
            || matches!(
                name.as_str(),
                "authorization" | "proxy-authorization" | "x-api-key" | "api-key"
            ))
}

impl Usage {
    fn total(&self) -> Option<u64> {
        Some((self.prompt_tokens? as u64).saturating_add(self.completion_tokens? as u64))
    }
}

fn is_context_error(code: &str, detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    code == "context_length_exceeded"
        || [
            "maximum context length",
            "context window",
            "context length exceeded",
            "context size",
        ]
        .iter()
        .any(|phrase| detail.contains(phrase))
}

fn ensure_response_size(received: usize) -> Result<()> {
    ensure!(
        received <= MAX_RESPONSE_BYTES,
        "provider response exceeds {MAX_RESPONSE_BYTES} bytes"
    );
    Ok(())
}

struct Preview<'a> {
    api: &'a Api,
    id: Option<usize>,
    label: &'a str,
    last: Instant,
}

impl<'a> Preview<'a> {
    fn new(api: &'a Api, label: &'a str) -> Self {
        let id = api
            .reporter
            .observes_network()
            .then(|| GENERATION_IDS.fetch_add(1, Ordering::Relaxed));
        if let Some(id) = id {
            api.reporter.generation(Generation::Preview {
                id,
                label: clean(clip(label, 240)),
                text: "Waiting for provider…".into(),
            });
        }
        Self {
            api,
            id,
            label,
            last: Instant::now(),
        }
    }

    fn update(&mut self, content: &str, progress: &str) {
        let Some(id) = self.id else { return };
        self.last = Instant::now();
        let text = self.api.redact(content);
        let mut end = text.len();
        // A credential can span two deltas. Withhold a matching prefix.
        for secret in &self.api.secrets {
            for (length, _) in secret.char_indices().rev() {
                if length > 0 && text.ends_with(&secret[..length]) {
                    end = end.min(text.len() - length);
                    break;
                }
            }
        }
        let text = &text[..end];
        let text = if text.trim().is_empty() {
            progress
        } else {
            text
        };
        let mut start = text.len().saturating_sub(2048);
        while !text.is_char_boundary(start) {
            start += 1;
        }
        self.api.reporter.generation(Generation::Preview {
            id,
            label: clean(clip(self.label, 240)),
            text: clean(text[start..].to_owned()),
        });
    }
}

impl Drop for Preview<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.api.reporter.generation(Generation::End(id));
        }
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

pub(crate) fn chat_message(message: &Message) -> Value {
    let mut value = serde_json::to_value(message).expect("serializable model message");
    if let Some(fields) = value.as_object_mut() {
        fields.remove("responses_items");
    }
    value
}

fn completion_url(endpoint: &str, api: ProviderApi) -> Result<Url> {
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
    let suffix = match api {
        ProviderApi::ChatCompletions => "/chat/completions",
        ProviderApi::Responses => "/responses",
    };
    if !path.ends_with(suffix) {
        let path = format!("{path}{suffix}");
        url.set_path(&path);
    } else if path.len() != url.path().len() {
        let path = path.to_owned();
        url.set_path(&path);
    }
    Ok(url)
}
