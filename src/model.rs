//! Provider-neutral messages, accounting, and UI events. Transport-only reasoning
//! metadata is retained for tool continuations, not rendered as chat text.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_details: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub native_content: Vec<Value>,
}

impl Message {
    pub fn new(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning: None,
            reasoning_details: vec![],
            native_content: vec![],
        }
    }
    pub fn tool(id: &str, content: impl Into<String>) -> Self {
        let mut message = Self::new("tool", content);
        message.tool_call_id = Some(id.into());
        message
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub tokens_reported: bool,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub cost_microusd: Option<u64>,
    pub estimated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Spend {
    pub microusd: u64,
    pub unpriced_requests: u64,
    pub estimated: bool,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

impl Spend {
    pub fn add(&mut self, usage: &Usage) {
        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
        if let Some(cost) = usage.cost_microusd {
            self.microusd = self.microusd.saturating_add(cost);
        } else {
            self.unpriced_requests += 1;
        }
        self.estimated |= usage.estimated;
    }
    pub fn display(&self) -> String {
        format!(
            "{}${:.4}{}",
            if self.estimated { "~" } else { "" },
            self.microusd as f64 / 1_000_000.0,
            if self.unpriced_requests > 0 {
                " + unknown"
            } else {
                ""
            }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Reject,
    Retry,
    Skip,
    Abort,
}

#[derive(Debug)]
pub enum UiEvent {
    Model {
        context: String,
        provider: String,
        model: String,
        effort: Option<crate::config::Effort>,
    },
    Delta {
        context: String,
        text: String,
    },
    Message {
        context: String,
        message: Message,
    },
    Status(String),
    Spend(Spend),
    /// Latest context size for one conversation, sent after each model response.
    Context {
        context: String,
        tokens: u64,
    },
    Approval {
        title: String,
        detail: String,
        workflow: bool,
        reply: oneshot::Sender<Decision>,
    },
}
