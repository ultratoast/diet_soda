//! HTTP model adapters. A shared reqwest client reuses connections across turns
//! and parallel children; each request still has its own timeout/cancellation.
mod catalog;
mod reasoning;
mod sse;
pub use catalog::CatalogModel;
pub use sse::SseDecoder;

use crate::{
    config::{ModelConfig, ProviderConfig, ProviderKind},
    model::{Message, ToolCall, ToolSpec, UiEvent, Usage},
};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Display-only phases emitted in order for each streaming request. The
/// strings live alongside the producer so consumers (TUI, headless stderr)
/// and tests can rely on a single source of truth.
pub mod phases {
    pub const CONNECTING: &str = "Connecting";
    pub const WAITING: &str = "Waiting for first chunk/token";
    pub const STREAMING_PREFIX: &str = "Streaming";
    /// Format the post-first-data status line. "First data" is deliberately
    /// broader than a text token: a tool-call-only or reasoning-only stream
    /// still has latency worth reporting. Separated from the prefix constant
    /// so tests can match on the produced string without redoing the
    /// millisecond arithmetic themselves.
    pub fn streaming(first_data_millis: u128) -> String {
        format!("{STREAMING_PREFIX} (first data {first_data_millis} ms)")
    }
}

/// Typed error surfaced when the provider stream ends before producing the
/// protocol completion event. The engine translates this into a transcript
/// marker; raw tool-call fragments never leak into the returned [`Message`]
/// or back into model request history. Reasoning text and native continuation
/// metadata are intentionally dropped — a partial turn can never be replayed
/// or resumed — so only the accumulated visible text is preserved.
#[derive(Debug)]
pub struct IncompleteStreamError {
    pub message: Message,
    pub reason: String,
}
impl std::fmt::Display for IncompleteStreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Provider stream ended before completion: {}",
            self.reason
        )
    }
}
impl std::error::Error for IncompleteStreamError {}

pub struct ModelRequest {
    pub model: ModelConfig,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub context: String,
}
pub struct ModelResponse {
    pub message: Message,
    pub usage: Usage,
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn stream(
        &self,
        request: ModelRequest,
        events: &mpsc::UnboundedSender<UiEvent>,
        cancel: &CancellationToken,
    ) -> Result<ModelResponse>;
}

pub struct RemoteProvider {
    config: ProviderConfig,
    client: reqwest::Client,
}
impl RemoteProvider {
    fn authenticate(&self, mut http: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        let anthropic = self.config.kind == ProviderKind::Anthropic;
        if let Some(env) = &self.config.api_key_env {
            let key = std::env::var(env)
                .with_context(|| format!("Missing environment variable {env}"))?;
            http = if anthropic {
                http.header("x-api-key", key)
            } else {
                http.bearer_auth(key)
            };
        }
        if anthropic {
            http = http.header("anthropic-version", "2023-06-01");
        }
        if self.config.kind == ProviderKind::Openrouter {
            http = http.header("X-Title", env!("CARGO_PKG_NAME"));
        }
        for (name, value) in &self.config.headers {
            http = http.header(name, crate::config::expand_env(value)?);
        }
        Ok(http)
    }

    pub fn with_client(config: ProviderConfig, client: reqwest::Client) -> Self {
        Self { config, client }
    }
    /// Build a provider with a fresh client. The shared client deliberately
    /// carries no total-body deadline: streaming enforces its own header and
    /// idle bounds, and the catalog path wraps the whole call in
    /// [`crate::config::ProviderConfig::timeout_seconds`].
    pub fn new(config: ProviderConfig) -> Result<Self> {
        Ok(Self::with_client(
            config,
            reqwest::Client::builder().build()?,
        ))
    }
}

pub fn openai_messages(system: &str, messages: &[Message]) -> Vec<Value> {
    let mut result = vec![json!({"role":"system","content":system})];
    for m in messages {
        let mut value = json!({"role":m.role,"content":m.content});
        if !m.tool_calls.is_empty() {
            value["tool_calls"] = json!(m.tool_calls.iter().map(|t| json!({"id":t.id,"type":"function","function":{"name":t.name,"arguments":t.arguments}})).collect::<Vec<_>>());
        }
        if let Some(id) = &m.tool_call_id {
            value["tool_call_id"] = json!(id);
        }
        if let Some(reasoning) = &m.reasoning {
            value["reasoning"] = json!(reasoning);
        }
        if !m.reasoning_details.is_empty() {
            value["reasoning_details"] = json!(m.reasoning_details);
        }
        result.push(value);
    }
    result
}

