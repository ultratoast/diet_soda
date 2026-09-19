//! Strict workflow files and post-step approval gates. Steps have isolated
//! histories and pass only accepted output to the next invocation.
use crate::{
    config::Config,
    engine::{intersect, Engine, Selection},
    hooks,
    model::{Decision, Message},
    template,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workflow {
    pub title: String,
    pub author: String,
    pub steps: Vec<Step>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    #[serde(default)]
    pub agent: Option<String>,
    pub model: String,
    pub prompt: String,
    pub mcps: Vec<McpReference>,
    pub hitl: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpReference {
    pub name: String,
    pub uuid: String,
    pub enabled: bool,
}

impl Workflow {
    pub fn load(path: &Path, config: &Config) -> Result<Self> {
        let workflow: Self = serde_json::from_str(&std::fs::read_to_string(path)?)?;
        workflow.validate(config)?;
        Ok(workflow)
    }
    pub fn validate(&self, config: &Config) -> Result<()> {
        if self.title.is_empty() || self.author.is_empty() || self.steps.is_empty() {
            bail!("Workflow title, author and nonempty steps are required");
        }
        for (index, step) in self.steps.iter().enumerate() {
            if let Some(agent) = &step.agent {
                if !config.agents.contains_key(agent) {
                    bail!("Step {} unknown agent: {agent}", index + 1);
                }
            }
            config
                .resolve_model(&step.model)
                .with_context(|| format!("Step {} model", index + 1))?;
            if step.prompt.trim().is_empty() {
                bail!("Step {} prompt is empty", index + 1);
            }
            template::render(
                &step.prompt,
                &json!({"input":"","previous_result":"","workflow_title":self.title,"step_index":index+1}),
            )?;
            let mut ids = std::collections::HashSet::new();
            for reference in &step.mcps {
                let server = config
                    .mcp_servers
                    .get(&reference.name)
                    .with_context(|| format!("Unknown MCP server: {}", reference.name))?;
                if server.uuid != reference.uuid || !ids.insert(&reference.uuid) {
                    bail!("MCP reference mismatch or duplicate: {}", reference.name);
                }
            }
        }
        Ok(())
    }
}

pub async fn run(
    engine: &Engine,
    workflow: Workflow,
    input: String,
    selection: Selection,
    cancel: CancellationToken,
) -> Result<String> {
    let config = engine.config.read().await.clone();
    workflow.validate(&config)?;
    let run_id = uuid::Uuid::new_v4().to_string();
    engine.session.lock().await.append(
        "workflow_start",
        &run_id,
        json!({"workflow":workflow,"input":input}),
    )?;
    let mut previous = String::new();
    for (index, step) in workflow.steps.iter().enumerate() {
        let mut attempt = 0;
        'attempt: loop {
            if cancel.is_cancelled() {
                bail!("Workflow cancelled");
            }
            attempt += 1;
            let context = format!("workflow:{run_id}:{}:{attempt}", index + 1);
            let step_selection = Selection {
                agent: step.agent.clone().or(selection.agent.clone()),
                model: Some(step.model.clone()),
                agent_mode: None,
                ..selection.clone()
            };
            let mut scope = engine.scope(&step_selection, &context, None).await?;
            scope.mcps = intersect(
                scope.mcps,
                Some(
                    step.mcps
                        .iter()
                        .filter(|m| m.enabled)
                        .map(|m| m.uuid.clone())
                        .collect(),
                ),
            );
            let resolved = template::render(
                &step.prompt,
                &json!({"input":input,"previous_result":previous,"workflow_title":workflow.title,"step_index":index+1}),
            )?;
            let prompt = format!("Workflow: {}\nStep: {} of {}\n\nWorkflow input:\n{}\n\nPrevious step result:\n{}\n\nInstructions:\n{}",workflow.title,index+1,workflow.steps.len(),input,previous,resolved);
            let mut history = vec![];
            let result = engine
                .conversation(&scope, &mut history, prompt, &cancel)
                .await;
            match result {
                Ok(output) => {
                    engine.session.lock().await.append(
                        "workflow_step",
                        &context,
                        json!({"index":index+1,"attempt":attempt,"output":output}),
                    )?;
                    hooks::emit(
                        &config,
                        "workflow_step",
                        json!({"workflow":workflow.title,"index":index+1,"output":output}),
                        &cancel,
                    )
                    .await?;
                    if step.hitl && index + 1 < workflow.steps.len() {
                        match engine
                            .approve(
                                &context,
                                format!("Step {} complete — advance?", index + 1),
                                output.clone(),
                                true,
                                &cancel,
                            )
                            .await?
                        {
                            Decision::Approve => {}
                            Decision::Retry => continue 'attempt,
                            Decision::Skip => break 'attempt,
                            _ => bail!("Workflow aborted after step {}", index + 1),
                        }
                    }
                    previous = output;
                    break 'attempt;
                }
                Err(error) => {
                    engine.session.lock().await.append(
                        "workflow_error",
                        &context,
                        json!({"error":format!("{error:#}")}),
                    )?;
                    if cancel.is_cancelled() {
                        return Err(error);
                    }
                    match engine
                        .approve(
                            &context,
                            format!("Step {} failed — retry, skip, or abort", index + 1),
                            format!("{error:#}"),
                            true,
                            &cancel,
                        )
                        .await?
                    {
                        Decision::Retry => continue 'attempt,
                        Decision::Skip => break 'attempt,
                        _ => return Err(error),
                    }
                }
            }
        }
    }
    engine
        .session
        .lock()
        .await
        .append("workflow_complete", &run_id, json!({"result":previous}))?;
    engine
        .record(
            "main",
            Message::new(
                "user",
                format!("Run workflow {} with input: {input}", workflow.title),
            ),
        )
        .await?;
    engine
        .record("main", Message::new("assistant", previous.clone()))
        .await?;
    Ok(previous)
}

pub fn workflow_path(name: &str, config: &Config) -> PathBuf {
    let path = PathBuf::from(name);
    if path.exists() {
        path
    } else {
        config.workflows_dir.join(if name.ends_with(".json") {
            name.into()
        } else {
            format!("{name}.json")
        })
    }
}
pub fn list_workflows(config: &Config) -> Result<Vec<String>> {
    if !config.workflows_dir.exists() {
        return Ok(vec![]);
    }
    let mut paths = vec![];
    for entry in std::fs::read_dir(&config.workflows_dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|s| s == "json") {
            paths.push(path.display().to_string());
        }
    }
    paths.sort();
    Ok(paths)
}
