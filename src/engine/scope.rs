//! Prompt composition and permission narrowing for agents, modes, and children.
use super::Engine;
use crate::{
    config::{AgentConfig, Effort, ModelConfig},
    skills,
};
use anyhow::{bail, Context, Result};

#[derive(Clone, Default)]
pub struct Selection {
    pub agent: Option<String>,
    pub agent_mode: Option<String>,
    pub model: Option<String>,
    pub effort: Option<Effort>,
}

#[derive(Clone)]
pub struct Scope {
    pub context: String,
    pub model: ModelConfig,
    pub system: String,
    pub tools: Option<Vec<String>>,
    pub mcps: Option<Vec<String>>,
    pub max_turns: usize,
    pub depth: usize,
    pub timeout_seconds: u64,
    pub can_edit: bool,
}

impl Engine {
    pub async fn scope(
        &self,
        selection: &Selection,
        context: &str,
        parent: Option<&Scope>,
    ) -> Result<Scope> {
        let config = self.config.read().await.clone();
        let mut agent = match &selection.agent {
            Some(name) => config
                .agents
                .get(name)
                .with_context(|| format!("Unknown agent: {name}"))?
                .clone(),
            None => AgentConfig::default(),
        };
        let mut system = config.system_prompt.clone();
        append_prompt(
            &mut system,
            &format!(
                "Runtime context:\n- Working directory: {}\n- Configuration directory: {}\n- Read and manipulate project files with workspace-scoped file tools immediately; do not guess paths or substitute another filesystem root.",
                config.workspace.display(),
                config.config_dir.display()
            ),
        );
        for prompt in [&agent.system_prompt, &agent.prompt].into_iter().flatten() {
            append_prompt(&mut system, prompt);
        }
        if let Some(mode) = &selection.agent_mode {
            let mode = agent
                .modes
                .get(mode)
                .with_context(|| format!("Unknown agent mode: {mode}"))?
                .clone();
            if let Some(prompt) = mode.prompt {
                append_prompt(&mut system, &prompt);
            }
            if let Some(model) = mode.model {
                agent.model = Some(model);
            }
            agent.tools = intersect(agent.tools, mode.tools);
            agent.mcp_servers = intersect(agent.mcp_servers, mode.mcp_servers);
        }
        let mut model = selection
            .model
            .as_ref()
            .or(agent.model.as_ref())
            .map(|m| config.resolve_model(m))
            .transpose()?
            .unwrap_or(config.model.clone());
        if let Some(effort) = selection.effort {
            model.set_effort(effort)?;
        }
        let enabled = agent.skills.as_ref().unwrap_or(&config.skills.enabled);
        system.push_str(&skills::instructions(&config, enabled)?);
        if !config.agents.is_empty() {
            append_prompt(&mut system,&format!("Available subagents: {}. Delegate at your discretion. Use delegate_parallel for independent tasks, or issue multiple delegate calls in one response. Each child gets only its task and its configured agent prompt.",config.agents.keys().cloned().collect::<Vec<_>>().join(", ")));
        }
        let mut scope = Scope {
            context: context.into(),
            model,
            system,
            tools: agent.tools,
            mcps: agent.mcp_servers,
            max_turns: agent
                .max_turns
                .unwrap_or(config.max_turns)
                .min(config.max_turns),
            depth: 0,
            timeout_seconds: agent.timeout_seconds.unwrap_or(1800),
            // The top-level interactive session retains the historical ability
            // to edit. Configured agents/subagents must opt in explicitly.
            can_edit: selection.agent.is_none() || agent.can_edit,
        };
        if let Some(parent) = parent {
            scope.depth = parent.depth + 1;
            if scope.depth > config.max_subagent_depth {
                bail!("Subagent depth limit reached");
            }
            let default_tools = vec!["web_fetch".into(), "read_file".into(), "load_skill".into()];
            scope.tools = intersect(
                Some(scope.tools.unwrap_or(default_tools)),
                parent.tools.clone(),
            );
            scope.mcps = intersect(Some(scope.mcps.unwrap_or_default()), parent.mcps.clone());
            scope.can_edit &= parent.can_edit;
        }
        Ok(scope)
    }
}

fn append_prompt(system: &mut String, prompt: &str) {
    system.push_str("\n\n");
    system.push_str(prompt);
}

/// An omitted restriction inherits its counterpart; two lists always intersect.
pub fn intersect(a: Option<Vec<String>>, b: Option<Vec<String>>) -> Option<Vec<String>> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.into_iter().filter(|x| b.contains(x)).collect()),
        (a, None) => a,
        (None, b) => b,
    }
}
