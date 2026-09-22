//! Tool advertising and dispatch. Runtime checks bracket approvals and hooks so
//! disabling a previously advertised tool takes effect before it executes.
use super::{sanitize_activity_title, Engine, Scope, Selection};
use crate::{
    config::Config,
    hooks,
    mcp::McpTool,
    model::{
        ActivityEvent, ActivityKind, ActivityPhase, ActivityStatus, Decision, Message, ToolCall,
        ToolSpec, UiEvent,
    },
    skills, tools,
};
use anyhow::{bail, Context, Result};
use futures_util::FutureExt;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// Inner outcome of a single tool invocation. The lifecycle wrapper in
/// [`Engine::invoke`] translates this into the matching `End` activity
/// status before returning the provider-visible value so callers (the
/// conversation loop, tests) keep their original control flow.
///
/// The success/denial branches are typed because they map to specific
/// `ActivityStatus` variants without any error inspection. The failure
/// branches hold the original `anyhow::Error` (and its chain/context) so
/// the wrapper does not have to format-and-recreate errors to surface
/// them — `tool_result` and other downstream formatters apply
/// `format!("{error:#}")` themselves.
pub(super) enum Outcome {
    /// Normal return — also covers hook warnings that still produce a value.
    Completed(Value),
    /// The approval dialog returned `Reject` (not `Cancel`/`Abort`). The
    /// attached `Value` is reserved for future richer payloads (e.g., a
    /// partial explanation the model could retry against) without changing
    /// the lifecycle mapping.
    Denied(#[allow(dead_code)] Value),
    /// The cancellation token fired, or the inner work returned
    /// `bail!("Cancelled")`. The original `anyhow::Error` is preserved so
    /// the wrapper returns the same chain the inner executor produced.
    Cancelled(anyhow::Error),
    /// Anything else: lookup miss, disabled, schema failure, execution
    /// error, after-hook hard failure. The original `anyhow::Error` (with
    /// its context chain) is preserved untouched — the wrapper returns it
    /// as-is so `tool_result` and downstream renderers format it
    /// consistently with the pre-lifecycle code path.
    Error(anyhow::Error),
}

impl Outcome {
    fn status(&self) -> ActivityStatus {
        match self {
            Outcome::Completed(_) => ActivityStatus::Success,
            Outcome::Denied(_) => ActivityStatus::Denied,
            Outcome::Cancelled(_) => ActivityStatus::Cancelled,
            Outcome::Error(_) => ActivityStatus::Error,
        }
    }
    /// Translate the typed outcome into the original `Result<Value>` the
    /// inner executor would have returned. `Denied` becomes a fresh
    /// `Tool rejected by user` because rejection is a control-flow signal,
    /// not an error from the executor — the inner never returned it.
    fn into_result(self) -> Result<Value> {
        match self {
            Outcome::Completed(value) => Ok(value),
            Outcome::Denied(_) => bail!("Tool rejected by user"),
            Outcome::Cancelled(error) | Outcome::Error(error) => Err(error),
        }
    }
}

/// Sentinel error used internally to flag a user rejection without having to
/// match the surfaced `bail!("Tool rejected by user")` string in the outer
/// `invoke`. The outer wraps any `Err(_)` into `Outcome::Error` unless it
/// matches this guard via [`Deny::tag`], in which case it produces
/// `Outcome::Denied`.
///
/// The `String` slot is reserved for a future richer rejection reason
/// (e.g., the user's typed decline note) without changing the boundary.
#[derive(Debug)]
struct Deny(#[allow(dead_code)] String);

impl std::fmt::Display for Deny {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Tool rejected by user")
    }
}

impl std::error::Error for Deny {}

impl Deny {
    fn tag(self) -> anyhow::Error {
        anyhow::Error::new(self)
    }
    fn matched(err: &anyhow::Error) -> bool {
        err.downcast_ref::<Deny>().is_some()
    }
}

/// Build the redacted, display-safe title for a Subagent activity record.
/// Shape is `"agent <name>: <prompt summary>"`. The agent name comes
/// from the configured agent dictionary / model task arguments; both the
/// lookup and the result payload retain the exact original value. Only
/// the display title is sanitized through the shared helper so the
/// transcript never embeds control characters or unbounded text.
///
/// The agent name in the title is passed through the shared sanitizer
/// (which collapses whitespace and bounds the total to 160 Unicode
/// scalar values), so a model-controlled name cannot exceed the
/// 160-scalar budget or smuggle in newlines, tabs, or control bytes.
fn subagent_title(name: &str, prompt: &str) -> String {
    let prefix = format!("agent {name}: ");
    sanitize_activity_title(&prefix, prompt)
}

/// Map a Subagent's inner `Result<Value>` and cancellation state into
/// the matching `ActivityStatus`. The cancellation token is the
/// **authoritative** Cancelled signal: a fired token overrides whatever
/// the inner executor returned. Any other error — including one whose
/// rendered message happens to contain the substring `"Cancelled"` —
/// is reported as `Error` and the original `anyhow::Error` chain is
/// preserved untouched.
fn subagent_outcome_status(inner: &Result<Value>, cancel: &CancellationToken) -> ActivityStatus {
    match inner {
        Ok(_) => ActivityStatus::Success,
        Err(_) => {
            if cancel.is_cancelled() {
                ActivityStatus::Cancelled
            } else {
                ActivityStatus::Error
            }
        }
    }
}

#[derive(Clone)]
enum Source {
    Builtin,
    Custom,
    Mcp(McpTool),
}

fn approval_detail(name: &str, args: &Value, outside: bool) -> String {
    let mut detail = tools::describe_call(name, args);
    if name == "write_file" {
        if let Some(preview) = tools::write_preview(args) {
            detail.push_str("\nContent preview:\n");
            detail.push_str(&preview);
        }
    }
    if outside {
        format!("This call targets something outside the configured workspace.\n\n{detail}")
    } else {
        detail
    }
}
#[derive(Clone)]
/// A tool advertised to the model and the metadata the engine needs to
/// dispatch it. Returned by [`Engine::available`] (the current snapshot)
/// and passed back into [`Engine::invoke`] as the `registered` argument.
///
/// `source` and `hitl` are private because they are an implementation
/// detail of the dispatch layer; callers should rely on the spec and the
/// public `available` snapshot rather than reconstructing tool records by
/// hand.
pub struct RegisteredTool {
    pub spec: ToolSpec,
    source: Source,
    pub hitl: bool,
}

impl Engine {
    /// Compute the current snapshot of tools the model is allowed to call
    /// for `scope`. The result respects runtime enablement, agent
    /// allow/deny lists, edit permissions, MCP server enablement, and the
    /// standing `allow_outside_workspace` grant. The returned `Vec` is a
    /// point-in-time snapshot — runtime toggles applied after this call
    /// can still disable tools before they execute.
    ///
    /// `invoke` re-checks enablement through [`Engine::check_enabled`] on
    /// every call (including before/after hooks), so a tool that was
    /// disabled between `available()` and `invoke()` is rejected even if
    /// it appears in the `registered` snapshot.
    pub async fn available(
        &self,
        scope: &Scope,
        cancel: &CancellationToken,
    ) -> Result<Vec<RegisteredTool>> {
        let config = self.config.read().await.clone();
        self.available_with_config(scope, &config, cancel).await
    }

