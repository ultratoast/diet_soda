//! HTTP model adapters. Each configured endpoint is DNS-validated and pinned
//! before requests; provider streaming applies cancellation and idle deadlines.
mod cache;
mod catalog;
mod reasoning;
mod sse;
pub use catalog::CatalogModel;
pub use sse::SseDecoder;

use crate::{
    config::{ModelConfig, ProviderConfig, ProviderKind},
    model::{Message, ToolCall, ToolSpec, UiEvent, Usage},
};
use anyhow::{Context, Result};
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
    /// Final billed usage when the provider completed the protocol before the
    /// response was rejected (e.g. truncation); `None` for mid-stream breaks.
    pub usage: Option<Usage>,
    /// True only when the engine rejected a model turn that had no visible text and no tool calls (an empty response). Lets callers detect it without string matching.
    pub empty: bool,
}
impl std::fmt::Display for IncompleteStreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Provider response incomplete: {}", self.reason)
    }
}
impl std::error::Error for IncompleteStreamError {}

/// Typed non-success HTTP response from a provider. Display text matches the
/// pre-existing `bail!` messages so rendered errors and tests are unchanged.
#[derive(Debug)]
pub struct ProviderHttpError {
    pub status: reqwest::StatusCode,
    pub body: String,
}
impl std::fmt::Display for ProviderHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.body.is_empty() {
            write!(f, "Provider returned HTTP {}", self.status)
        } else {
            write!(f, "Provider returned HTTP {}: {}", self.status, self.body)
        }
    }
}
impl std::error::Error for ProviderHttpError {}

pub struct ModelRequest {
    pub model: ModelConfig,
    pub discovered: Option<crate::config::DiscoveredLimits>,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub context: String,
}
pub struct ModelResponse {
    pub message: Message,
    pub usage: Usage,
    /// Set only when the provider stopped at the output token limit and at
    /// least one streamed tool call was complete; the message then carries
    /// just those complete calls, and the string is the truncation reason
    /// (it names the cut call(s)).
    pub truncated: Option<String>,
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
    session_id: Option<String>,
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

    /// Validate and retain a configured provider. Guarded HTTP clients are
    /// built only after DNS validation when a request is made.
    pub fn new(config: ProviderConfig) -> Result<Self> {
        crate::config::validate_url(&config.base_url).context("Invalid provider base URL")?;
        Ok(Self {
            config,
            session_id: None,
        })
    }

