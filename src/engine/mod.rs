//! Terminal-independent orchestration. Messages are committed once, completed
//! tool exchanges stay provider-valid, and child work shares accounting/limits.
mod dispatch;
mod scope;

pub use scope::{intersect, Scope, Selection};

use crate::{
    config::Config,
    hooks,
    mcp::McpManager,
    model::{Decision, Message, ToolCall, UiEvent},
    provider::{ModelProvider, ModelRequest, RemoteProvider},
    session::Session,
    tools::{self, Switches},
};
use anyhow::{bail, Result};
use async_recursion::async_recursion;
use futures_util::{future::BoxFuture, stream, FutureExt, StreamExt};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot, Mutex, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;

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
}

impl Engine {
    pub fn new(
        config: Config,
        mut session: Session,
        events: mpsc::UnboundedSender<UiEvent>,
    ) -> Self {
        session.add_redactions(config.secret_values());
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

    /// Only one approval is presented at a time, even when many children ask.
    /// Waiting for the UI or for another approval is always cancellable.
    pub async fn approve(
        &self,
        context: &str,
        title: String,
        detail: String,
        workflow: bool,
        cancel: &CancellationToken,
    ) -> Result<Decision> {
        let _approval = tokio::select! {
            _ = cancel.cancelled() => return Ok(Decision::Abort),
            guard = self.approval_lock.lock() => guard,
        };
        let (reply, receive) = oneshot::channel();
        self.events
            .send(UiEvent::Approval {
                title: title.clone(),
                detail,
                workflow,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("Approval interface is unavailable"))?;
        let decision = tokio::select! {
            _ = cancel.cancelled() => Decision::Abort,
            result = receive => result.unwrap_or(Decision::Abort),
        };
        if decision == Decision::Abort {
            cancel.cancel();
        }
        self.session.lock().await.append(
            "approval",
            context,
            json!({"title":title,"decision":format!("{decision:?}")}),
        )?;
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
        let result = {
            let work = self.conversation_inner(scope, history, input, &token);
            tokio::pin!(work);
            tokio::select! {
                result = &mut work => result,
                _ = tokio::time::sleep(Duration::from_secs(scope.timeout_seconds)) => {
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
            let missing: Vec<_> = history
                .iter()
                .flat_map(|m| &m.tool_calls)
                .filter(|call| {
                    !history
                        .iter()
                        .any(|m| m.tool_call_id.as_ref() == Some(&call.id))
                })
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
        for _ in 0..scope.max_turns.unwrap_or(usize::MAX) {
            if cancel.is_cancelled() {
                bail!("Cancelled");
            }
            let config = self.config.read().await.clone();
            let registered = self.available(scope, cancel).await?;
            self.session.lock().await.append("model_request",&scope.context,json!({"provider":scope.model.provider,"model":scope.model.model,"effort":scope.model.reasoning.as_ref().and_then(|r| r.effort)}))?;
            hooks::emit(
                &config,
                "before_model",
                json!({"context":scope.context,"model":scope.model,"messages":history}),
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
                provider
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
                    .await?
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
            let hook_result = hooks::emit(
                &config,
                "after_model",
                json!({"context":scope.context,"message":response.message,"usage":response.usage}),
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
                                Ok(()) => self.invoke(scope, call, &registered, cancel).await,
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