    /// Compute the availability snapshot against an already-loaded `config`,
    /// avoiding a second read-lock clone when the caller already holds one.
    /// Shares the exact advertising logic of [`Engine::available`].
    pub(crate) async fn available_with_config(
        &self,
        scope: &Scope,
        config: &Config,
        cancel: &CancellationToken,
    ) -> Result<Vec<RegisteredTool>> {
        let switches = self.switches.read().await;
        let allows = |name: &str| {
            switches.tool_enabled(name, config)
                && scope
                    .tools
                    .as_ref()
                    .is_none_or(|list| list.iter().any(|n| n == name))
                && (scope.can_edit || name != "write_file")
        };
        let mut result = vec![];
        for spec in tools::builtins() {
            if config.builtins.contains(&spec.name) && allows(&spec.name) {
                let hitl = config.approval_tools.contains(&spec.name)
                    || (config.require_for_destructive_tools
                        && ["write_file", "gh"].contains(&spec.name.as_str()));
                result.push(RegisteredTool {
                    spec,
                    source: Source::Builtin,
                    hitl,
                });
            }
        }
        for (name, tool) in &config.tools {
            if allows(name) && (scope.can_edit || !tool.destructive) {
                result.push(RegisteredTool {
                    spec: ToolSpec {
                        name: name.clone(),
                        description: tool.description.clone(),
                        input_schema: tool.input_schema.clone(),
                    },
                    source: Source::Custom,
                    hitl: tool.hitl
                        || config.approval_tools.contains(name)
                        || (tool.destructive && config.require_for_destructive_tools),
                });
            }
        }
        let servers: Vec<_> = config
            .mcp_servers
            .iter()
            .filter(|(name, server)| {
                switches.mcp_enabled(name, config)
                    && scope
                        .mcps
                        .as_ref()
                        .is_none_or(|ids| ids.contains(&server.uuid))
            })
            .map(|(name, _)| name.clone())
            .collect();
        drop(switches);
        for name in servers {
            match self.mcp.tools(&name, config, cancel).await {
                Ok(tools) => {
                    let switches = self.switches.read().await;
                    for tool in tools {
                        // An allowed MCP server's tools are exposed regardless
                        // of the agent's builtin/custom `tools` list, so MCP
                        // servers remain useful when an agent only constrains
                        // its shell and HTTP toolkit. `scope.mcps`,
                        // `scope.can_edit`, runtime/server enablement, and
                        // child scope narrowing (via `servers` filtering above)
                        // all still apply. `check_enabled` enforces the same
                        // rules at execution time.
                        //
                        // Runtime per-tool disablement (`/tools` toggles) is
                        // applied here so the model does not see tools that
                        // would be rejected at execution. The execution-time
                        // recheck in `check_enabled` still fires so a toggle
                        // applied between advertisement and execution also
                        // blocks the call.
                        let server = config.mcp_servers.get(&name);
                        if !switches.tool_enabled(&tool.spec.name, config) {
                            continue;
                        }
                        if !scope.can_edit && server.is_some_and(|s| s.hitl) {
                            continue;
                        }
                        let hitl = server.is_some_and(|s| s.hitl)
                            || config.approval_tools.contains(&tool.spec.name);
                        result.push(RegisteredTool {
                            spec: tool.spec.clone(),
                            source: Source::Mcp(tool),
                            hitl,
                        });
                    }
                }
                Err(e) => {
                    if cancel.is_cancelled() {
                        bail!("Cancelled");
                    }
                    let _ = self.events.send(UiEvent::Status {
                        context: scope.context.clone(),
                        text: format!("MCP {name} unavailable: {e}"),
                    });
                }
            }
        }
        Ok(result)
    }