pub fn anthropic_messages(messages: &[Message]) -> Vec<Value> {
    let mut result: Vec<Value> = vec![];
    for m in messages {
        let role = if m.role == "assistant" {
            "assistant"
        } else {
            "user"
        };
        let mut content = vec![];
        if m.role == "tool" {
            content.push(
                json!({"type":"tool_result","tool_use_id":m.tool_call_id,"content":m.content}),
            );
        } else if !m.native_content.is_empty() {
            content.clone_from(&m.native_content);
        } else {
            if !m.content.is_empty() {
                content.push(json!({"type":"text","text":m.content}));
            }
            for t in &m.tool_calls {
                content.push(json!({"type":"tool_use","id":t.id,"name":t.name,"input":serde_json::from_str::<Value>(&t.arguments).unwrap_or(json!({}))}));
            }
        }
        if let Some(previous) = result.last_mut().filter(|v| v["role"] == role) {
            previous["content"].as_array_mut().unwrap().extend(content);
        } else {
            result.push(json!({"role":role,"content":content}));
        }
    }
    result
}

#[async_trait]
impl ModelProvider for RemoteProvider {
    async fn stream(
        &self,
        request: ModelRequest,
        events: &mpsc::UnboundedSender<UiEvent>,
        cancel: &CancellationToken,
    ) -> Result<ModelResponse> {
        let anthropic = self.config.kind == ProviderKind::Anthropic;
        let mut body = if anthropic {
            json!({"model":request.model.model,"system":request.system,"messages":anthropic_messages(&request.messages),"max_tokens":request.model.max_tokens,"stream":true})
        } else {
            json!({"model":request.model.model,"messages":openai_messages(&request.system,&request.messages),"max_tokens":request.model.max_tokens,"stream":true,"stream_options":{"include_usage":true}})
        };
        if let Some(t) = request.model.temperature {
            body["temperature"] = json!(t);
        }
        if self.config.kind == ProviderKind::Openai {
            body.as_object_mut().unwrap().remove("max_tokens");
            body["max_completion_tokens"] = json!(request.model.max_tokens);
        }
        if !request.tools.is_empty() {
            body["tools"] = json!(request.tools.iter().map(|t| if anthropic { json!({"name":t.name,"description":t.description,"input_schema":t.input_schema}) } else { json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.input_schema}}) }).collect::<Vec<_>>());
        }
        if self.config.kind == ProviderKind::Openrouter {
            body["usage"] = json!({"include":true});
        }
        reasoning::apply_effort(&mut body, &request.model, &self.config.kind)?;
        let url = format!(
            "{}/{}",
            self.config.base_url.trim_end_matches('/'),
            if anthropic {
                "messages"
            } else {
                "chat/completions"
            }
        );
        let http = self.client.post(url).json(&body);
        let http = self.authenticate(http)?;
        // Header + first-chunk deadline: applies to the time from `send()`
        // returning to the first bytes arriving. After the first chunk the
        // gap is enforced per-chunk by the loop below, so a slow streaming
        // model that keeps dribbling bytes never trips the deadline as long
        // as each gap stays under the configured limit.
        let header_deadline = idle_deadline(self.config.timeout_seconds);
        let request_start = Instant::now();
        let _ = events.send(UiEvent::Status {
            context: request.context.clone(),
            text: phases::CONNECTING.into(),
        });
        let response = match tokio::select! {
            _ = cancel.cancelled() => Err(incomplete(&Message::new("assistant", ""), "cooperative cancellation")),
            result = tokio::time::timeout(header_deadline, http.send()) => match result {
                Ok(inner) => inner.context("Provider connection failed"),
                Err(_) => Err(incomplete(&Message::new("assistant", ""), "header timeout: no response within configured budget")),
            },
        } {
            Ok(response) => response,
            Err(error) => return Err(error),
        };
        if !response.status().is_success() {
            bail!("Provider returned HTTP {}", response.status());
        }
        let _ = events.send(UiEvent::Status {
            context: request.context.clone(),
            text: phases::WAITING.into(),
        });
        let mut stream = response.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut message = Message::new("assistant", "");
        let mut calls: BTreeMap<u64, ToolCall> = BTreeMap::new();
        let mut blocks = BTreeMap::new();
        let mut reasoning_details = BTreeMap::new();
        let mut usage = Usage::default();
        let mut finished = false;
        let mut content_bytes = 0;
        // OpenAI-compatible streams may close cleanly without `[DONE]` when
        // a valid non-null, non-empty `finish_reason` was observed and every
        // streamed tool call carries both an id, a name, and a valid JSON
        // arguments payload. The handler only consumes the first choice
        // because the conversation contract is one assistant turn at a time;
        // a second choice in the same chunk would be a protocol violation.
        // Anthropic still requires its protocol completion event; the
        // absence of `message_stop` is an incomplete stream either way.
        let mut observed_finish_reasons: Vec<String> = vec![];
        let mut saw_first_data = false;
        // Per-chunk idle deadline, re-armed each iteration so a provider that
        // keeps dribbling bytes never trips the deadline. We never cancel the
        // request: cooperative cancel goes through the cancellation token only.
        let chunk_idle = idle_deadline(self.config.timeout_seconds);
        loop {
            let next_chunk = stream.next();
            let chunk = tokio::select! {
                _ = cancel.cancelled() => {
                    return Err(incomplete(&message, "cooperative cancellation"));
                }
                result = tokio::time::timeout(chunk_idle, next_chunk) => match result {
                    Ok(Some(Ok(chunk))) => chunk,
                    Ok(Some(Err(error))) => {
                        return Err(incomplete(&message, format!("network error: {error}")));
                    }
                    Ok(None) => break,
                    Err(_) => {
                        return Err(incomplete(
                            &message,
                            format!("idle timeout: no chunk within {}s", chunk_idle.as_secs()),
                        ));
                    }
                },
            };
            // SSE decoder/UTF-8/JSON corruption here means a chunk that may
            // already carry partial visible text. Surface it as an incomplete
            // stream so the engine persists the safe partial instead of a
            // raw serde error that would discard what was streamed.
            let chunk_events = match decoder.push(&chunk) {
                Ok(events) => events,
                Err(error) => {
                    return Err(incomplete(&message, format!("SSE decoder error: {error}")));
                }
            };
            for data in chunk_events {
                if finished {
                    // Post-completion sentinel was already seen; the stream
                    // is protocol-complete and any trailing frames must be
                    // ignored so a malformed or stray event after [DONE]
                    // cannot append content or fail the completed response.
                    break;
                }
                if data == "[DONE]" {
                    finished = true;
                    continue;
                }
                let value: Value = match serde_json::from_str(&data) {
                    Ok(value) => value,
                    Err(error) => {
                        return Err(incomplete(
                            &message,
                            format!("Invalid provider SSE JSON: {error}"),
                        ));
                    }
                };
                if value.get("error").is_some() || value["type"] == "error" {
                    return Err(incomplete(&message, "provider reported a streaming error"));
                }
                let mut text = None;
                // Set when this event carries meaningful provider output —
                // visible text, reasoning, or a tool call. It drives the
                // one-shot Streaming status so reasoning-only and
                // tool-call-only streams still report first-data latency.
                let mut data_seen = false;
                if anthropic {
                    match value["type"].as_str().unwrap_or("") {
                        "message_start" => {
                            update_usage(&mut usage, &value["message"]["usage"], true)
                        }
                        "message_delta" => update_usage(&mut usage, &value["usage"], true),
                        "message_stop" => finished = true,
                        "content_block_start" => {
                            let block = &value["content_block"];
                            blocks.insert(value["index"].as_u64().unwrap_or(0), block.clone());
                            if block["type"] == "tool_use" {
                                calls.insert(
                                    value["index"].as_u64().unwrap_or(0),
                                    ToolCall {
                                        id: block["id"].as_str().unwrap_or("").into(),
                                        name: block["name"].as_str().unwrap_or("").into(),
                                        arguments: String::new(),
                                    },
                                );
                                data_seen = true;
                            } else if block["type"] == "text" {
                                text = block["text"].as_str();
                                data_seen |= text.is_some_and(|s| !s.is_empty());
                            }
                        }
                        "content_block_delta" => {
                            let index = value["index"].as_u64().unwrap_or(0);
                            reasoning::append_block_delta(&mut blocks, &value)?;
                            let delta = &value["delta"];
                            // Reasoning deltas (thinking/signature) are
                            // meaningful first data even without text.
                            data_seen |= ["text", "thinking", "signature"]
                                .iter()
                                .any(|field| delta[*field].as_str().is_some_and(|s| !s.is_empty()));
                            if blocks
                                .get(&index)
                                .is_some_and(|block| block["type"] == "text")
                            {
                                text = value["delta"]["text"].as_str();
                            }
                            if let Some(part) = value["delta"]["partial_json"].as_str() {
                                calls
                                    .get_mut(&value["index"].as_u64().unwrap_or(0))
                                    .context("Tool delta before start")?
                                    .arguments
                                    .push_str(part);
                                data_seen |= !part.is_empty();
                            }
                        }
                        _ => {}
                    }
                } else {
                    update_usage(&mut usage, &value["usage"], false);
                    if let Some(choice) = value["choices"].as_array().and_then(|a| a.first()) {
                        let delta = &choice["delta"];
                        if let Some(reasoning_text) = delta["reasoning"]
                            .as_str()
                            .or_else(|| delta["reasoning_content"].as_str())
                        {
                            message
                                .reasoning
                                .get_or_insert_with(String::new)
                                .push_str(reasoning_text);
                            data_seen |= !reasoning_text.is_empty();
                        }
                        data_seen |= delta["reasoning_details"]
                            .as_array()
                            .is_some_and(|items| !items.is_empty());
                        reasoning::append_details(
                            &mut reasoning_details,
                            &delta["reasoning_details"],
                        );
                        text = delta["content"].as_str();
                        data_seen |= text.is_some_and(|s| !s.is_empty());
                        if let Some(items) = delta["tool_calls"].as_array() {
                            data_seen |= !items.is_empty();
                            for item in items {
                                let call = calls
                                    .entry(item["index"].as_u64().unwrap_or(0))
                                    .or_insert_with(|| ToolCall {
                                        id: String::new(),
                                        name: String::new(),
                                        arguments: String::new(),
                                    });
                                if let Some(id) = item["id"].as_str() {
                                    call.id.push_str(id);
                                }
                                if let Some(name) = item["function"]["name"].as_str() {
                                    call.name.push_str(name);
                                }
                                if let Some(part) = item["function"]["arguments"].as_str() {
                                    call.arguments.push_str(part);
                                }
                            }
                        }
                        if let Some(reason) = choice["finish_reason"].as_str() {
                            observed_finish_reasons.push(reason.to_owned());
                        }
                    }
                }
                content_bytes += data.len();
                if content_bytes > 16_000_000 {
                    return Err(incomplete(&message, "model response exceeded 16 MB limit"));
                }
                // Emit the one-shot Streaming phase on the first meaningful
                // provider data. It is sent before the first `Delta` (when
                // this event carried text) and fires for reasoning-only and
                // tool-call-only streams that never produce visible text.
                if data_seen && !saw_first_data {
                    saw_first_data = true;
                    let first_data = request_start.elapsed();
                    let _ = events.send(UiEvent::Status {
                        context: request.context.clone(),
                        text: phases::streaming(first_data.as_millis()),
                    });
                }
                if let Some(text) = text.filter(|s| !s.is_empty()) {
                    message.content.push_str(text);
                    let _ = events.send(UiEvent::Delta {
                        context: request.context.clone(),
                        text: text.into(),
                    });
                }
            }
            if finished {
                break;
            }
        }
        if !finished {
            // EOF without the protocol completion event. OpenAI-compatible
            // streams get one exception: a clean close without `[DONE]` is
            // acceptable only when every observed finish_reason is a valid
            // non-null, non-empty string AND every streamed tool call is
            // complete (id, name, and valid JSON arguments; empty arguments
            // count as complete because the completion path normalizes them
            // to `{}`).
            // Anthropic still requires its `message_stop` event; the absence
            // is an incomplete stream either way.
            let openai_calls_complete = calls.values().all(|call| {
                !call.id.is_empty()
                    && !call.name.is_empty()
                    && (call.arguments.is_empty()
                        || serde_json::from_str::<Value>(&call.arguments).is_ok())
            });
            let openai_eof_ok = !anthropic
                && !observed_finish_reasons.is_empty()
                && observed_finish_reasons
                    .iter()
                    .all(|reason| !reason.is_empty() && reason != "null")
                && openai_calls_complete;
            if !openai_eof_ok {
                return Err(incomplete(&message, "stream ended before completion event"));
            }
        }
        // Final sanity check: tool calls with a missing id/name are treated
        // as incomplete even when EOF looked clean. The provider contract is
        // that every streamed call carries both before the final event.
        for call in calls.values() {
            if call.id.is_empty() || call.name.is_empty() {
                return Err(incomplete(&message, "incomplete tool call from provider"));
            }
        }
        for (index, mut call) in calls {
            if call.arguments.is_empty() {
                call.arguments = "{}".into();
            }
            if let Some(block) = blocks.get_mut(&index) {
                block["input"] =
                    serde_json::from_str(&call.arguments).context("Invalid streamed tool input")?;
            }
            message.tool_calls.push(call);
        }
        message.native_content = blocks.into_values().collect();
        message.reasoning_details = reasoning_details.into_values().collect();
        if usage.cost_microusd.is_none() && usage.tokens_reported {
            if let (Some(input), Some(output)) = (
                request.model.input_usd_per_million,
                request.model.output_usd_per_million,
            ) {
                usage.cost_microusd = Some(
                    (usage.input_tokens as f64 * input + usage.output_tokens as f64 * output)
                        .round() as u64,
                );
                usage.estimated = true;
            }
        }
        Ok(ModelResponse { message, usage })
    }
}

