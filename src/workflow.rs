//! Strict workflow files and post-step approval gates. Steps have isolated
//! histories and pass only accepted output to the next invocation.
use crate::{
    config::Config,
    engine::{intersect, sanitize_activity_title, Engine, Selection},
    hooks,
    model::{ActivityEvent, ActivityKind, ActivityPhase, ActivityStatus, Decision, Message},
    template,
    text::is_unsafe_terminal_char,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

/// Build the redacted, display-safe title for a `WorkflowStep` activity
/// record. Shape is `"workflow <title> step <index>: <step prompt
/// summary>"`. The workflow title is sanitized through the shared
/// helper so control characters / newlines / tabs cannot reach the
/// transcript, then the helper caps the entire result to 160 Unicode
/// scalar values. The bound is in Unicode scalar values
/// (`chars().count()`), not bytes, so multi-byte UTF-8 sequences
/// cannot push the on-disk title past the limit.
///
/// The 1-based step index and the prompt summary are added after
/// sanitizing the workflow title so the helper sees the exact prefix
/// it needs to budget against.
fn step_title(workflow_title: &str, step_index: usize, prompt: &str) -> String {
    // Sanitize the workflow title into display-safe form before it
    // becomes part of the prefix. The shared helper assumes the
    // prefix is already clean, so we collapse control characters
    // and runs of whitespace here (the body path does the same
    // thing internally for the prompt summary).
    let sanitized_title: String = workflow_title
        .chars()
        .map(|c| if is_unsafe_terminal_char(c) { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let prefix = format!("workflow {sanitized_title} step {step_index}: ");
    sanitize_activity_title(&prefix, prompt)
}

/// Map a step attempt's inner `Result<String>` and cancellation state
/// into the matching `ActivityStatus`. The cancellation token is the
/// **authoritative** Cancelled signal: a fired token overrides whatever
/// the conversation returned. Any other error — including one whose
/// rendered message happens to contain the substring `"Cancelled"` —
/// is reported as `Error` and the original `anyhow::Error` chain is
/// preserved untouched.
fn step_outcome_status(inner: &Result<String>, cancel: &CancellationToken) -> ActivityStatus {
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
    // Preflight every step's selected agent/model (including the
    // workflow-level effort override) before any workflow step, provider,
    // or tool side effect. Reuses `Engine::scope` so unknown agents,
    // unresolvable models, agent modes, and unsupported effort levels are
    // all returned before step 1 runs. This only composes/validates the
    // scope; it renders no prompts, connects no tools, and emits no
    // lifecycle or session events, leaving actual step execution and
    // HITL ordering untouched.
    for (index, step) in workflow.steps.iter().enumerate() {
        engine
            .scope(
                &Selection {
                    agent: step.agent.clone().or(selection.agent.clone()),
                    model: Some(step.model.clone()),
                    agent_mode: None,
                    ..selection.clone()
                },
                &format!("workflow:preflight:{index}"),
                None,
            )
            .await
            .with_context(|| format!("Step {} preflight", index + 1))?;
    }
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
            // Every step attempt gets one fresh UUID for the paired
            // `WorkflowStep` activity lifecycle and a fresh context label.
            // The id and the context are allocated up front so the
            // on-disk Start record already pins both, and a retry reuses
            // neither: the new attempt produces its own Start/End pair
            // correlated only by the outer workflow run id (`external_id`).
            let step_id = uuid::Uuid::new_v4().to_string();
            let context = format!("workflow:{run_id}:{}:{attempt}", index + 1);
            let title = step_title(&workflow.title, index + 1, &step.prompt);
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
            // Pin `scope.activity_id` to the step lifecycle id BEFORE
            // `engine.conversation` runs. Nested tool calls and the
            // post-step HITL approval both read `scope.activity_id`, so
            // the existing `approve_with_activity` call sites already
            // thread it through and now correlate to this step.
            scope.activity_id = Some(step_id.clone());
            // Render the step prompt BEFORE emitting Start so a render
            // failure (e.g. an undeclared template variable) cannot
            // strand an unmatched Start on disk or in the live channel.
            // The render is otherwise a precondition for the
            // conversation, so doing it first keeps the lifecycle
            // invariant ("Start is always paired with an End") even
            // when the workflow definition has a bug.
            let resolved = template::render(
                &step.prompt,
                &json!({"input":input,"previous_result":previous,"workflow_title":workflow.title,"step_index":index+1}),
            )?;
            let prompt = format!("Workflow: {}\nStep: {} of {}\n\nWorkflow input:\n{}\n\nPrevious step result:\n{}\n\nInstructions:\n{}",workflow.title,index+1,workflow.steps.len(),input,previous,resolved);
            let start = ActivityEvent {
                id: step_id.clone(),
                parent_id: None,
                context: context.clone(),
                kind: ActivityKind::WorkflowStep,
                phase: ActivityPhase::Start,
                title: title.clone(),
                external_id: Some(run_id.clone()),
                status: None,
            };
            // Persist + emit Start before any inner work. Hard-fail on
            // persistence matches the tool and subagent invariants: a
            // one-sided Start is a worse outcome than aborting the
            // attempt. The template render above guarantees the prompt
            // string is valid; if the render fails the Start is never
            // emitted, so a one-sided Start cannot be stranded on
            // disk from a malformed step prompt.
            engine.emit_activity(start).await?;
            let mut history = vec![];
            let result = engine
                .conversation(&scope, &mut history, prompt, &cancel)
                .await;
            // Emit End FIRST, then run the existing post-step gate/advance
            // /retry/skip logic. The status derives from the conversation
            // result and the cancel token; the original `Result` and
            // `anyhow::Error` chain are preserved across the wrapper so
            // existing workflow / session / hook behavior stays
            // unchanged. Activity persistence failure remains hard-fail
            // per the invariant established by tool and subagent
            // lifecycles.
            let status = step_outcome_status(&result, &cancel);
            let end = ActivityEvent {
                id: step_id.clone(),
                parent_id: None,
                context: context.clone(),
                kind: ActivityKind::WorkflowStep,
                phase: ActivityPhase::End,
                title: title.clone(),
                external_id: Some(run_id.clone()),
                status: Some(status),
            };
            engine.emit_activity(end).await?;
            match result {
                Ok(output) => {
                    engine.session.lock().await.append(
                        "workflow_step",
                        &context,
                        json!({"index":index+1,"attempt":attempt,"output":output}),
                    )?;
                    hooks::emit_lazy(
                        &config,
                        "workflow_step",
                        || json!({"workflow":workflow.title,"index":index+1,"output":output}),
                        &cancel,
                    )
                    .await?;
                    if step.hitl && index + 1 < workflow.steps.len() {
                        match engine
                            .approve_with_activity(
                                &context,
                                format!("Step {} complete — advance?", index + 1),
                                output.clone(),
                                true,
                                Some(&step_id),
                                &cancel,
                            )
                            .await?
                        {
                            Decision::Approve | Decision::ApprovePersist => {}
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
                        .approve_with_activity(
                            &context,
                            format!("Step {} failed — retry, skip, or abort", index + 1),
                            format!("{error:#}"),
                            true,
                            Some(&step_id),
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