    async fn check_enabled(
        &self,
        scope: &Scope,
        tool: &RegisteredTool,
        config: &Config,
    ) -> Result<()> {
        let switches = self.switches.read().await;
        // MCP tools are gated on `scope.mcps` (and runtime/server enablement),
        // not on `scope.tools`, because they ship from an allowed server
        // independent of the agent's builtin/custom list.
        let allow_tools_filter = !matches!(tool.source, Source::Mcp(_));
        if !switches.tool_enabled(&tool.spec.name, config)
            || (allow_tools_filter
                && !scope
                    .tools
                    .as_ref()
                    .is_none_or(|list| list.contains(&tool.spec.name)))
        {
            bail!("Tool is disabled: {}", tool.spec.name);
        }
        if !scope.can_edit && tool.spec.name == "write_file" {
            bail!("Editing is disabled for this agent: {}", tool.spec.name);
        }
        if !scope.can_edit
            && config
                .tools
                .get(&tool.spec.name)
                .is_some_and(|tool| tool.destructive)
        {
            bail!("Editing is disabled for this agent: {}", tool.spec.name);
        }
        if let Source::Mcp(mcp) = &tool.source {
            let server = config
                .mcp_servers
                .get(&mcp.server)
                .context("MCP server removed")?;
            if !switches.mcp_enabled(&mcp.server, config)
                || !scope
                    .mcps
                    .as_ref()
                    .is_none_or(|ids| ids.contains(&server.uuid))
            {
                bail!("MCP server is disabled");
            }
            if !scope.can_edit && config.mcp_servers.get(&mcp.server).is_some_and(|s| s.hitl) {
                bail!("Editing is disabled for this agent: {}", tool.spec.name);
            }
        }
        Ok(())
    }

