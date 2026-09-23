//! Terminal-independent orchestration. Messages are committed once, completed
//! tool exchanges stay provider-valid, and child work shares accounting/limits.
mod budget;
mod dispatch;
mod scope;

pub use budget::Budget;
pub use dispatch::RegisteredTool;
pub use scope::{intersect, Scope, Selection};

use crate::{
    config::Config,
    hooks,
    mcp::McpManager,
    model::{ActivityEvent, Decision, Message, ToolCall, UiEvent},
    provider::{self, ModelProvider, ModelRequest, RemoteProvider},
    session::Session,
    text::is_unsafe_terminal_char,
    tools::{self, Switches},
};
use anyhow::{bail, Result};
use async_recursion::async_recursion;
use futures_util::{future::BoxFuture, stream, FutureExt, StreamExt};
use serde_json::{json, Value};
use std::{collections::HashSet, sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot, Mutex, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;

/// Hard cap on the length of any activity record's `title` field, measured
/// in Unicode scalar values (i.e. `chars().count()`). The bound keeps
/// multi-byte UTF-8 sequences from pushing the on-disk title past the
/// 160-scalar ceiling, which the transcript renders as one cell per
/// scalar regardless of byte width.
pub(super) const ACTIVITY_TITLE_MAX_CHARS: usize = 160;

/// Ellipsis character appended when an activity title has been truncated
/// to fit the 160-scalar budget. Using a single Unicode scalar keeps the
/// cap simple and renders consistently across terminals.
pub(super) const ACTIVITY_TITLE_ELLIPSIS: char = '\u{2026}';

/// Build the redacted, display-safe body of an activity record title.
///
/// Sanitization rules shared by every Wave 2 lifecycle producer (tool
/// dispatch, subagent, workflow step):
///   * control characters, newlines, and tabs become spaces in **both**
///     `prefix` and `body`,
///   * runs of whitespace collapse to a single space across the joined
///     title, so a caller's `"…: "` separator survives as exactly one
///     space,
///   * the result is trimmed to fit a 160-Unicode-scalar budget for the
///     **entire** title (including `prefix` and a trailing `…` when
///     truncation occurs).
///
/// `prefix` is sanitized and included in the size budget so the caller
/// knows exactly how much room the body has. The bound is in Unicode
/// scalar values (`chars().count()`), not bytes, so multi-byte UTF-8
/// sequences cannot push the on-disk title past the limit. When the
/// sanitized title already exceeds the budget it is truncated
/// deterministically with an ellipsis; this guarantees the title is
/// always bounded by `ACTIVITY_TITLE_MAX_CHARS` regardless of caller
/// input.
pub(super) fn sanitize_activity_title(prefix: &str, body: &str) -> String {
    const MAX: usize = ACTIVITY_TITLE_MAX_CHARS;
    // Sanitize the prefix and body as one unit so a separator space that
    // straddles the boundary (for example `"tool x: "` + `"cmd"`) is
    // collapsed exactly once rather than lost or doubled.
    let sanitized: String = format!("{prefix}{body}")
        .chars()
        .map(|c| if is_unsafe_terminal_char(c) { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let chars: Vec<char> = sanitized.chars().collect();
    if chars.len() <= MAX {
        return sanitized;
    }
    // One scalar is reserved for the ellipsis, so the truncated title is
    // exactly `MAX` scalars long.
    let mut out: String = chars.into_iter().take(MAX - 1).collect();
    out.push(ACTIVITY_TITLE_ELLIPSIS);
    out
}

/// Cheap clones share services, not conversations. A child owns its history.
#[derive(Clone)]
pub struct Engine {
    pub config: Arc<RwLock<Config>>,
    pub session: Arc<Mutex<Session>>,
    pub switches: Arc<RwLock<Switches>>,
    pub mcp: Arc<McpManager>,
    pub events: mpsc::UnboundedSender<UiEvent>,
    client: reqwest::Client,
    approval_lock: Arc<Mutex<()>>,
    child_slots: Arc<RwLock<Arc<Semaphore>>>,
    /// Directories approved for outside reads this session. Shared by children.
    outside_dirs: Arc<Mutex<std::collections::HashSet<std::path::PathBuf>>>,
    /// Command-family approvals granted for the current session. Shared by children.
    session_grants: Arc<Mutex<SessionGrants>>,
}

struct SessionGrants {
    session_id: String,
    keys: HashSet<String>,
}

struct ApprovalOptions<'a> {
    workflow: bool,
    activity_id: Option<&'a str>,
    persist_key: Option<String>,
}

impl Engine {
    pub fn new(
        config: Config,
        mut session: Session,
        events: mpsc::UnboundedSender<UiEvent>,
    ) -> Self {
        session.add_redactions(config.secret_values());
        let session_id = session.id.clone();
        let slots = Semaphore::new(config.max_parallel_subagents);
        Self {
            config: Arc::new(RwLock::new(config)),
            session: Arc::new(Mutex::new(session)),
            switches: Arc::new(RwLock::new(Switches::default())),
            mcp: Arc::new(McpManager::default()),
            events,
            client: reqwest::Client::new(),
            approval_lock: Arc::new(Mutex::new(())),
            child_slots: Arc::new(RwLock::new(Arc::new(slots))),
            outside_dirs: Arc::new(Mutex::new(std::collections::HashSet::new())),
            session_grants: Arc::new(Mutex::new(SessionGrants {
                session_id,
                keys: HashSet::new(),
            })),
        }
    }

    /// Called while idle after a config edit/reload. HTTP connection pools survive.
    pub async fn replace_config(&self, config: Config, reset_switches: bool) {
        self.session
            .lock()
            .await
            .add_redactions(config.secret_values());
        *self.child_slots.write().await = Arc::new(Semaphore::new(config.max_parallel_subagents));
        *self.config.write().await = config;
        if reset_switches {
            *self.switches.write().await = Switches::default();
        }
    }

    pub async fn reset_session_grants(&self, session_id: &str) {
        let mut grants = self.session_grants.lock().await;
        grants.session_id = session_id.to_owned();
        grants.keys.clear();
    }

    pub async fn has_session_grant(&self, key: &str) -> bool {
        let session_id = self.session.lock().await.id.clone();
        let grants = self.session_grants.lock().await;
        grants.session_id == session_id && grants.keys.contains(key)
    }

    pub async fn list_models(
        &self,
        provider: crate::config::ProviderConfig,
    ) -> Result<Vec<crate::provider::CatalogModel>> {
        RemoteProvider::with_client(provider, self.client.clone())
            .list_models()
            .await
    }

    pub async fn turn(
        &self,
        input: String,
        selection: Selection,
        cancel: CancellationToken,
    ) -> Result<String> {
        let scope = self.scope(&selection, "main", None).await?;
        let mut history = self.session.lock().await.messages.clone();
        self.conversation(&scope, &mut history, input, &cancel)
            .await
    }

    pub async fn record(&self, context: &str, message: Message) -> Result<()> {
        self.session
            .lock()
            .await
            .record_message(context, message.clone())?;
        let _ = self.events.send(UiEvent::Message {
            context: context.into(),
            message,
        });
        Ok(())
    }

    /// Atomically follow the activity contract: persist the event via
    /// [`Session::record_activity`] (the authoritative store) and then
    /// emit the matching [`UiEvent::Activity`] on the live event channel.
    ///
    /// Persistence failures surface as `Err` so callers can abort the
    /// enclosing lifecycle; the live `UiEvent::Activity` is best-effort
    /// (the TUI is allowed to drop a single activity record, but the
    /// persisted JSONL is not). This is the single helper every Wave 2
    /// lifecycle producer (tool dispatch, subagent, workflow-step)
    /// routes through so the two surfaces cannot diverge.
    pub async fn emit_activity(&self, event: ActivityEvent) -> Result<()> {
        self.session.lock().await.record_activity(event.clone())?;
        let _ = self.events.send(UiEvent::Activity(event));
        Ok(())
    }

    /// Only one approval is presented at a time, even when many children ask.
    /// Waiting for the UI or for another approval is always cancellable.
    ///
    /// Persists an `approval` event whose data shape is backward-compatible:
    /// `{"title":..,"decision":..}` with an optional `activity_id` field
    /// that is only present when a lifecycle producer supplied one. Old
    /// readers that only know the two legacy keys continue to parse it.
    pub async fn approve(
        &self,
        context: &str,
        title: String,
        detail: String,
        workflow: bool,
        cancel: &CancellationToken,
    ) -> Result<Decision> {
        self.approve_with_activity(context, title, detail, workflow, None, cancel)
            .await
    }

    /// Variant of [`approve`] that records the id of the activity record
    /// that requested the prompt. The tool and workflow approval call sites
    /// thread `Scope.activity_id` through here so persisted `approval`
    /// events correlate with their originating lifecycle when the producer
    /// populated the field. The existing `approve` continues to work
    /// unchanged because `activity_id` defaults to `None` and is omitted
    /// from the persisted JSON when absent.
    pub async fn approve_with_activity(
        &self,
        context: &str,
        title: String,
        detail: String,
        workflow: bool,
        activity_id: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Decision> {
        self.approve_internal(
            context,
            title,
            detail,
            ApprovalOptions {
                workflow,
                activity_id,
                persist_key: None,
            },
            cancel,
        )
        .await
    }

    pub async fn approve_command_with_activity(
        &self,
        context: &str,
        title: String,
        detail: String,
        persist_key: String,
        activity_id: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Decision> {
        self.approve_internal(
            context,
            title,
            detail,
            ApprovalOptions {
                workflow: false,
                activity_id,
                persist_key: Some(persist_key),
            },
            cancel,
        )
        .await
    }

    async fn approve_internal(
        &self,
        context: &str,
        title: String,
        detail: String,
        options: ApprovalOptions<'_>,
        cancel: &CancellationToken,
    ) -> Result<Decision> {
        let requested_session_id = self.session.lock().await.id.clone();
        let _approval = tokio::select! {
            _ = cancel.cancelled() => return Ok(Decision::Abort),
            guard = self.approval_lock.lock() => guard,
        };
        if let Some(key) = &options.persist_key {
            let grants = self.session_grants.lock().await;
            if grants.session_id == requested_session_id && grants.keys.contains(key) {
                return Ok(Decision::Approve);
            }
        }
        let (reply, receive) = oneshot::channel();
        self.events
            .send(UiEvent::Approval {
                title: title.clone(),
                detail,
                workflow: options.workflow,
                persist_allowed: options.persist_key.is_some(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("Approval interface is unavailable"))?;
        let mut decision = tokio::select! {
            _ = cancel.cancelled() => Decision::Abort,
            result = receive => result.unwrap_or(Decision::Abort),
        };
        let persist_grant = if decision == Decision::ApprovePersist {
            match options.persist_key {
                Some(key) => Some(key),
                None => {
                    decision = Decision::Reject;
                    None
                }
            }
        } else {
            None
        };
        if decision == Decision::Abort {
            cancel.cancel();
        }
        let mut payload = json!({"title":title,"decision":format!("{decision:?}")});
        if let Some(id) = options.activity_id {
            payload["activity_id"] = json!(id);
        }
        self.session
            .lock()
            .await
            .append("approval", context, payload)?;
        if let Some(key) = persist_grant {
            let mut grants = self.session_grants.lock().await;
            if grants.session_id == requested_session_id {
                grants.keys.insert(key);
            }
        }
        Ok(decision)
    }

    #[async_recursion]
    pub async fn conversation(
        &self,
        scope: &Scope,
        history: &mut Vec<Message>,
        input: String,
        cancel: &CancellationToken,
    ) -> Result<String> {
        let token = cancel.child_token();
        // Install a fresh execution budget for this conversation, chained to the
        // inherited parent budget (if any) so pausing a descendant freezes its
        // ancestors. The scoped clone carries the budget for the duration of
        // this call; the caller's scope is left untouched.
        let budget = Budget::new(
            Duration::from_secs(scope.timeout_seconds),
            scope.budget.clone(),
        );
        let scope = &Scope {
            budget: Some(budget.clone()),
            ..scope.clone()
        };
        let result = {
            let work = self.conversation_inner(scope, history, input, &token);
            tokio::pin!(work);
            tokio::select! {
                result = &mut work => result,
                _ = budget.expired() => {
                    // Cancel cooperatively instead of dropping an MCP exchange midway.
                    token.cancel();
                    let _ = work.await;
                    Err(anyhow::anyhow!("Agent execution timed out"))
                },
            }
        };
        token.cancel();
        if result.is_err() {
            // A child's failure must never terminate a sibling's MCP calls.
            if scope.depth == 0 {
                self.mcp.shutdown().await;
            }
            let answered: HashSet<&str> = history
                .iter()
                .filter_map(|m| m.tool_call_id.as_deref())
                .collect();
            let missing: Vec<_> = history
                .iter()
                .flat_map(|m| &m.tool_calls)
                .filter(|call| !answered.contains(call.id.as_str()))
                .map(|call| call.id.clone())
                .collect();
            for id in missing {
                let message =
                    Message::tool(&id, "Execution interrupted before a result was recorded.");
                self.record(&scope.context, message.clone()).await?;
                history.push(message);
            }
        }
        self.session.lock().await.checkpoint()?;
        result
    }

    async fn conversation_inner(
        &self,
        scope: &Scope,
        history: &mut Vec<Message>,
        input: String,
        cancel: &CancellationToken,
    ) -> Result<String> {
        let user = Message::new("user", input);
        self.record(&scope.context, user.clone()).await?;
        history.push(user);
        let config = self.config.read().await.clone();
        for _ in 0..scope.max_turns.unwrap_or(usize::MAX) {
            if cancel.is_cancelled() {
                bail!("Cancelled");
            }
            let registered = self.available_with_config(scope, &config, cancel).await?;
            self.session.lock().await.append("model_request",&scope.context,json!({"provider":scope.model.provider,"model":scope.model.model,"effort":scope.model.reasoning.as_ref().and_then(|r| r.effort)}))?;
            hooks::emit_lazy(
                &config,
                "before_model",
                || json!({"context":scope.context,"model":scope.model,"messages":history}),
                cancel,
            )
            .await?;
            let provider = RemoteProvider::with_client(
                config.providers[&scope.model.provider].clone(),
                self.client.clone(),
            );
            let _ = self.events.send(UiEvent::Model {
                context: scope.context.clone(),
                provider: scope.model.provider.clone(),
                model: scope.model.model.clone(),
                effort: scope.model.reasoning.as_ref().and_then(|r| r.effort),
            });
            let response = {
                let _slot = self.child_slot(scope, cancel).await?;
                match provider
                    .stream(
                        ModelRequest {
                            model: scope.model.clone(),
                            system: scope.system.clone(),
                            messages: history.clone(),
                            tools: registered.iter().map(|t| t.spec.clone()).collect(),
                            context: scope.context.clone(),
                        },
                        &self.events,
                        cancel,
                    )
                    .await
                {
                    Ok(response) => response,
                    Err(error) => {
                        if let Some(partial) =
                            error.downcast_ref::<provider::IncompleteStreamError>()
                        {
                            // The provider stream ended before its
                            // protocol completion event. The error
                            // already carries a scrubbed partial assistant
                            // message; persist it as a transcript marker
                            // in place of the live stream so the user
                            // sees what was produced, but do not record
                            // usage/spend (we have no reliable final
                            // usage) and do not push it into the model
                            // request history. Re-emitting the partial as
                            // a regular `Message` event also replaces the
                            // live streaming entry in the TUI.
                            let _ = self.events.send(UiEvent::Message {
                                context: scope.context.clone(),
                                message: partial.message.clone(),
                            });
                            let mut session = self.session.lock().await;
                            session.record_message(&scope.context, partial.message.clone())?;
                            return Err(error);
                        }
                        return Err(error);
                    }
                }
            };
            {
                let mut session = self.session.lock().await;
                session.usage(&scope.context, &response.usage)?;
                let _ = self.events.send(UiEvent::Spend(session.spend.clone()));
            }
            let _ = self.events.send(UiEvent::Context {
                context: scope.context.clone(),
                tokens: response
                    .usage
                    .input_tokens
                    .saturating_add(response.usage.output_tokens),
            });
            self.record(&scope.context, response.message.clone())
                .await?;
            history.push(response.message.clone());
            let hook_result = hooks::emit_lazy(
                &config,
                "after_model",
                || json!({"context":scope.context,"message":response.message,"usage":response.usage}),
                cancel,
            )
            .await;
            if response.message.tool_calls.is_empty() {
                hook_result?;
                return Ok(response.message.content);
            }

            // Only delegation calls are concurrent. Ordinary side-effecting tools
            // retain model order; results are committed in that same order.
            let calls = &response.message.tool_calls;
            let mut index = 0;
            while index < calls.len() {
                let end = if hook_result.is_ok() && calls[index].name == "delegate" {
                    index
                        + calls[index..]
                            .iter()
                            .take_while(|c| c.name == "delegate")
                            .count()
                } else {
                    index + 1
                };
                let mut pending = Vec::new();
                for call in &calls[index..end] {
                    pending.push(
                        async {
                            match &hook_result {
                                Ok(()) => {
                                    self.invoke_with_config(
                                        scope,
                                        call,
                                        &registered,
                                        &config,
                                        cancel,
                                    )
                                    .await
                                }
                                Err(error) => {
                                    Err(anyhow::anyhow!("after_model hook failed: {error}"))
                                }
                            }
                        }
                        .boxed(),
                    );
                }
                let results = parallel_ordered(pending, config.max_parallel_subagents).await;
                for (call, result) in calls[index..end].iter().zip(results) {
                    self.tool_result(scope, history, call, result).await?;
                }
                index = end;
            }
            hook_result?;
        }
        bail!(
            "Maximum model turns reached ({})",
            scope.max_turns.unwrap_or(25)
        )
    }

    async fn tool_result(
        &self,
        scope: &Scope,
        history: &mut Vec<Message>,
        call: &ToolCall,
        result: Result<Value>,
    ) -> Result<()> {
        let value = match result {
            Ok(value) => value,
            // Provider-visible errors keep the failing call attached: spawn and
            // transport errors often omit the command, and the model still needs
            // to know exactly what failed.
            Err(error) => {
                let args = serde_json::from_str::<Value>(&call.arguments)
                    .unwrap_or_else(|_| Value::String(call.arguments.clone()));
                json!({
                    "error": format!("{error:#}"),
                    "tool": call.name,
                    "call": tools::describe_call(&call.name, &args),
                })
            }
        };
        let message = Message::tool(&call.id, tools::truncate(&value.to_string(), 100_000));
        self.record(&scope.context, message.clone()).await?;
        history.push(message);
        Ok(())
    }

    /// Permits cover active child work, never a parent waiting for delegation.
    /// Holding a permit across recursive delegation would deadlock at the limit.
    async fn child_slot(
        &self,
        scope: &Scope,
        cancel: &CancellationToken,
    ) -> Result<Option<tokio::sync::OwnedSemaphorePermit>> {
        if scope.depth == 0 {
            return Ok(None);
        }
        let slots = self.child_slots.read().await.clone();
        tokio::select! {
            _ = cancel.cancelled() => bail!("Cancelled"),
            permit = slots.acquire_owned() => Ok(Some(permit?)),
        }
    }
}

/// Keep workers busy even if the earliest task is slow, then restore input order
/// for the parent's tool results. The only buffering is the bounded task results.
fn parallel_ordered<'a, T: Send + 'a>(
    pending: Vec<BoxFuture<'a, T>>,
    limit: usize,
) -> BoxFuture<'a, Vec<T>> {
    async move {
        let mut pending = pending.into_iter().enumerate();
        let mut running: stream::FuturesUnordered<BoxFuture<'a, (usize, T)>> =
            stream::FuturesUnordered::new();
        for (index, work) in pending.by_ref().take(limit) {
            running.push(async move { (index, work.await) }.boxed());
        }
        let mut results = Vec::new();
        while let Some(result) = running.next().await {
            results.push(result);
            if let Some((index, work)) = pending.next() {
                running.push(async move { (index, work.await) }.boxed());
            }
        }
        results.sort_unstable_by_key(|(index, _)| *index);
        results.into_iter().map(|(_, result)| result).collect()
    }
    .boxed()
}
