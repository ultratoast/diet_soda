//! Tool advertising and dispatch. Runtime checks bracket approvals and hooks so
//! disabling a previously advertised tool takes effect before it executes.
use super::{Engine, Scope, Selection};
use crate::{
    hooks,
    mcp::McpTool,
    model::{Decision, ToolCall, ToolSpec, UiEvent},
    skills, tools,
};
use anyhow::{bail, Context, Result};
use futures_util::FutureExt;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
enum Source {
    Builtin,
    Custom,
    Mcp(McpTool),
}
#[derive(Clone)]
pub(super) struct RegisteredTool {
    pub spec: ToolSpec,
    source: Source,
    hitl: bool,
}

impl Engine {
    pub(super) async fn available(
        &self,
        scope: &Scope,
        cancel: &CancellationToken,
    ) -> Result<Vec<RegisteredTool>> {
        let config = self.config.read().await.clone();
        let switches = self.switches.read().await;
        let allows = |name: &str| {
            switches.tool_enabled(name, &config)
                && scope
                    .tools
                    .as_ref()
                    .is_none_or(|list| list.iter().any(|n| n == name))
                && (scope.can_edit || !["shell", "write_file"].contains(&name))
        };
        let mut result = vec![];
        for spec in tools::builtins() {
            if config.builtins.contains(&spec.name) && allows(&spec.name) {
                let hitl = config.approval_tools.contains(&spec.name)
                    || (config.require_for_destructive_tools
                        && ["shell", "write_file", "gh"].contains(&spec.name.as_str()));
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
                switches.mcp_enabled(name, &config)
                    && scope
                        .mcps
                        .as_ref()
                        .is_none_or(|ids| ids.contains(&server.uuid))
            })
            .map(|(name, _)| name.clone())
            .collect();
        drop(switches);
        for name in servers {
            match self.mcp.tools(&name, &config, cancel).await {
                Ok(tools) => {
                    let switches = self.switches.read().await;
                    for tool in tools {
                        if switches.tool_enabled(&tool.spec.name, &config)
                            && scope
                                .tools
                                .as_ref()
                                .is_none_or(|list| list.contains(&tool.spec.name))
                        {
                            let hitl = config.mcp_servers[&name].hitl
                                || config.approval_tools.contains(&tool.spec.name);
                            result.push(RegisteredTool {
                                spec: tool.spec.clone(),
                                source: Source::Mcp(tool),
                                hitl,
                            });
                        }
                    }
                }
                Err(e) => {
                    if cancel.is_cancelled() {
                        bail!("Cancelled");
                    }
                    let _ = self
                        .events
                        .send(UiEvent::Status(format!("MCP {name} unavailable: {e}")));
                }
            }
        }
        Ok(result)
    }

    async fn check_enabled(&self, scope: &Scope, tool: &RegisteredTool) -> Result<()> {
        let config = self.config.read().await;
        let switches = self.switches.read().await;
        if !switches.tool_enabled(&tool.spec.name, &config)
            || !scope
                .tools
                .as_ref()
                .is_none_or(|list| list.contains(&tool.spec.name))
        {
            bail!("Tool is disabled: {}", tool.spec.name);
        }
        if !scope.can_edit && ["shell", "write_file"].contains(&tool.spec.name.as_str()) {
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
            if !switches.mcp_enabled(&mcp.server, &config)
                || !scope
                    .mcps
                    .as_ref()
                    .is_none_or(|ids| ids.contains(&server.uuid))
            {
                bail!("MCP server is disabled");
            }
        }
        Ok(())
    }

    pub(super) async fn invoke(
        &self,
        scope: &Scope,
        call: &ToolCall,
        registered: &[RegisteredTool],
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let tool = registered
            .iter()
            .find(|t| t.spec.name == call.name)
            .context("Tool is unavailable for this agent")?;
        self.check_enabled(scope, tool).await?;
        let args: Value =
            serde_json::from_str(&call.arguments).context("Invalid tool arguments JSON")?;
        tools::validate_arguments(&tool.spec, &args)?;
        if cancel.is_cancelled() {
            bail!("Cancelled");
        }
        if tool.hitl
            && self
                .approve(
                    &scope.context,
                    format!("Allow tool {}?", call.name),
                    serde_json::to_string_pretty(&args)?,
                    false,
                    cancel,
                )
                .await?
                != Decision::Approve
        {
            bail!("Tool rejected by user");
        }
        self.check_enabled(scope, tool).await?;
        let config = self.config.read().await.clone();
        hooks::emit(
            &config,
            "before_tool",
            json!({"context":scope.context,"tool":call.name,"arguments":args}),
            cancel,
        )
        .await?;
        self.check_enabled(scope, tool).await?;
        if cancel.is_cancelled() {
            bail!("Cancelled");
        }
        let _ = self.events.send(UiEvent::Status(format!(
            "Running {} ({})",
            call.name, scope.context
        )));
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
                    &config,
                    cancel,
                )
                .await?
            }
            Source::Mcp(mcp) => self.mcp.call(mcp, args.clone(), &config, cancel).await?,
            Source::Builtin => match call.name.as_str() {
                "delegate" => self.delegate(scope, &args, cancel).await?,
                "delegate_parallel" => {
                    let tasks = args["tasks"].as_array().context("Missing tasks")?;
                    let mut pending = Vec::new();
                    for task in tasks {
                        pending.push(
                            async {
                                self.delegate(scope, task, cancel).await.unwrap_or_else(
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
                    let skill = skills::discover(&config)?
                        .into_iter()
                        .find(|s| s.name == name)
                        .context("Skill not found")?;
                    json!({"name":skill.name,"directory":skill.directory,"instructions":skill.instructions})
                }
                name => tools::builtin(name, &args, &config, cancel).await?,
            },
        };
        match hooks::emit(
            &config,
            "after_tool",
            json!({"context":scope.context,"tool":call.name,"result":result}),
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

    async fn delegate(
        &self,
        parent: &Scope,
        task: &Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let name = task["agent"].as_str().context("Missing agent")?;
        let selection = Selection {
            agent: Some(name.into()),
            agent_mode: task["mode"].as_str().map(str::to_owned),
            ..Selection::default()
        };
        let context = format!("subagent:{name}:{}", uuid::Uuid::new_v4());
        let scope = self.scope(&selection, &context, Some(parent)).await?;
        let mut history = vec![];
        let result = self
            .conversation(
                &scope,
                &mut history,
                task["prompt"].as_str().context("Missing prompt")?.into(),
                cancel,
            )
            .await?;
        Ok(json!({"agent":name,"result":result}))
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