    /// Execute a single tool call inside an activity lifecycle. Emits a
    /// paired `Start`/`End` `ActivityEvent` to disk and to the live UI
    /// channel around the inner dispatch so the on-disk trail is always
    /// complete, then returns the inner executor's `Result<Value>` so the
    /// conversation loop, tests, and downstream callers see the same
    /// control flow as the pre-lifecycle code path.
    ///
    /// `registered` must be the current `available()` snapshot for `scope`.
    /// Execution re-checks enablement through `check_enabled` before any
    /// side effect and around every hook, so a tool disabled between
    /// `available()` and `invoke()` is still rejected. A tool not present
    /// in the snapshot surfaces as `Err("Tool is unavailable for this
    /// agent")` with `ActivityStatus::Error`.
    ///
    /// The status mapping is:
    /// - `Ok(value)`  -> `ActivityStatus::Success`
    /// - `Err(Deny)`  -> `ActivityStatus::Denied` + `bail!("Tool rejected by user")`
    /// - token cancelled or `bail!("Cancelled")` -> `ActivityStatus::Cancelled`
    /// - everything else -> `ActivityStatus::Error`
    ///
    /// Original `anyhow::Error` chains are preserved across the wrapper;
    /// the outer does not format-and-recreate errors, so downstream
    /// formatters (e.g. `tool_result`) apply `format!("{error:#}")` to
    /// the chain the inner executor produced.
    pub async fn invoke(
        &self,
        scope: &Scope,
        call: &ToolCall,
        registered: &[RegisteredTool],
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let config = self.config.read().await.clone();
        self.invoke_with_config(scope, call, registered, &config, cancel)
            .await
    }

