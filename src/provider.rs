//! HTTP model adapters. A shared reqwest client reuses connections across turns
//! and parallel children; each request still has its own timeout/cancellation.
mod reasoning;
mod sse;
pub use sse::SseDecoder;

use crate::{
    config::{ModelConfig, ProviderConfig, ProviderKind},
    model::{Message, ToolCall, ToolSpec, UiEvent, Usage},
};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{collections::BTreeMap, time::Duration};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

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
    pub fn with_client(config: ProviderConfig, client: reqwest::Client) -> Self {
        Self { config, client }
    }
    pub fn new(config: ProviderConfig) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(config.timeout_seconds))
                .build()?,
            config,
        })
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
        let mut http = self
            .client
            .post(url)
            .timeout(Duration::from_secs(self.config.timeout_seconds))
            .json(&body);
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
            http = http.header("X-Title", "Diet Harness");
        }
        let response = tokio::select! { _ = cancel.cancelled() => bail!("Cancelled"), result = http.send() => result.context("Provider connection failed")? };
        if !response.status().is_success() {
            bail!("Provider returned HTTP {}", response.status());
        }
        let mut stream = response.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut message = Message::new("assistant", "");
        let mut calls: BTreeMap<u64, ToolCall> = BTreeMap::new();
        let mut blocks = BTreeMap::new();
        let mut reasoning_details = BTreeMap::new();
        let mut usage = Usage::default();
        let mut finished = false;
        let mut content_bytes = 0;
        loop {
            let chunk = tokio::select! { _ = cancel.cancelled() => bail!("Cancelled"), chunk = stream.next() => chunk };
            let Some(chunk) = chunk else {
                break;
            };
            for data in decoder.push(&chunk?)? {
                if data == "[DONE]" {
                    finished = true;
                    continue;
                }
                let value: Value =
                    serde_json::from_str(&data).context("Invalid provider SSE JSON")?;
                if value.get("error").is_some() || value["type"] == "error" {
                    bail!("Provider reported a streaming error");
                }
                let mut text = None;
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
                            } else {
                                text = block["text"].as_str();
                            }
                        }
                        "content_block_delta" => {
                            reasoning::append_block_delta(&mut blocks, &value)?;
                            text = value["delta"]["text"].as_str();
                            if let Some(part) = value["delta"]["partial_json"].as_str() {
                                calls
                                    .get_mut(&value["index"].as_u64().unwrap_or(0))
                                    .context("Tool delta before start")?
                                    .arguments
                                    .push_str(part);
                            }
                        }
                        _ => {}
                    }
                } else {
                    update_usage(&mut usage, &value["usage"], false);
                    if let Some(choice) = value["choices"].as_array().and_then(|a| a.first()) {
                        let delta = &choice["delta"];
                        if let Some(text) = delta["reasoning"]
                            .as_str()
                            .or_else(|| delta["reasoning_content"].as_str())
                        {
                            message
                                .reasoning
                                .get_or_insert_with(String::new)
                                .push_str(text);
                        }
                        reasoning::append_details(
                            &mut reasoning_details,
                            &delta["reasoning_details"],
                        );
                        text = delta["content"].as_str();
                        if let Some(items) = delta["tool_calls"].as_array() {
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
                    }
                }
                content_bytes += data.len();
                if content_bytes > 16_000_000 {
                    bail!("Model response exceeded 16 MB limit");
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
            bail!("Provider stream ended before completion; partial text was not added to model history");
        }
        for (index, mut call) in calls {
            if call.id.is_empty() || call.name.is_empty() {
                bail!("Incomplete tool call from provider");
            }
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