    /// Attach the local session id. OpenRouter uses `session_id` as its sticky-routing
    /// key so every turn of a session is served by the same upstream provider and
    /// keeps that provider's prompt cache warm.
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
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
        let output_cap = request.model.output_cap(request.discovered);
        tracing::debug!(
            target: "diet_soda::provider",
            model = %request.model.model,
            max_tokens = output_cap,
            config_window = ?request.model.context_window,
            discovered = ?request.discovered,
            "derived output cap"
        );
        let mut body = if anthropic {
            json!({"model":request.model.model,"system":request.system,"messages":anthropic_messages(&request.messages),"max_tokens":output_cap,"stream":true})
        } else {
            json!({"model":request.model.model,"messages":openai_messages(&request.system,&request.messages),"max_tokens":output_cap,"stream":true,"stream_options":{"include_usage":true}})
        };
        if let Some(t) = request.model.temperature {
            body["temperature"] = json!(t);
        }
        if self.config.kind == ProviderKind::Openai {
            body.as_object_mut().unwrap().remove("max_tokens");
            body["max_completion_tokens"] = json!(output_cap);
        }
        if !request.tools.is_empty() {
            body["tools"] = json!(request.tools.iter().map(|t| if anthropic { json!({"name":t.name,"description":t.description,"input_schema":t.input_schema}) } else { json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.input_schema}}) }).collect::<Vec<_>>());
        }
        if self.config.kind == ProviderKind::Openrouter {
            body["usage"] = json!({"include":true});
            if let Some(id) = &self.session_id {
                // OpenRouter accepts at most 256 characters.
                body["session_id"] = json!(id.chars().take(256).collect::<String>());
            }
            if cache::wants_prompt_cache(&request.model.model) {
                cache::apply_prompt_cache(&mut body);
            }
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
        crate::config::validate_url(&url).context("Invalid provider URL")?;
        let parsed_url = reqwest::Url::parse(&url).context("Invalid provider URL")?;
        let client = crate::tools::guarded_http_client(
            &parsed_url,
            self.config.allow_private_networks,
            None,
            cancel,
        )
        .await?;
        let http = client.post(url).json(&body);
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
            let status = response.status();
            let api_key = self
                .config
                .api_key_env
                .as_ref()
                .and_then(|env| std::env::var(env).ok());
            let body = match tokio::select! {
                _ = cancel.cancelled() => None,
                result = tokio::time::timeout(
                    Duration::from_secs(5),
                    crate::tools::read_response(response, 2000),
                ) => result
                    .ok()
                    .and_then(|inner| inner.ok())
                    .map(|(bytes, _truncated)| bytes),
            } {
                Some(bytes) => sanitize_provider_error(&bytes, api_key.as_deref()),
                None => String::new(),
            };
            if body.is_empty() {
                return Err(anyhow::Error::new(ProviderHttpError {
                    status,
                    body: String::new(),
                }));
            }
            return Err(anyhow::Error::new(ProviderHttpError { status, body }));
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
        let mut anthropic_stop_reason: Option<String> = None;
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
                        "message_delta" => {
                            update_usage(&mut usage, &value["usage"], true);
                            if let Some(reason) = value["delta"]["stop_reason"].as_str() {
                                if !reason.is_empty() {
                                    anthropic_stop_reason = Some(reason.to_owned());
                                }
                            }
                        }
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
        // A response that stopped at the max-output-token limit is never a
        // complete answer: reasoning models can spend the whole cap on thinking
        // and emit nothing, and a cut-off tool call is unusable. Surface it
        // instead of returning an empty/partial message as a success.
        let truncated = observed_finish_reasons
            .iter()
            .any(|reason| reason == "length" || reason == "max_tokens")
            || anthropic_stop_reason.as_deref() == Some("max_tokens");
        if truncated {
            let suffix = truncated_tool_calls(&calls);
            let reason = format!(
                "response truncated: the model stopped at its max output token limit ({output_cap} tokens); raise max_tokens, or context_window if the cap is derived from it (a model's advertised max output is a hard ceiling){suffix}"
            );
            // Salvage calls the token cut left whole: a call is complete when
            // its id and name are non-empty and its arguments are empty or
            // parse as JSON. When at least one such call exists, return it as
            // a successful turn — accumulated visible text plus just those
            // complete calls, built the way the success tail builds it — so
            // work that streamed completely stays usable instead of being
            // discarded with the severed call(s) `reason` still names.
            let complete: Vec<(u64, ToolCall)> = calls
                .iter()
                .filter(|(_, call)| {
                    !call.id.is_empty()
                        && !call.name.is_empty()
                        && (call.arguments.is_empty()
                            || serde_json::from_str::<Value>(&call.arguments).is_ok())
                })
                .map(|(index, call)| (*index, call.clone()))
                .collect();
            if !complete.is_empty() {
                for (index, mut call) in complete {
                    if call.arguments.is_empty() {
                        call.arguments = "{}".into();
                    }
                    if let Some(block) = blocks.get_mut(&index) {
                        block["input"] = serde_json::from_str(&call.arguments).unwrap_or(json!({}));
                    }
                    message.tool_calls.push(call);
                }
                apply_cost_estimate(&mut usage, &request.model);
                return Ok(ModelResponse {
                    message,
                    usage,
                    truncated: Some(reason),
                });
            }
            // Record the provider's final billed usage only when the protocol
            // actually completed ([DONE]/message_stop seen) and the provider
            // reported tokens; a stream without the completion event may carry
            // only partial accounting.
            return Err(if finished && usage.tokens_reported {
                let mut billed = usage.clone();
                apply_cost_estimate(&mut billed, &request.model);
                incomplete_with_usage(&message, reason, &billed)
            } else {
                incomplete(&message, reason)
            });
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
        apply_cost_estimate(&mut usage, &request.model);
        Ok(ModelResponse {
            message,
            usage,
            truncated: None,
        })
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
        usage: None,
        empty: false,
    })
}