    /// Lifecycle/invocation body of [`Engine::invoke`], operating against an
    /// already-loaded `config` so callers that hold a snapshot avoid a second
    /// read-lock clone. Shares the exact behavior of [`Engine::invoke`].
    pub(crate) async fn invoke_with_config(
        &self,
        scope: &Scope,
        call: &ToolCall,
        registered: &[RegisteredTool],
        config: &Config,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        // 1. Generate the lifecycle id before any other work so the persisted
        //    Start/End pair always shares it, even on lookup/schema failures
        //    that bail before reaching the executor.
        let tool_id = uuid::Uuid::new_v4().to_string();
        // 2. Build a redacted, display-safe title for the activity
        //    record. The title prefixes the actual tool name (built-in,
        //    custom, or MCP) so the transcript always shows which tool
        //    produced the record even when `describe_call` collapses to
        //    a short summary. `tools::describe_call` is the same
        //    human-readable summary the approval dialog uses, so the
        //    activity title and the prompt detail stay in lockstep.
        //    We tolerate JSON parse failures on the title because the
        //    inner executor surfaces them as proper Error End events
        //    anyway; in that case the body is `"(invalid arguments)"` so
        //    the prefix is still meaningful.
        //
        //    The full approval and transcript descriptions are NOT touched by
        //    this helper; only the activity title is sanitized and
        //    capped. `describe_call` continues to flow through to the
        //    approval detail untouched.
        let title_args: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
        let raw_body = tools::describe_call(&call.name, &title_args);
        let body = if raw_body.is_empty() {
            "(invalid arguments)".to_owned()
        } else {
            raw_body
        };
        let prefix = format!("tool {}: ", call.name);
        let title = sanitize_activity_title(&prefix, &body);
        let parent_id = scope.activity_id.clone();
        let context = scope.context.clone();
        let external_id = Some(call.id.clone());
        let start = ActivityEvent {
            id: tool_id.clone(),
            parent_id: parent_id.clone(),
            context: context.clone(),
            kind: ActivityKind::Tool,
            phase: ActivityPhase::Start,
            title: title.clone(),
            external_id: external_id.clone(),
            status: None,
        };
        // 3. Persist + emit Start before any lookup/enable/schema validation
        //    so the on-disk trail covers even the rejection cases. A failure
        //    here aborts the call entirely — the on-disk write is the
        //    authoritative store; the live UiEvent is best-effort.
        self.emit_activity(start).await?;
        // 4. Threads the tool lifecycle id into the inner scope so any
        //    subsequent approval event can carry `activity_id == tool_id`.
        //    The inner call clones its scope; this allocation only happens
        //    once per invocation.
        let inner_scope = Scope {
            activity_id: Some(tool_id.clone()),
            ..scope.clone()
        };
        // 5. Run the original dispatch logic. Every result becomes a typed
        //    `Outcome` so End-status mapping never relies on string parsing.
        let outcome = match self
            .invoke_inner(&inner_scope, call, registered, config, cancel)
            .await
        {
            Ok(value) => Outcome::Completed(value),
            Err(error) => {
                if cancel.is_cancelled() {
                    // The cancellation token fired while the call was in
                    // flight. Surface it as `Cancelled` regardless of what
                    // the inner returned — the status and provider-visible
                    // message both reflect the user-initiated cancel,
                    // even if the inner happened to bail with `Deny` or
                    // some other error along the way. We rebuild a
                    // matching `Cancelled` error (or reuse the inner's
                    // `bail!("Cancelled")` if it already bailed) so the
                    // original chain is preserved when available.
                    if Deny::matched(&error) {
                        Outcome::Cancelled(anyhow::anyhow!("Cancelled"))
                    } else {
                        Outcome::Cancelled(error)
                    }
                } else if Deny::matched(&error) {
                    Outcome::Denied(Value::Null)
                } else {
                    Outcome::Error(error)
                }
            }
        };
        // 6. Build the matching End with the right status and emit it. If the
        //    End persistence fails we surface that error so the caller aborts
        //    rather than silently shipping a one-sided lifecycle to disk.
        //
        //    WARNING: this is a deliberately hard-failure policy. The on-disk
        //    activity log is the authoritative store, so a one-sided
        //    `Start` is a worse outcome than aborting the call. The
        //    downside is that a persistence error here may have left the
        //    tool's side effects (file writes, shell commands, MCP calls)
        //    committed upstream with no recorded End — the engine has no
        //    rollback mechanism, so the caller may need to retry the call
        //    idempotently. If a future policy decides that a single
        //    persisted `Start` should never abort a tool whose effects
        //    already ran, introduce a non-fatal `Activity` warning event
        //    here instead of returning `Err`. Until then, persistence is
        //    an explicit invariant and the `?` stays.
        //
        //    TODO(ActivityEvent::Warning): add a non-fatal activity
        //    warning variant so End-persistence failures can be surfaced
        //    without aborting the call once a non-rollback-aware policy
        //    is approved.
        let status = outcome.status();
        let end = ActivityEvent {
            id: tool_id.clone(),
            parent_id,
            context,
            kind: ActivityKind::Tool,
            phase: ActivityPhase::End,
            title,
            external_id,
            status: Some(status),
        };
        self.emit_activity(end).await?;
        // 7. Translate the typed outcome into the original `Result<Value>` so
        //    the conversation loop, tests, and downstream callers see the
        //    exact same control flow as before. The original `anyhow::Error`
        //    chain is preserved for `Cancelled` and `Error`, so callers like
        //    `tool_result` apply their own `format!("{error:#}")` and the
        //    surface never drifts from the pre-lifecycle code path.
        outcome.into_result()
    }

