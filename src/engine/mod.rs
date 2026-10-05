//! Terminal-independent orchestration. Messages are committed once, completed
//! tool exchanges stay provider-valid, and child work shares accounting/limits.
mod budget;
pub mod context_budget;
mod dispatch;
mod failure;
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
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
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

/// User-role rescue note appended when a response is cut off at the output
/// token limit before it completed. Declared once at module level so both
/// injection sites cannot drift in wording; [`retry_note`] selects between
/// this note and [`STREAM_FAILURE_NOTE`] based on the provider's reported
/// reason, and the in-turn truncated-response path appends the provider's
/// truncation reason (which names the cut call(s)) after this text.
const RETRY_NOTE: &str = "Your previous response was truncated at the output token limit before it completed. The partial output was kept only as a transcript marker, so you did not see it. Re-issue the affected tool call in smaller pieces so the next response fits within the output limit: for a large write_file, write the first chunk normally and each remaining chunk with `\"append\": true` (a shell heredoc also works).";

/// User-role rescue note appended when the provider stream failed for a
/// reason OTHER than an output-limit truncation (idle timeout, transport
/// error, malformed tool call, and so on). `{reason}` is filled with the
/// provider's reported reason. Kept separate from [`RETRY_NOTE`] so a
/// transient stream failure is not misdescribed as an output-token limit.
const STREAM_FAILURE_NOTE: &str = "The provider stream failed before your previous response completed ({reason}). The partial output was kept only as a transcript marker, so you did not see it. Continue the task and re-issue any interrupted tool call.";

/// Provider stream-failure reasons that are safe to retry exactly once.
/// Deliberately an allow-list: cooperative cancellation, header/idle
/// timeout, a 16 MB overflow, and empty turns are NOT retried, because a
/// retry would either ignore the user's cancellation or needlessly repeat a
/// deterministic failure.
const RETRYABLE_STREAM_REASONS: &[&str] = &[
    "stream ended before completion event",
    "incomplete tool call from provider",
    "provider reported a streaming error",
];