/// Like [`incomplete`], but carries the provider's final billed [`Usage`] so
/// the engine can still record spend for a protocol-complete response that was
/// rejected (truncation).
fn incomplete_with_usage(
    partial: &Message,
    reason: impl Into<String>,
    usage: &Usage,
) -> anyhow::Error {
    let reason = reason.into();
    let safe = Message::incomplete_assistant(partial.content.clone(), reason.clone());
    anyhow::Error::new(IncompleteStreamError {
        message: safe,
        reason,
        usage: Some(usage.clone()),
        empty: false,
    })
}

/// Suffix appended to the truncation reason when the token cut severed one or
/// more streamed tool calls, naming them so the caller knows exactly which
/// calls died and how to retry. A call is incomplete when its `name` is empty
/// or its non-empty `arguments` do not parse as JSON. Names are collected in
/// stream-index order (the map iterates ascending by index) and deduplicated
/// while preserving that order. Empty when every streamed call is intact, so
/// the reason then reads exactly as it did before.
fn truncated_tool_calls(calls: &BTreeMap<u64, ToolCall>) -> String {
    let mut names: Vec<&str> = Vec::new();
    for call in calls.values() {
        let incomplete = (!call.arguments.is_empty()
            && serde_json::from_str::<Value>(&call.arguments).is_err())
            || call.name.is_empty();
        if !incomplete {
            continue;
        }
        let name = if call.name.is_empty() {
            "(unnamed tool call)"
        } else {
            call.name.as_str()
        };
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if names.is_empty() {
        return String::new();
    }
    format!(
        "; truncated tool call(s): {} — re-issue each one in smaller pieces (for write_file, split the content across several writes)",
        names.join(", ")
    )
}

/// Backfill a token-derived cost estimate when the provider reported tokens
/// but no explicit `cost`. Shared by the success path and the truncation
/// branch so a protocol-complete response that is rejected as truncated is
/// billed identically to one that is accepted.
fn apply_cost_estimate(usage: &mut Usage, model: &ModelConfig) {
    if usage.cost_microusd.is_none() && usage.tokens_reported {
        if let (Some(input), Some(output)) = (
            model.input_usd_per_million,
            model.output_usd_per_million,
        ) {
            usage.cost_microusd = Some(
                (usage.input_tokens as f64 * input + usage.output_tokens as f64 * output).round()
                    as u64,
            );
            usage.estimated = true;
        }
    }
}

/// Sanitize a provider error body for inclusion in an error message: lossy
/// UTF-8, redact the API key, replace control characters (except spaces) with
/// spaces, collapse whitespace runs, and bound the length. Empty input yields
/// an empty string so the caller can fall back to the status-only message.
fn sanitize_provider_error(bytes: &[u8], api_key: Option<&str>) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    if let Some(key) = api_key {
        if !key.is_empty() {
            text = text.replace(key, "[REDACTED]");
        }
    }
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() && c != ' ' { ' ' } else { c })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX_CHARS: usize = 2000;
    if collapsed.chars().count() > MAX_CHARS {
        let truncated: String = collapsed.chars().take(MAX_CHARS).collect();
        format!("{truncated}…")
    } else {
        collapsed
    }
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
    if let Some(n) = value["prompt_tokens_details"]["cache_write_tokens"]
        .as_u64()
        .or_else(|| value["cache_creation_input_tokens"].as_u64())
    {
        usage.cache_write_tokens = n;
    }
    if let Some(n) = value["cost"]
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0)
    {
        usage.cost_microusd = Some((n * 1_000_000.0).round() as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::{sanitize_provider_error, truncated_tool_calls, ToolCall};
    use std::collections::BTreeMap;

    #[test]
    fn sanitize_provider_error_redacts_api_key() {
        let out = sanitize_provider_error(br#"{"error":"bad key sk-SECRET"}"#, Some("sk-SECRET"));

        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("sk-SECRET"));
    }

    #[test]
    fn sanitize_provider_error_replaces_controls_and_collapses_whitespace() {
        let out = sanitize_provider_error(b"a\n\n  b\0c", None);

        assert_eq!(out, "a b c");
        assert!(!out.chars().any(|c| c.is_control() && c != ' '));
        assert_eq!(sanitize_provider_error(b"a\n\n  b", None), "a b");
    }

    #[test]
    fn sanitize_provider_error_truncates_long_bodies() {
        let body = vec![b'x'; 5000];

        assert_eq!(sanitize_provider_error(&body, None).chars().count(), 2001);
    }

    #[test]
    fn sanitize_provider_error_handles_empty_body() {
        assert_eq!(sanitize_provider_error(b"", None), "");
    }

    #[test]
    fn sanitize_provider_error_preserves_body_without_key() {
        assert_eq!(sanitize_provider_error(br#"{"e":1}"#, None), "{\"e\":1}");
    }

    #[test]
    fn truncated_tool_calls_reports_nothing_when_all_calls_are_intact() {
        let calls = BTreeMap::from([
            (
                0,
                ToolCall {
                    id: "call-0".into(),
                    name: "read_file".into(),
                    arguments: r#"{"path":"a.txt"}"#.into(),
                },
            ),
            (
                1,
                ToolCall {
                    id: "call-1".into(),
                    name: "write_file".into(),
                    arguments: r#"{"path":"b.txt"}"#.into(),
                },
            ),
        ]);

        assert_eq!(truncated_tool_calls(&calls), "");
    }

    #[test]
    fn truncated_tool_calls_names_a_call_with_partial_json() {
        let calls = BTreeMap::from([
            (
                0,
                ToolCall {
                    id: "call-0".into(),
                    name: "read_file".into(),
                    arguments: r#"{"path":"a.txt"}"#.into(),
                },
            ),
            (
                1,
                ToolCall {
                    id: "call-1".into(),
                    name: "write_file".into(),
                    arguments: r#"{"path":"a.txt","content":"unterminated"#.into(),
                },
            ),
        ]);

        assert_eq!(
            truncated_tool_calls(&calls),
            "; truncated tool call(s): write_file — re-issue each one in smaller pieces (for write_file, split the content across several writes)"
        );
    }

    #[test]
    fn truncated_tool_calls_labels_unnamed_calls_and_deduplicates() {
        let calls = BTreeMap::from([
            (
                0,
                ToolCall {
                    id: "call-0".into(),
                    name: "write_file".into(),
                    arguments: "not json".into(),
                },
            ),
            (
                1,
                ToolCall {
                    id: "call-1".into(),
                    name: String::new(),
                    arguments: "not json".into(),
                },
            ),
            (
                2,
                ToolCall {
                    id: "call-2".into(),
                    name: "write_file".into(),
                    arguments: "still not json".into(),
                },
            ),
        ]);

        assert_eq!(
            truncated_tool_calls(&calls),
            "; truncated tool call(s): write_file, (unnamed tool call) — re-issue each one in smaller pieces (for write_file, split the content across several writes)"
        );
    }

    #[test]
    fn truncated_tool_calls_ignores_empty_arguments() {
        let calls = BTreeMap::from([(
            0,
            ToolCall {
                id: "call-0".into(),
                name: "write_file".into(),
                arguments: String::new(),
            },
        )]);

        assert_eq!(truncated_tool_calls(&calls), "");
    }
}