    async fn invoke_inner(
        &self,
        scope: &Scope,
        call: &ToolCall,
        registered: &[RegisteredTool],
        config: &Config,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let tool = registered
            .iter()
            .find(|t| t.spec.name == call.name)
            .context("Tool is unavailable for this agent")?;
        self.check_enabled(scope, tool, config).await?;
        let args: Value =
            serde_json::from_str(&call.arguments).context("Invalid tool arguments JSON")?;
        tools::validate_arguments(&tool.spec, &args)?;
        if cancel.is_cancelled() {
            bail!("Cancelled");
        }
        // Outside reads are approved by directory: one approval covers every
        // file in it for the session. The standing grant skips approval.
        let read_dir = if call.name == "read_file" && !scope.allow_outside_workspace {
            tools::read_directory(config, args["path"].as_str().context("Missing path")?)?
        } else {
            None
        };
        let outside_read = match &read_dir {
            Some(directory) => !self.outside_dirs.lock().await.contains(directory),
            None => false,
        };
        let shell_argv: Vec<String> = args["args"]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let shell_outside = call.name == "shell" && tools::outside_path_args(config, &shell_argv)?;
        let custom_outside = if scope.allow_outside_workspace {
            false
        } else {
            match (&tool.source, config.tools.get(&call.name)) {
                (Source::Custom, Some(definition)) => match &definition.kind {
                    crate::config::ToolKind::Command { cwd, .. } => tools::command_cwd_outside(
                        config,
                        cwd.as_deref().unwrap_or(&config.workspace),
                    ),
                    _ => false,
                },
                _ => false,
            }
        };
        let approval_required = tool.hitl
            || outside_read
            || custom_outside
            || (call.name == "shell"
                && tools::shell_requires_approval(
                    config,
                    args["command"].as_str().unwrap_or_default(),
                    &shell_argv,
                    scope.allow_outside_workspace,
                )?);
        // Read-only agents retain `shell` for heuristically-safe commands
        // (auto-run) and route everything classified as approval-required
        // through the ordinary explicit approval path. The standing
        // `allow_outside_workspace` grant, `write_file`/destructive custom
        // gates, and the unified bash policy denials remain unchanged.
        if approval_required {
            let detail = match &read_dir {
                Some(directory) => format!(
                    "Reads outside the workspace are approved by directory.\n\nRead `{}`\nDirectory: `{}`",
                    args["path"].as_str().unwrap_or("(missing path)"),
                    directory.display()
                ),
                None => approval_detail(&call.name, &args, shell_outside || custom_outside),
            };
            // Freeze the execution budget (and recursively its ancestors) while
            // waiting for approval serialization and the human response, then
            // resume before any tool execution or post-approval recheck. Drop
            // also runs on the error/deny early returns below.
            let pause = scope.budget.as_ref().map(|budget| budget.pause());
            let decision = self
                .approve_with_activity(
                    &scope.context,
                    format!("Allow {}?", call.name),
                    detail,
                    false,
                    scope.activity_id.as_deref(),
                    cancel,
                )
                .await;
            drop(pause);
            if decision? != Decision::Approve {
                return Err(Deny(String::new()).tag());
            }
            if let Some(directory) = &read_dir {
                self.outside_dirs.lock().await.insert(directory.clone());
            }
        }
        // An approved outside call is granted for this call only. A `read_file`
        // with a resolved outside directory has either just been approved above
        // or was already approved for this session, so it belongs to the
        // per-call grant. The static allow_outside_workspace agent setting
        // remains the standing grant.
        let outside_granted =
            scope.allow_outside_workspace || read_dir.is_some() || shell_outside || custom_outside;
        self.check_enabled(scope, tool, config).await?;
        hooks::emit_lazy(
            config,
            "before_tool",
            || json!({"context":scope.context,"tool":call.name,"arguments":args}),
            cancel,
        )
        .await?;
        self.check_enabled(scope, tool, config).await?;
        if cancel.is_cancelled() {
            bail!("Cancelled");
        }
        let _ = self.events.send(UiEvent::Status {
            context: scope.context.clone(),
            text: format!("Running {} ({})", call.name, scope.context),
        });
        // Delegation releases the parent's slot so nested fan-out cannot deadlock.
        let _slot = if ["delegate", "delegate_parallel"].contains(&call.name.as_str()) {
            None
        } else {
            self.child_slot(scope, cancel).await?
        };
        let result = match &tool.source {
            Source::Custom => {
                tools::custom(
                    config.tools.get(&call.name).context("Tool removed")?,
                    &args,
                    config,
                    outside_granted,
                    cancel,
                )
                .await?
            }
            Source::Mcp(mcp) => self.mcp.call(mcp, args.clone(), config, cancel).await?,
            Source::Builtin => match call.name.as_str() {
                "delegate" => self.delegate_with_lifecycle(scope, &args, cancel).await?,
                "delegate_parallel" => {
                    let tasks = args["tasks"].as_array().context("Missing tasks")?;
                    let mut pending = Vec::new();
                    for task in tasks {
                        pending.push(
                            async {
                                // Each sibling runs inside its own paired
                                // Subagent Start/End lifecycle so the
                                // on-disk trail is complete for every
                                // sibling even if another sibling fails
                                // or its conversation bails. The wrapper
                                // returns the inner `Result<Value>` so
                                // outer tool-result rendering keeps its
                                // shape: a sibling error becomes the
                                // pre-existing `{"agent":..,"error":..}`
                                // payload, not a propagated Err.
                                self.delegate_with_lifecycle(scope, task, cancel)
                                    .await
                                    .unwrap_or_else(
                                        |e| json!({"agent":task["agent"],"error":format!("{e:#}")}),
                                    )
                            }
                            .boxed(),
                        );
                    }
                    let results =
                        super::parallel_ordered(pending, config.max_parallel_subagents).await;
                    json!({"results":results})
                }
                "load_skill" => {
                    let name = args["name"].as_str().context("Missing name")?;
                    let skill = skills::discover(config)?
                        .into_iter()
                        .find(|s| s.name == name)
                        .context("Skill not found")?;
                    json!({"name":skill.name,"directory":skill.directory,"instructions":skill.instructions})
                }
                name => tools::builtin(name, &args, config, cancel, outside_granted).await?,
            },
        };
        match hooks::emit_lazy(
            config,
            "after_tool",
            || json!({"context":scope.context,"tool":call.name,"result":result}),
            cancel,
        )
        .await
        {
            Ok(()) => Ok(result),
            Err(error) => Ok(
                json!({"result":result,"hook_error":format!("{error:#}"),"note":"The tool already executed; its side effects were not rolled back."}),
            ),
        }
    }