/// Select the rescue note for an incomplete response. Output-limit
/// truncations (the provider's reason names the max output token limit) use
/// [`RETRY_NOTE`], which asks for smaller tool-call retries; every other
/// reason uses [`STREAM_FAILURE_NOTE`] with the reason substituted in.
fn retry_note(reason: &str) -> String {
    if reason.contains("max output token limit") {
        RETRY_NOTE.to_owned()
    } else {
        STREAM_FAILURE_NOTE.replace("{reason}", reason)
    }
}

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
    approval_lock: Arc<Mutex<()>>,
    child_slots: Arc<RwLock<Arc<Semaphore>>>,
    /// Directories approved for outside reads this session. Shared by children.
    outside_dirs: Arc<Mutex<std::collections::HashSet<std::path::PathBuf>>>,
    /// Command-family approvals granted for the current session. Shared by children.
    session_grants: Arc<Mutex<SessionGrants>>,
    /// Per-model limits discovered from provider catalogs, keyed by
    /// (provider base_url, model id). Filled lazily by `list_models` (the
    /// /model picker path) and READ — never fetched — on the turn hot path, so
    /// it adds no latency or network dependency to a request. Shared across
    /// clones (all `Arc`), so a model is looked up at most once per Engine
    /// lineage. A missing key means the model has not been looked up yet;
    /// `list_models` stores `Some(limits)` for every catalog entry (limits may
    /// themselves be `None` when the catalog advertises none).
    discovered_limits: Arc<
        Mutex<HashMap<(String, String), Option<crate::config::DiscoveredLimits>>>,
    >,
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
            approval_lock: Arc::new(Mutex::new(())),
            child_slots: Arc::new(RwLock::new(Arc::new(slots))),
            outside_dirs: Arc::new(Mutex::new(std::collections::HashSet::new())),
            session_grants: Arc::new(Mutex::new(SessionGrants {
                session_id,
                keys: HashSet::new(),
            })),
            discovered_limits: Arc::new(Mutex::new(HashMap::new())),
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
        let base_url = provider.base_url.clone();
        let models = RemoteProvider::new(provider)?.list_models().await?;
        {
            let mut cache = self.discovered_limits.lock().await;
            for model in &models {
                cache.insert(
                    (base_url.clone(), model.id.clone()),
                    Some(crate::config::DiscoveredLimits {
                        context_window: model.context_window,
                        max_output: model.max_output,
                    }),
                );
            }
        }
        Ok(models)
    }

    /// Fill the per-model limits cache from every configured provider's model
    /// catalog. Best-effort: nothing is held locked across a network await. A
    /// provider whose API key env var is unset is skipped silently (normal
    /// configuration, not a malfunction); every other failure is logged with
    /// `tracing::warn!`, and a failure on the default model's provider also
    /// surfaces a user-visible `UiEvent::Status`. A no-op when
    /// `discover_model_limits` is false. Run once at startup; `/model`
    /// refreshes the same cache.
    pub async fn prefetch_limits(&self) {
        let (providers, default_provider) = {
            let config = self.config.read().await;
            if !config.discover_model_limits {
                return;
            }
            let providers: Vec<(String, crate::config::ProviderConfig)> = config
                .providers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            (providers, config.model.provider.clone())
        };
        futures_util::future::join_all(providers.into_iter().map(|(name, provider)| {
            let is_default = name == default_provider;
            async move {
                if let Some(env) = &provider.api_key_env {
                    if std::env::var(env).is_err() {
                        tracing::warn!(
                            provider = %name,
                            env = %env,
                            "model catalog discovery skipped: API key env var is not set"
                        );
                        return;
                    }
                }
                if let Err(e) = self.list_models(provider.clone()).await {
                    tracing::warn!(
                        provider = %name,
                        error = %e,
                        "model catalog discovery failed; falling back to configured global limits"
                    );
                    if is_default {
                        let _ = self.events.send(UiEvent::Status {
                            context: "main".into(),
                            text: format!(
                                "model discovery failed for provider {name}: {e} (falling back to configured global limits)"
                            ),
                        });
                    }
                }
            }
        }))
        .await;
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
        // Declared once before the turn loop: a second truncated response in
        // the same turn still runs its complete calls but appends no repeat
        // note.
        let mut truncation_noted = false;
        let mut bytes_per_token = scope.model.bytes_per_token;
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
            let session_id = self.session.lock().await.id.clone();
            let provider = RemoteProvider::new(config.providers[&scope.model.provider].clone())?
                .with_session_id(session_id);
            let discovered = match config.providers.get(&scope.model.provider) {
                Some(provider_config) => {
                    let key = (provider_config.base_url.clone(), scope.model.model.clone());
                    let cache = self.discovered_limits.lock().await;
                    cache.get(&key).copied().flatten()
                }
                None => None,
            };
            let _ = self.events.send(UiEvent::Model {
                context: scope.context.clone(),
                provider: scope.model.provider.clone(),
                model: scope.model.model.clone(),
                effort: scope.model.reasoning.as_ref().and_then(|r| r.effort),
            });
            let (context_limit, output_cap) = scope.model.effective_limits(
                config.max_output_tokens,
                config.max_context_tokens,
                discovered,
            );
            let tools_bytes = serde_json::to_string(
                &registered.iter().map(|t| t.spec.clone()).collect::<Vec<_>>(),
            )
            .map(|s| s.len())
            .unwrap_or(0);
            let system_and_tools_bytes = scope.system.len() + tools_bytes;
            let budget = context_budget::budget_bytes(
                context_limit,
                output_cap,
                system_and_tools_bytes,
                bytes_per_token,
            );
            let mut request_messages = history.clone();
            let report = context_budget::trim(
                &scope.system,
                &mut request_messages,
                tools_bytes,
                budget,
                bytes_per_token,
            );
            if report.irreducible {
                return Err(anyhow::Error::new(context_budget::ContextBudgetExceeded {
                    estimated: report.estimated_after,
                    budget: report.budget_bytes,
                }));
            }
            if report.collapsed > 0 || report.cleared_reasoning > 0 {
                self.session.lock().await.append(
                    "context_trim",
                    &scope.context,
                    json!({"estimated_before": report.estimated_before, "estimated_after": report.estimated_after, "budget": report.budget_bytes, "collapsed": report.collapsed, "cleared_reasoning": report.cleared_reasoning}),
                )?;
            }
            let response = {
                let mut attempt: u8 = 0;
                // Set once per model iteration: a single transient stream
                // failure may be retried, but not a second time.
                let mut transient_retried = false;
                loop {
                    attempt += 1;
                    let _slot = self.child_slot(scope, cancel).await?;
                    let outcome = provider
                        .stream(
                            ModelRequest {
                                model: scope.model.clone(),
                                output_cap,
                                system: scope.system.clone(),
                                messages: request_messages.clone(),
                                tools: registered.iter().map(|t| t.spec.clone()).collect(),
                                context: scope.context.clone(),
                            },
                            &self.events,
                            cancel,
                        )
                        .await;
                    match outcome {
                        Ok(response) => break response,
                        Err(error) => {
                            // One-shot reactive recovery: when the provider
                            // rejects the request as context overflow on the
                            // first attempt, re-trim to a tighter budget and
                            // retry exactly once. Anything else fails as before.
                            if attempt == 1
                                && failure::classify(&error) == "provider_context_overflow"
                            {
                                let sent_estimate = context_budget::estimate_bytes(
                                    &scope.system,
                                    &request_messages,
                                    tools_bytes,
                                );
                                let tighter = (budget as f64 * 0.6) as usize;
                                let retry_report = context_budget::trim(
                                    &scope.system,
                                    &mut request_messages,
                                    tools_bytes,
                                    tighter,
                                    bytes_per_token,
                                );
                                // Retry only when the tighter trim actually
                                // shrank the request: re-sending an identical
                                // payload would just earn the same rejection.
                                if retry_report.estimated_after < sent_estimate
                                    && !retry_report.irreducible
                                {
                                    if retry_report.collapsed
                                        + retry_report.cleared_reasoning
                                        > 0
                                    {
                                        self.session.lock().await.append(
                                            "context_trim",
                                            &scope.context,
                                            json!({
                                                "retry": true,
                                                "estimated_before": retry_report.estimated_before,
                                                "estimated_after": retry_report.estimated_after,
                                                "budget": retry_report.budget_bytes,
                                                "collapsed": retry_report.collapsed,
                                                "cleared_reasoning": retry_report.cleared_reasoning,
                                            }),
                                        )?;
                                    }
                                    continue;
                                }
                            }
                            // Transient provider stream failures (an
                            // allow-listed set of incomplete-stream reasons)
                            // are retried exactly once per model iteration.
                            // The retry re-issues only the provider call; the
                            // `before_model` hook and the `model_request`
                            // activity record already ran once above the loop
                            // and do not re-fire. `attempt` is rolled back so
                            // the one-shot context-overflow recovery above
                            // remains available on the retried attempt.
                            if !transient_retried && !cancel.is_cancelled() {
                                if let Some(partial) = error
                                    .downcast_ref::<provider::IncompleteStreamError>()
                                {
                                    if RETRYABLE_STREAM_REASONS
                                        .contains(&partial.reason.as_str())
                                    {
                                        transient_retried = true;
                                        if let Some(usage) = &partial.usage {
                                            let mut session = self.session.lock().await;
                                            session.usage(&scope.context, usage)?;
                                            let _ = self
                                                .events
                                                .send(UiEvent::Spend(session.spend.clone()));
                                        }
                                        tracing::warn!(
                                            context = %scope.context,
                                            reason = %partial.reason,
                                            "provider stream failed; retrying once"
                                        );
                                        let _ = self.events.send(UiEvent::Status {
                                            context: scope.context.clone(),
                                            text: format!(
                                                "provider stream failed ({}); retrying once",
                                                partial.reason
                                            ),
                                        });
                                        attempt -= 1;
                                        continue;
                                    }
                                }
                            }
                            if let Some(partial) =
                                error.downcast_ref::<provider::IncompleteStreamError>()
                            {
                                // The provider stream ended before a usable answer
                                // (protocol completion never arrived, or it did and
                                // the response was rejected as truncated). The error
                                // already carries a scrubbed partial assistant
                                // message; persist it as a transcript marker in
                                // place of the live stream so the user sees what
                                // was produced, and do not push it into the model
                                // request history. Re-emitting the partial as a
                                // regular `Message` event also replaces the live
                                // streaming entry in the TUI. When the provider
                                // still reported final billed usage (protocol
                                // complete before the rejection), record spend so
                                // those tokens are not lost.
                                let _ = self.events.send(UiEvent::Message {
                                    context: scope.context.clone(),
                                    message: partial.message.clone(),
                                });
                                if let Some(usage) = &partial.usage {
                                    let mut session = self.session.lock().await;
                                    session.usage(&scope.context, usage)?;
                                    let _ = self.events.send(UiEvent::Spend(session.spend.clone()));
                                }
                                {
                                    let mut session = self.session.lock().await;
                                    session
                                        .record_message(&scope.context, partial.message.clone())?;
                                }
                                // The marker above is display-only: `record_message`
                                // keeps it out of the request history, so the model
                                // would otherwise never learn that its reply was cut
                                // off mid tool-call. Append a user-role rescue note to
                                // the same context — recorded here rather than retried,
                                // unlike `delegate_inner`'s RESCUE_PROMPT — stating that
                                // the response hit the output token limit and that the
                                // affected tool call must be re-issued in smaller
                                // pieces. Being a normal message, the note does enter
                                // request history on the next turn; the marker's
                                // placement and the returned error are unchanged.
                                let notice = Message::new("user", retry_note(&partial.reason));
                                self.record(&scope.context, notice.clone()).await?;
                                history.push(notice);
                                return Err(error);
                            }
                            return Err(error);
                        }
                    }
                }
            };
            let sent_bytes =
                context_budget::estimate_bytes(&scope.system, &request_messages, tools_bytes);
            {
                let mut session = self.session.lock().await;
                session.usage(&scope.context, &response.usage)?;
                let _ = self.events.send(UiEvent::Spend(session.spend.clone()));
            }
            let observed = response.usage.input_tokens as f64;
            if observed > 0.0 && sent_bytes > 0 {
                // Deliberately conservative: the observed ratio may only
                // tighten the budget, never exceed the configured value.
                // Cache-token accounting (e.g. Anthropic input_tokens
                // excludes cache reads/writes) undercounts input tokens,
                // which would otherwise inflate the ratio and make the
                // budget too permissive.
                let observed_ratio = (sent_bytes as f64 / observed).clamp(1.0, 8.0);
                bytes_per_token = observed_ratio.min(scope.model.bytes_per_token);
            }
            tracing::debug!(
                target: "diet_soda::engine",
                context = %scope.context,
                estimated_input_bytes = sent_bytes,
                actual_input_tokens = response.usage.input_tokens,
                bytes_per_token = bytes_per_token,
                budget_bytes = budget,
                "context budget estimate vs actual"
            );
            let _ = self.events.send(UiEvent::Context {
                context: scope.context.clone(),
                tokens: response
                    .usage
                    .input_tokens
                    .saturating_add(response.usage.output_tokens),
            });
            // An assistant turn with no visible text and no tool calls is not an
            // answer: reasoning-only or blank replies used to complete the turn
            // as a successful empty string (the user saw a "crash"; a delegated
            // child returned `result: ""`). Surface it, and keep the blank turn
            // out of history so a retry does not replay `content: ""`.
            if response.message.tool_calls.is_empty() && response.message.content.trim().is_empty() {
                let reason = format!(
                    "model returned an empty response ({} output tokens, no tool calls) for model {}; retry the turn, or switch models if it repeats",
                    response.usage.output_tokens, scope.model.model
                );
                let partial = Message::incomplete_assistant("", reason.clone());
                let _ = self.events.send(UiEvent::Message {
                    context: scope.context.clone(),
                    message: partial.clone(),
                });
                self.session
                    .lock()
                    .await
                    .record_message(&scope.context, partial.clone())?;
                return Err(anyhow::Error::new(provider::IncompleteStreamError {
                    message: partial,
                    reason,
                    usage: None,
                    empty: true,
                }));
            }
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
            // A successful-but-truncated response already executed every tool
            // call that streamed completely (just above). Tell the model — once
            // per turn loop — which call(s) were cut so it re-issues only those,
            // in smaller pieces; a second truncation in the same turn runs its
            // complete calls but appends no repeat note.
            if let Some(reason) = &response.truncated {
                if !truncation_noted {
                    truncation_noted = true;
                    let notice = Message::new("user", format!("{} {reason}", retry_note(reason)));
                    self.record(&scope.context, notice.clone()).await?;
                    history.push(notice);
                }
            }
        }
        bail!(
            "Maximum model turns reached ({})",
            scope.max_turns.unwrap_or(scope::MAX_MODEL_TURNS)
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
                    "error_class": failure::classify(&error),
                    "tool": call.name,
                    "call": tools::describe_call(&call.name, &args),
                })
            }
        };
        let cap = self
            .config
            .read()
            .await
            .max_tool_output_bytes
            .min(tools::MAX_RESPONSE_BYTES);
        let serialized = value.to_string();
        // Tools that self-truncate (read_file, shell, web_fetch) already bound
        // their content and set `truncated: true`; re-wrapping here would
        // destroy that marker (JSON escaping alone can exceed `cap`).
        let already_truncated = value.get("truncated") == Some(&Value::Bool(true));
        let content = if already_truncated || serialized.len() <= cap {
            serialized
        } else {
            let mut head_end = cap / 2;
            while head_end > 0 && !serialized.is_char_boundary(head_end) {
                head_end -= 1;
            }
            let mut tail_start = serialized.len() - cap / 2;
            while tail_start < serialized.len() && !serialized.is_char_boundary(tail_start) {
                tail_start += 1;
            }
            json!({
                "truncated": true,
                "original_bytes": serialized.len(),
                "head": &serialized[..head_end],
                "tail": &serialized[tail_start..],
            })
            .to_string()
        };
        let message = Message::tool(&call.id, content);
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

#[cfg(test)]
mod tests {
    use super::{retry_note, RETRY_NOTE};

    #[test]
    fn retry_note_keeps_retry_note_for_output_limit_reasons() {
        let reason =
            "response truncated: the model stopped at its max output token limit (4096 tokens); raise max_output_tokens (or the model's max_tokens override)";
        assert_eq!(retry_note(reason), RETRY_NOTE);
    }

    #[test]
    fn retry_note_uses_stream_failure_note_for_other_reasons() {
        let note = retry_note("idle timeout: no chunk within 1s");
        assert!(note.contains(
            "The provider stream failed before your previous response completed"
        ));
        assert!(note.contains("idle timeout: no chunk within 1s"));
        assert!(!note.contains("{reason}"));
    }
}
