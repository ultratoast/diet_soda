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
    /// Set only when the provider stream ended before the protocol completion
    /// event and we surfaced the partial result as a transcript marker. The
    /// fields here are display-only; the model request history, the engine's
    /// tool loop, and the wire serialization all ignore them so partial
    /// assistant output can never be re-sent or executed. Absent on every
    /// well-formed message, so older readers (and old sessions on disk) parse
    /// the same way as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incomplete: Option<Incomplete>,
}

/// Display-only marker for an assistant message that did not finish cleanly.
/// Carries safe text already scrubbed of provider reasoning detail and tool
/// call fragments; the engine never pushes it into model request history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Incomplete {
    /// Short, redacted reason safe to show in the transcript (e.g.
    /// `"idle timeout after 5s"`, `"connection closed before completion"`).
    #[serde(default)]
    pub reason: String,
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
            incomplete: None,
        }
    }
    pub fn tool(id: &str, content: impl Into<String>) -> Self {
        let mut message = Self::new("tool", content);
        message.tool_call_id = Some(id.into());
        message
    }
    /// Build the partial assistant record persisted when a stream ends before
    /// its protocol completion. The call drops any tool-call fragments and
    /// reasoning metadata, clears native content, and stores a redacted
    /// reason; the caller is responsible for filling `content` with the safe
    /// partial text the user should see.
    pub fn incomplete_assistant(content: impl Into<String>, reason: impl Into<String>) -> Self {
        let mut message = Self::new("assistant", content);
        message.incomplete = Some(Incomplete {
            reason: reason.into(),
        });
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

// -------------------------------------------------------------------------
// Activity lifecycle. The kinds are additive and tolerant of older
// readers: producers serialize an `ActivityEvent` payload, the session stores
// it under `{"type":"activity", ...}`, and consumers that don't yet know the
// kind simply skip the line via the existing `_ => {}` arm.
//
// `kind` and `phase` use plain `#[serde(rename_all = "snake_case")]` strings
// so the on-disk shape is stable without an externally-versioned schema.
// Optional fields use `skip_serializing_if = "Option::is_none"` so legacy
// readers never see absent-vs-null drift, and unknown future fields are
// accepted via the default serde derive (no deny-unknown-fields).
//
// Producers: the tool lifecycle in `src/engine/dispatch.rs` emits paired
// `Start`/`End` records with `ActivityKind::Tool`. Subagent and workflow
// producers are wired up the same way through `Engine::emit_activity`.
// -------------------------------------------------------------------------

/// What produced the activity record. Used by the lifecycle producers:
/// tool dispatch emits `Tool`, subagent scopes emit `Subagent`, and
/// workflow steps emit `WorkflowStep`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    Tool,
    Subagent,
    WorkflowStep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityPhase {
    Start,
    End,
}

/// Final state once the matching `End` arrives. A running record is the
/// unmatched `Start` itself; the persisted status is only meaningful on end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityStatus {
    Success,
    Error,
    Cancelled,
    Denied,
}

/// One lifecycle record. Producers always serialize `kind` and `phase`; the
/// optional fields may be omitted by writers and ignored by readers. A
/// companion `End` event reuses `id` so a `Start` without a matching `End`
/// can be flagged as unmatched when the session is reopened.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityEvent {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub context: String,
    pub kind: ActivityKind,
    pub phase: ActivityPhase,
    /// Human-readable label safe for display. Producers should sanitize
    /// arguments/paths through the same `tools::describe_call` summary the
    /// approval dialog uses so the transcript never embeds raw secrets.
    pub title: String,
    /// Optional upstream correlation id (provider request id, workflow run
    /// id, etc.). Omitted when there is no corresponding provider call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ActivityStatus>,
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
    /// Human-readable status line. `context` identifies the source
    /// (`main`, a subagent, or a workflow) so the UI can attribute it.
    Status {
        context: String,
        text: String,
    },
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
    /// Lifecycle notification for an activity record (start/end with optional
    /// status). The tool dispatch lifecycle emits these via
    /// `Engine::emit_activity`; subagent and workflow producers go through
    /// the same helper so the live UI channel and the on-disk log share
    /// one source.
    Activity(ActivityEvent),
}