    /// Mirror of [`Engine::invoke`] for the subagent lifecycle. Emits a
    /// paired `Subagent` `Start`/`End` `ActivityEvent` before and after
    /// the inner [`Engine::delegate`] runs, attaches `parent_id` to the
    /// invoking delegate tool activity id, and returns the inner
    /// `Result<Value>` so the conversation loop and tests keep their
    /// original control flow.
    ///
    /// Status mapping:
    /// - `Ok(_)`                   -> `Success`
    /// - cancellation fired /
    ///   error chain says "Cancelled" -> `Cancelled`
    /// - everything else           -> `Error`
    ///
    /// The original `anyhow::Error` chain is preserved across the
    /// wrapper, matching the tool lifecycle. Activity persistence
    /// failures remain hard-fail per the same invariant as
    /// [`Engine::invoke`].
    async fn delegate_with_lifecycle(
        &self,
        parent: &Scope,
        task: &Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        // 1. The Subagent id is allocated up front so the on-disk
        //    Start/End pair is guaranteed to share it, and so the
        //    child scope can install it as `scope.activity_id` before
        //    the conversation starts. The id is globally unique by
        //    construction (UUID v4).
        let subagent_id = uuid::Uuid::new_v4().to_string();
        // 2. The agent name is required to form the child context. If
        //    the caller didn't supply one we bail before emitting any
        //    activity record: there's no context to record against.
        let name = task["agent"].as_str().context("Missing agent")?.to_owned();
        // 3. Allocate the child context up front so the on-disk Start
        //    record already pins the context. The conversation call
        //    uses the same context further down.
        let context = format!("subagent:{name}:{}", uuid::Uuid::new_v4());
        // 4. Compute the bounded, sanitized title before the scope is
        //    formed so we never persist a Start with an empty title.
        //    The same title is reused on End so the on-disk trail
        //    stays self-describing. A missing or non-string prompt
        //    becomes an empty summary -- the agent name is the
        //    load-bearing part of the title, not the prompt.
        let raw_prompt = task["prompt"].as_str().unwrap_or("");
        let title = subagent_title(&name, raw_prompt);
        let parent_id = parent.activity_id.clone();
        // 5. Persist + emit Start before any inner work so the on-disk
        //    graph is built up from the leaf outward. The tool
        //    lifecycle call site already emitted a parent Start event
        //    with the matching `parent_id`, so concurrent siblings all
        //    share that parent activity id and never alias each
        //    other. Hard-fail on persistence, matching the tool
        //    lifecycle invariant.
        let start = ActivityEvent {
            id: subagent_id.clone(),
            parent_id: parent_id.clone(),
            context: context.clone(),
            kind: ActivityKind::Subagent,
            phase: ActivityPhase::Start,
            title: title.clone(),
            external_id: None,
            status: None,
        };
        self.emit_activity(start).await?;
        // 6. Build the child scope and overwrite `scope.activity_id`
        //    with the fresh Subagent id. `Engine::scope` inherits the
        //    parent's activity id by reference so a subagent executes
        //    under its originating tool lifecycle by default; we
        //    intentionally overwrite it so subsequent tool approvals
        //    inside the child correlate with the Subagent id, not the
        //    parent tool id. The overwrite happens before any
        //    conversation work so the chain is deterministic.
        let selection = Selection {
            agent: Some(name.clone()),
            agent_mode: task["mode"].as_str().map(str::to_owned),
            ..Selection::default()
        };
        let scope_result = self.scope(&selection, &context, Some(parent)).await;
        let mut scope = match scope_result {
            Ok(scope) => scope,
            Err(error) => {
                // Scope formation failed. Emit End with status=Error so
                // the Start is paired, then propagate the original
                // error chain. Same hard-fail policy as the tool
                // lifecycle: a one-sided Start is a worse outcome
                // than aborting.
                let end = ActivityEvent {
                    id: subagent_id.clone(),
                    parent_id: parent_id.clone(),
                    context: context.clone(),
                    kind: ActivityKind::Subagent,
                    phase: ActivityPhase::End,
                    title: title.clone(),
                    external_id: None,
                    status: Some(ActivityStatus::Error),
                };
                self.emit_activity(end).await?;
                return Err(error);
            }
        };
        scope.activity_id = Some(subagent_id.clone());
        // 7. Run the inner delegate. The inner is responsible for
        //    surfacing a missing/non-string prompt bail; the lifecycle
        //    wrapper maps the inner's `Result<Value>` to an End status.
        //    An explicitly present empty-string prompt is valid and is
        //    forwarded as-is for backward compatibility.
        let mut history = vec![];
        let inner = match task.get("prompt").and_then(Value::as_str) {
            Some(prompt) => {
                self.delegate_inner(&scope, &mut history, prompt, &name, cancel)
                    .await
            }
            None => Err(anyhow::anyhow!("Missing prompt")),
        };
        let status = subagent_outcome_status(&inner, cancel);
        // 8. Emit End with the same id, parent, and context as Start.
        //    Hard-fail on persistence, same invariant as
        //    [`Engine::invoke`].
        let end = ActivityEvent {
            id: subagent_id.clone(),
            parent_id,
            context,
            kind: ActivityKind::Subagent,
            phase: ActivityPhase::End,
            title,
            external_id: None,
            status: Some(status),
        };
        self.emit_activity(end).await?;
        inner
    }