/// Build an incomplete-stream error from whatever partial output has been
/// collected. The returned [`Message`] intentionally drops tool calls, native
/// content, and both reasoning text and reasoning continuation metadata, so
/// the caller can persist a partial turn without leaking anything the model
/// would later re-execute, re-send, or resume from. Only the accumulated
/// visible text is preserved for the transcript.
fn incomplete(partial: &Message, reason: impl Into<String>) -> anyhow::Error {
    let reason = reason.into();
    let safe = Message::incomplete_assistant(partial.content.clone(), reason.clone());
    anyhow::Error::new(IncompleteStreamError {
        message: safe,
        reason,
    })
}

/// Upper bound on time from `send()` until the first bytes arrive (response
/// headers + first body chunk), and on the gap between subsequent body
/// chunks. The configured `timeout_seconds` is the only knob: a small value
/// is tight, a large value is forgiving. The deadline is re-armed on every
/// chunk so a provider that keeps dribbling bytes never trips it.
fn idle_deadline(timeout_seconds: u64) -> Duration {
    Duration::from_secs(timeout_seconds.max(1))
}

fn update_usage(usage: &mut Usage, value: &Value, anthropic: bool) {
    usage.tokens_reported |=
        value.get("input_tokens").is_some() || value.get("prompt_tokens").is_some();
    if let Some(n) = value[if anthropic {
        "input_tokens"
    } else {
        "prompt_tokens"
    }]
    .as_u64()
    {
        usage.input_tokens = n;
    }
    if let Some(n) = value[if anthropic {
        "output_tokens"
    } else {
        "completion_tokens"
    }]
    .as_u64()
    {
        usage.output_tokens = n;
    }
    if let Some(n) = value["prompt_tokens_details"]["cached_tokens"]
        .as_u64()
        .or_else(|| value["cache_read_input_tokens"].as_u64())
    {
        usage.cached_tokens = n;
    }
    if let Some(n) = value["cost"]
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0)
    {
        usage.cost_microusd = Some((n * 1_000_000.0).round() as u64);
    }
}