    /// Body of the original `delegate` after the lifecycle scaffolding
    /// is split out. Kept separate so the lifecycle wrapper can re-use
    /// the exact same conversation call without recursion concerns.
    ///
    /// `name` is the **exact original** agent name as supplied by the
    /// caller (a configured `agents.<name>` key or a model-supplied
    /// `task["agent"]`). It is threaded in explicitly rather than re-
    /// derived from `scope.context` so:
    ///   * the agent name in the result JSON matches the configured /
    ///     task value byte-for-byte (no string-prefix parsing),
    ///   * the lookup, selection, and conversation call use the same
    ///     configured / task value the model named.
    ///
    /// Only the display title is sanitized; the result payload, the
    /// `Selection.agent`, the scope, and the conversation all use the
    /// original value untouched.
    async fn delegate_inner(
        &self,
        scope: &Scope,
        history: &mut Vec<Message>,
        prompt: &str,
        name: &str,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let result = self
            .conversation(scope, history, prompt.into(), cancel)
            .await?;
        Ok(json!({"agent": name, "result": result}))
    }

    pub async fn list_tools(
        &self,
        selection: &Selection,
        cancel: &CancellationToken,
    ) -> Result<Vec<ToolSpec>> {
        let scope = self.scope(selection, "main", None).await?;
        Ok(self
            .available(&scope, cancel)
            .await?
            .into_iter()
            .map(|t| t.spec)
            .collect())
    }
}
