//! Slash commands are deliberately plain text plus JSON for structured additions.
//! Each handler either changes local UI state or calls one well-defined service.
use super::{app::App, picker::Picker};
use crate::{
    config::{store, themes, Config, Effort},
    engine::{Engine, Selection},
    session::Session,
    skills, workflow,
};
use anyhow::{bail, Context, Result};
use std::path::Path;

pub(super) const HELP: &str = r#"Commands
/agent [name|default]       Show or change the active agent
/mode [name|default]        Alias for changing agents; legacy modes still work
/model                    Browse/search models in a dialog
/model <alias|provider:id> Switch the active model directly
/model add <alias> <JSON>   Add a model to config and select it
/model add <alias> <ref>    Add an alias for provider:model-id
/effort [level|default]    Show or change supported reasoning effort
/mcp                      Open the searchable MCP on/off dialog
/mcp <name on|off|restart> Activate, deactivate, or restart MCP
/mcp add <name> <JSON>     Add a server; generate UUID if omitted
/theme [name|configured]  Browse/preview themes or select one directly
/tools [name on|off]       List or toggle tools at runtime
/mouse [on|off|toggle]    Session mouse capture; off restores native terminal selection
/workflow [file] [input]   List workflows or run one
/skills [name on|off]      List or activate installed skills
/install-skill <source>    Install a local/HTTPS skill
/export [directory]       Export the complete session as timestamped text
/clear                    Start a fresh session and reset spend/history
/new                      Alias for /clear; old session files remain
/cost                     Show session spend
/reload                   Reload config and reset runtime overrides
/help                     Show this reference
/quit or :q               Quit, cancelling active work

Examples
/model add fast openrouter:openai/gpt-4.1-mini
/mcp add demo {"transport":"stdio","command":"python3","args":["server.py"],"enabled":true}
/effort low

Keys
Enter: send | Alt+Enter or Ctrl+J: newline
Left/Right/Home/End: edit | Up/Down: input history
PageUp/PageDown/Home/End: scroll history, dialogs, or the workflow-complete overlay
Ctrl+Home/End: history top/bottom | Tab/Shift+Tab: next/previous agent
F6: focus transcript/activity | Esc: return to input
Activity focus: Up/Down select, Left/Right collapse/expand, Enter/Space toggle
Left-click: toggle a visible activity row | Wheel: scroll the active transcript, overlay, or picker
Ctrl+C: cancel | Ctrl+D: quit with empty input
Approvals: y approve, n reject, q abort (workflow approvals: r retry, s skip)
Pickers: type to fuzzy-filter, Up/Down browse, Enter select/toggle, Esc close
Workflow-complete: n new run | r repeat | q exit workflow
Esc: close dialogs or reject approval; bare Esc cancels the active run when no dialog or activity focus owns it

Fonts
The TUI uses your terminal emulator's selected system/monospace font.
No special font is required. Set theme.ascii=true for ASCII borders.
"#;

impl App {
    pub(super) async fn command(
        &mut self,
        text: &str,
        engine: &Engine,
        config_path: &Path,
    ) -> Result<()> {
        let (command, rest) = split_head(text);
        match command {
            "/quit" | ":q" => self.quit = true,
            "/help" => {
                self.help = true;
                self.overlay_scroll = 0;
            }
            "/mouse" => {
                let enabled = match rest {
                    "" => self.mouse_enabled,
                    "on" => true,
                    "off" => false,
                    "toggle" => !self.mouse_enabled,
                    _ => bail!("Usage: /mouse [on|off|toggle]"),
                };
                self.mouse_enabled = enabled;
                self.status = format!(
                    "Mouse capture {}",
                    if enabled { "enabled" } else { "disabled" }
                );
            }
            "/cost" => self.note(format!(
                "Spend: {} ({} unpriced requests)",
                self.spend.display(),
                self.spend.unpriced_requests
            )),
            "/tools" => self.tools_command(rest, engine).await?,
            "/mcp" => self.mcp_command(rest, engine, config_path).await?,
            "/theme" => {
                let configured = engine.config.read().await.theme.clone();
                if rest.is_empty() {
                    self.picker = Some(Picker::themes(&self.theme, &configured));
                } else {
                    let theme = if rest == "configured" {
                        configured
                    } else {
                        let mut theme =
                            themes::preset(rest).context("Unknown theme; use /theme to browse")?;
                        theme.ascii = self.theme.ascii;
                        theme.syntax_highlighting = self.theme.syntax_highlighting;
                        theme
                    };
                    self.theme = theme;
                    self.status = format!("Theme: {rest}");
                }
            }
            _ => {
                self.require_idle()?;
                match command {
                    "/model" => self.model_command(rest, engine, config_path).await?,
                    "/effort" => self.effort_command(rest, engine).await?,
                    "/agent" => self.agent_command(rest, engine).await?,
                    "/mode" => self.mode_command(rest, engine).await?,
                    "/workflow" => {
                        let config = engine.config.read().await.clone();
                        if rest.is_empty() {
                            self.note(workflow::list_workflows(&config)?.join("\n"));
                        } else {
                            let (name, input) = split_head(rest);
                            self.start_workflow(engine, name, input.into()).await?;
                        }
                    }
                    "/skills" => self.skills_command(rest, engine).await?,
                    "/install-skill" => {
                        let directory = engine.config.read().await.skills_dir.clone();
                        self.note(format!(
                            "Installed {}",
                            skills::install(rest, &directory).await?.display()
                        ));
                    }
                    "/export" => {
                        let config = engine.config.read().await.clone();
                        let directory = if rest.is_empty() {
                            config.exports_dir
                        } else {
                            crate::config::resolve_path(&config.workspace, Path::new(rest))
                        };
                        let path = engine.session.lock().await.export(&directory)?;
                        self.note(format!("Exported {}", path.display()));
                    }
                    "/clear" | "/new" => self.reset_session(engine).await?,
                    "/reload" => {
                        let updated = Config::load(config_path)?;
                        engine.mcp.shutdown().await;
                        self.theme = updated.theme.clone();
                        self.selection = Selection::default();
                        self.mode = None;
                        self.workflow_mode = None;
                        self.workflow_complete = false;
                        self.last_workflow_input = None;
                        // The parked-front error references a workflow and
                        // model that may no longer exist on disk after the
                        // reload. Clear the parking so the user is not
                        // stuck retrying against the old configuration;
                        // queued_inputs themselves are preserved per the
                        // session-reset contract. Reset the status line too,
                        // otherwise the stale "press Enter to retry" text
                        // outlives the parking it described while the
                        // transcript/activity view stays intact.
                        if self.queue_blocked.take().is_some() {
                            self.status = "Ready".into();
                        }
                        engine.replace_config(updated, true).await;
                        self.note("Configuration reloaded; runtime overrides reset");
                    }
                    _ => bail!("Unknown command: {command}; /help lists commands"),
                }
                self.refresh_model(engine).await?;
            }
        }
        Ok(())
    }

    async fn model_command(&mut self, rest: &str, engine: &Engine, path: &Path) -> Result<()> {
        let config = engine.config.read().await.clone();
        if rest.is_empty() {
            let scope = engine.scope(&self.selection, "main", None).await?;
            // Keep the effective alias even when it comes from an agent/mode:
            // accepting the current row must not replace its model settings with
            // the defaults used for an unconfigured raw model ID.
            let agent = self
                .selection
                .agent
                .as_ref()
                .and_then(|name| config.agents.get(name));
            let reference = self.selection.model.as_deref().or_else(|| {
                agent.and_then(|agent| {
                    self.selection
                        .agent_mode
                        .as_ref()
                        .and_then(|name| agent.modes.get(name))
                        .and_then(|mode| mode.model.as_deref())
                        .or(agent.model.as_deref())
                })
            });
            let mut picker = Picker::models(&config, &scope.model, reference);
            picker.load(&config, engine);
            self.picker = Some(picker);
        } else if let Some(add) = rest.strip_prefix("add ") {
            let (name, definition) = split_head(add);
            let value = if definition.starts_with('{') {
                serde_json::from_str(definition)?
            } else {
                serde_json::to_value(config.resolve_model(definition)?)?
            };
            let updated = store::insert_named(path, "models", name, value)?;
            engine.replace_config(updated, false).await;
            self.selection.model = Some(name.into());
            self.selection.effort = None;
            self.note(format!("Added and selected model {name}"));
        } else {
            self.select_model(rest, engine).await?;
        }
        Ok(())
    }

    pub(super) async fn select_model(&mut self, reference: &str, engine: &Engine) -> Result<()> {
        self.require_idle()?;
        let selection = Selection {
            model: Some(reference.into()),
            effort: None,
            ..self.selection.clone()
        };
        engine.scope(&selection, "main", None).await?;
        self.selection = selection;
        self.note(format!("Switched model to {reference}"));
        Ok(())
    }

    async fn mcp_command(&mut self, rest: &str, engine: &Engine, path: &Path) -> Result<()> {
        if let Some(add) = rest.strip_prefix("add ") {
            self.require_idle()?;
            let (name, definition) = split_head(add);
            let updated =
                store::insert_named(path, "mcp_servers", name, serde_json::from_str(definition)?)?;
            let uuid = updated.mcp_servers[name].uuid.clone();
            engine.replace_config(updated, false).await;
            self.note(format!(
                "Added MCP {name} ({uuid}); connects when its tools are needed"
            ));
            return Ok(());
        }
        let config = engine.config.read().await.clone();
        if rest.is_empty() {
            let switches = engine.switches.read().await;
            self.picker = Some(Picker::mcps(&config, &switches));
            return Ok(());
        }
        let (mut name, mut action) = split_head(rest);
        if ["activate", "deactivate"].contains(&name) {
            std::mem::swap(&mut name, &mut action);
        }
        if !config.mcp_servers.contains_key(name) {
            bail!("Unknown MCP server: {name}");
        }
        if action == "restart" {
            self.require_idle()?;
            engine.mcp.stop(name).await;
        } else {
            engine
                .switches
                .write()
                .await
                .mcps
                .insert(name.into(), toggle(action)?);
        }
        self.note(format!("MCP {name}: {action}"));
        Ok(())
    }

    async fn tools_command(&mut self, rest: &str, engine: &Engine) -> Result<()> {
        let config = engine.config.read().await.clone();
        if rest.is_empty() {
            let switches = engine.switches.read().await;
            self.note(
                config
                    .builtins
                    .iter()
                    .chain(config.tools.keys())
                    .map(|name| format!("{} {name}", state(switches.tool_enabled(name, &config))))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        } else {
            let (name, action) = split_head(rest);
            if !config.tools.contains_key(name)
                && !config.builtins.iter().any(|n| n == name)
                && !name.starts_with("mcp_")
            {
                bail!("Unknown tool: {name}");
            }
            engine
                .switches
                .write()
                .await
                .tools
                .insert(name.into(), toggle(action)?);
            self.note(format!("Tool {name}: {action}"));
        }
        Ok(())
    }

    async fn effort_command(&mut self, rest: &str, engine: &Engine) -> Result<()> {
        if rest == "default" {
            self.selection.effort = None;
            return Ok(());
        }
        let mut scope = engine.scope(&self.selection, "main", None).await?;
        let reasoning = scope
            .model
            .reasoning
            .as_ref()
            .context("This model has no reasoning.supported_efforts configured")?;
        if rest.is_empty() {
            self.note(format!(
                "Effort: {}\nSupported: {}",
                reasoning
                    .effort
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "provider default".into()),
                reasoning
                    .supported_efforts
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        } else {
            let effort: Effort = rest.parse()?;
            scope.model.set_effort(effort)?;
            self.selection.effort = Some(effort);
            self.note(format!("Reasoning effort: {effort}"));
        }
        Ok(())
    }

    async fn agent_command(&mut self, rest: &str, engine: &Engine) -> Result<()> {
        if rest.is_empty() {
            let config = engine.config.read().await.clone();
            self.picker = Some(Picker::agents(&config));
            return Ok(());
        }
        let (name, agent_mode) = split_head(rest);
        let selection = Selection {
            // An explicit `default` clears the agent override so `engine.scope`
            // resolves the configured default agent (or the bare default); the
            // literal string is never stored.
            agent: (name != "default").then(|| name.into()),
            // An omitted second argument stays `None`; a supplied one is
            // resolved (and rejected, if unknown) by `engine.scope` below
            // along with agent-specific skills and model settings.
            agent_mode: (!agent_mode.is_empty()).then(|| agent_mode.into()),
            ..Selection::default()
        };
        engine.scope(&selection, "main", None).await?;
        self.selection = selection;
        self.mode = None;
        self.workflow_mode = None;
        // Same clearing as the picker apply path: leaving the workflow
        // means the parked front (if any) is no longer reproducible, so
        // the parking flag must drop with the workflow pointer.
        self.queue_blocked = None;
        self.note(if agent_mode.is_empty() {
            format!("Switched agent to {name}")
        } else {
            format!("Switched agent to {name} ({agent_mode})")
        });
        Ok(())
    }

    async fn mode_command(&mut self, rest: &str, engine: &Engine) -> Result<()> {
        let config = engine.config.read().await.clone();
        if rest == "default" || config.agents.contains_key(rest) {
            return self.agent_command(rest, engine).await;
        }
        if rest.is_empty() {
            self.note(format!(
                "Modes: {}",
                config.modes.keys().cloned().collect::<Vec<_>>().join(", ")
            ));
            return Ok(());
        }
        let (selection, workflow) = if rest == "default" {
            (Selection::default(), None)
        } else {
            let mode = config.modes.get(rest).context("Unknown mode")?;
            (
                Selection {
                    agent: mode.agent.clone(),
                    agent_mode: mode.agent_mode.clone(),
                    ..Selection::default()
                },
                mode.workflow.clone(),
            )
        };
        // Validate before changing UI state, including mode-specific skills.
        engine.scope(&selection, "main", None).await?;
        self.selection = selection;
        self.workflow_mode = workflow;
        self.mode = (rest != "default").then(|| rest.into());
        Ok(())
    }

    pub(super) async fn cycle_agent(&mut self, engine: &Engine, reverse: bool) -> Result<()> {
        // Match the /agent picker: every configured agent, hidden or not, so
        // a config with one visible agent still lets Tab reach the others.
        //
        // The bare "default" scope is offered only when no agent is marked
        // default; otherwise the configured default appears under its own
        // name. A configured agent literally named "default" is suppressed
        // only while no real default is configured: that legacy malformed
        // case would otherwise create a stuck duplicate cycling position.
        // When the configured default IS literally named "default", we keep
        // it visible and treat it as the default cycling position.
        let (agents, default_agent): (Vec<String>, Option<String>) = {
            let config = engine.config.read().await;
            let default_agent = config.default_agent_name();
            let agents = std::iter::once("default".to_owned())
                .filter(|_| default_agent.is_none())
                .chain(
                    config
                        .agents
                        .keys()
                        .filter(|name| !(name.as_str() == "default" && default_agent.is_none()))
                        .cloned(),
                )
                .collect();
            (agents, default_agent)
        };
        if agents.len() < 2 {
            self.status = "Only one agent is configured".into();
            return Ok(());
        }
        let default_agent_for_select = default_agent.clone();
        let current_name = self
            .selection
            .agent
            .clone()
            .or(default_agent)
            .unwrap_or_else(|| "default".into());
        let current = agents
            .iter()
            .position(|agent| *agent == current_name)
            .unwrap_or(0);
        let next = if reverse {
            (current + agents.len() - 1) % agents.len()
        } else {
            (current + 1) % agents.len()
        };
        let selected = agents[next].clone();
        // `selected == "default"` is the bare/synthetic scope only when no
        // agent is literally named "default" in the config; otherwise it
        // names the configured default agent. Pass the literal reference
        // through and let `engine.scope` resolve it.
        let selection = Selection {
            agent: (selected != "default" || default_agent_for_select.is_some())
                .then(|| selected.clone()),
            agent_mode: None,
            ..Selection::default()
        };
        engine.scope(&selection, "main", None).await?;
        self.selection = selection;
        self.mode = None;
        // `workflow_mode` is intentionally not cleared here: callers (Tab
        // handling, /agent command, picker apply) must each decide whether
        // cycling out of an agent should also drop the workflow pointer.
        // The Tab handler refuses to call this method while a workflow is
        // active; explicit /agent/picker calls clear it themselves.
        self.refresh_model(engine).await?;
        Ok(())
    }

    async fn skills_command(&mut self, rest: &str, engine: &Engine) -> Result<()> {
        let config = engine.config.read().await.clone();
        let discovered = skills::discover(&config)?;
        if rest.is_empty() {
            self.note(
                discovered
                    .iter()
                    .map(|s| {
                        format!(
                            "{} {} - {}",
                            state(config.skills.enabled.contains(&s.name)),
                            s.name,
                            s.description
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        } else {
            let (name, action) = split_head(rest);
            let enabled = toggle(action)?;
            if !discovered.iter().any(|s| s.name == name) {
                bail!("Skill not installed");
            }
            let mut config = engine.config.write().await;
            config.skills.enabled.retain(|s| s != name);
            if enabled {
                config.skills.enabled.push(name.into());
            }
        }
        Ok(())
    }

    async fn reset_session(&mut self, engine: &Engine) -> Result<()> {
        let config = engine.config.read().await.clone();
        let mut session = Session::open(&config.sessions_dir, None)?;
        session.add_redactions(config.secret_values());
        self.status = format!("Session {} | Ready", session.id);
        let mut previous = engine.session.lock().await;
        previous.checkpoint()?;
        *previous = session;
        // View-state reset: entries, activity spine, scroll, history view.
        // Runtime settings (selection, theme, mode, mouse, agent) and old
        // session files on disk are preserved.
        self.reset_view();
        self.input = Default::default();
        self.input_history.clear();
        self.spend = Default::default();
        self.context_tokens = 0;
        self.queued_inputs.clear();
        self.queue_blocked = None;
        self.workflow_complete = false;
        self.workflow_mode = None;
        self.last_workflow_input = None;
        Ok(())
    }
}

fn split_head(text: &str) -> (&str, &str) {
    let (head, rest) = text
        .trim()
        .split_once(char::is_whitespace)
        .unwrap_or((text.trim(), ""));
    (head, rest.trim())
}
fn toggle(value: &str) -> Result<bool> {
    match value {
        "on" | "activate" => Ok(true),
        "off" | "deactivate" => Ok(false),
        _ => bail!("Expected on/off (or activate/deactivate)"),
    }
}
fn state(enabled: bool) -> &'static str {
    if enabled {
        "on "
    } else {
        "off"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Message, UiEvent, Usage};
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };

    fn setup() -> (tempfile::TempDir, Engine, App, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{\"workspace\":\".\"}").unwrap();
        let config = Config::load(&path).unwrap();
        let app = App::new(&config, Selection::default());
        let session = Session::open(&config.sessions_dir, None).unwrap();
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        (dir, Engine::new(config, session, events), app, path)
    }
    #[tokio::test]
    async fn model_add_and_effort_are_validated_and_preserve_relative_config_paths() {
        let (_dir, engine, mut app, path) = setup();
        app.command(r#"/model add thinker {"provider":"openrouter","model":"test/reasoner","reasoning":{"supported_efforts":["low","high"],"effort":"low"}}"#,&engine,&path).await.unwrap();
        app.command("/effort high", &engine, &path).await.unwrap();
        assert_eq!(app.selection.effort, Some(Effort::High));
        assert_eq!(app.effort_label, "high");
        assert!(app.command("/effort medium", &engine, &path).await.is_err());
        assert_eq!(app.selection.effort, Some(Effort::High));
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(json["workspace"], ".");
        assert_eq!(
            json["models"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["name"] == "thinker")
                .unwrap()["reasoning"]["effort"],
            "low"
        );
        let before = std::fs::read(&path).unwrap();
        assert!(app
            .command("/model add thinker openrouter:x", &engine, &path)
            .await
            .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        app.command("/model openrouter:other", &engine, &path)
            .await
            .unwrap();
        assert_eq!(app.selection.effort, None);
        assert_eq!(app.model_label, "openrouter:other");
        assert!(app.command("/effort low", &engine, &path).await.is_err());
    }

    #[tokio::test]
    async fn skills_command_lists_discovered_local_skills_and_install_refreshes_discovery() {
        let (dir, engine, mut app, path) = setup();
        let skills_dir = dir.path().join("skills");
        let source = dir.path().join("source-skill");
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: local-skill\ndescription: A local test skill\n---\nUse the local fixture.\n",
        )
        .unwrap();
        engine.config.write().await.skills_dir = skills_dir.clone();

        app.command(
            &format!("/install-skill {}", source.display()),
            &engine,
            &path,
        )
        .await
        .unwrap();
        app.command("/skills", &engine, &path).await.unwrap();

        let listed = app
            .entries
            .iter()
            .rev()
            .find(|entry| entry.role == "status")
            .map(|entry| entry.text.as_str())
            .unwrap();
        assert!(listed.contains("local-skill"));
        assert!(listed.contains("A local test skill"));
        assert!(skills_dir.join("local-skill/SKILL.md").is_file());
    }

    #[tokio::test]
    async fn install_skill_rejects_duplicates_and_invalid_local_sources() {
        let (dir, engine, mut app, path) = setup();
        let source = dir.path().join("source-skill");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: duplicate-skill\ndescription: Already installed\n---\nInstructions.\n",
        )
        .unwrap();
        engine.config.write().await.skills_dir = dir.path().join("skills");

        app.command(
            &format!("/install-skill {}", source.display()),
            &engine,
            &path,
        )
        .await
        .unwrap();
        let duplicate = app
            .command(
                &format!("/install-skill {}", source.display()),
                &engine,
                &path,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(duplicate.contains("already installed"));

        let missing = app
            .command("/install-skill does-not-exist", &engine, &path)
            .await
            .unwrap_err()
            .to_string();
        assert!(!missing.is_empty());
    }

    #[tokio::test]
    async fn workflow_command_lists_names_and_starts_without_consuming_draft() {
        let (dir, engine, mut app, path) = setup();
        let workflows_dir = dir.path().join("workflows");
        std::fs::create_dir_all(&workflows_dir).unwrap();
        std::fs::write(
            workflows_dir.join("local.json"),
            r#"{
                "title": "Local workflow",
                "author": "test",
                "steps": [{
                    "model": "openrouter:test",
                    "prompt": "Respond to {{input}}",
                    "mcps": [],
                    "hitl": false
                }]
            }"#,
        )
        .unwrap();
        engine.config.write().await.workflows_dir = workflows_dir;

        app.command("/workflow", &engine, &path).await.unwrap();
        let listed = app
            .entries
            .iter()
            .rev()
            .find(|entry| entry.role == "status")
            .map(|entry| entry.text.as_str())
            .unwrap();
        assert!(listed.contains("local.json"));

        app.input
            .set("draft retained while selecting workflow".into());
        app.command("/workflow local workflow input", &engine, &path)
            .await
            .unwrap();
        assert_eq!(app.input.text, "draft retained while selecting workflow");
        assert_eq!(app.workflow_mode.as_deref(), Some("local"));
        assert_eq!(app.last_workflow_input.as_deref(), Some("workflow input"));
        assert!(app.busy.is_some());
        tokio::time::timeout(std::time::Duration::from_secs(2), app.cancel_and_join())
            .await
            .expect("workflow cancellation must be bounded");
    }

    #[tokio::test]
    async fn effort_default_clears_override_and_refreshes_effective_label() {
        let (_dir, engine, mut app, path) = setup();
        app.command(
            r#"/model add thinker {"provider":"openrouter","model":"test/reasoner","reasoning":{"supported_efforts":["low","high"],"effort":"low"}}"#,
            &engine,
            &path,
        )
        .await
        .unwrap();
        app.command("/effort high", &engine, &path).await.unwrap();
        assert_eq!(app.selection.effort, Some(Effort::High));
        assert_eq!(app.effort_label, "high");

        app.command("/effort default", &engine, &path)
            .await
            .unwrap();
        assert_eq!(app.selection.effort, None);
        assert_eq!(app.effort_label, "low");
    }

    #[tokio::test]
    async fn mcp_add_persists_definition_and_activation_is_runtime_only() {
        let (_dir, engine, mut app, path) = setup();
        app.command(
            r#"/mcp add local {"transport":"stdio","command":"never-started","enabled":false}"#,
            &engine,
            &path,
        )
        .await
        .unwrap();
        let loaded = Config::load(&path).unwrap();
        assert!(!loaded.mcp_servers["local"].uuid.is_empty());
        app.command("/mcp activate local", &engine, &path)
            .await
            .unwrap();
        assert!(engine.switches.read().await.mcp_enabled("local", &loaded));
        assert!(!Config::load(&path).unwrap().mcp_servers["local"].enabled);
        app.command("/mcp local off", &engine, &path).await.unwrap();
        assert!(!engine.switches.read().await.mcp_enabled("local", &loaded));
    }
    #[tokio::test]
    async fn export_then_clear_resets_spend_history_and_id_without_deleting_old_session() {
        let (_dir, engine, mut app, path) = setup();
        let old_id = engine.session.lock().await.id.clone();
        let old_path = engine.session.lock().await.path.clone();
        {
            let mut session = engine.session.lock().await;
            session
                .record_message("main", Message::new("user", "Remember this"))
                .unwrap();
            session
                .usage(
                    "main",
                    &Usage {
                        cost_microusd: Some(123),
                        ..Usage::default()
                    },
                )
                .unwrap();
        }
        app.command("/export", &engine, &path).await.unwrap();
        let exports = engine.config.read().await.exports_dir.clone();
        let exported = std::fs::read_dir(exports)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(std::fs::read_to_string(exported)
            .unwrap()
            .contains("Remember this"));
        app.command("/clear", &engine, &path).await.unwrap();
        let session = engine.session.lock().await;
        assert_ne!(session.id, old_id);
        assert!(session.messages.is_empty());
        assert_eq!(session.spend.microusd, 0);
        assert!(old_path.exists());
        assert!(app.entries.is_empty());
        assert_eq!(app.history_generation, 1);
    }
    #[tokio::test]
    async fn export_argument_resolves_from_workspace_and_preserves_session_state() {
        let (dir, engine, mut app, path) = setup();
        let reports = dir.path().join("reports");
        let (session_id, messages_json, spend_json) = {
            let mut session = engine.session.lock().await;
            session
                .record_message("main", Message::new("user", "Keep this session"))
                .unwrap();
            session
                .usage(
                    "main",
                    &Usage {
                        cost_microusd: Some(456),
                        ..Usage::default()
                    },
                )
                .unwrap();
            (
                session.id.clone(),
                serde_json::to_value(&session.messages).unwrap(),
                serde_json::to_value(&session.spend).unwrap(),
            )
        };

        app.command("/export ./reports", &engine, &path)
            .await
            .unwrap();

        assert!(reports.is_dir());
        let exported = std::fs::read_dir(&reports)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(exported.len(), 1);
        assert!(std::fs::read_to_string(&exported[0])
            .unwrap()
            .contains("Keep this session"));
        #[cfg(unix)]
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&exported[0].metadata().unwrap().permissions())
                & 0o077,
            0
        );
        assert!(!std::path::Path::new("reports").exists());

        let session = engine.session.lock().await;
        assert_eq!(session.id, session_id);
        assert_eq!(
            serde_json::to_value(&session.messages).unwrap(),
            messages_json
        );
        assert_eq!(serde_json::to_value(&session.spend).unwrap(), spend_json);
    }
    #[tokio::test]
    async fn colon_q_quits_without_sending_a_model_request() {
        let (_dir, engine, mut app, path) = setup();
        app.input.insert(":q");
        app.submit(&engine, &path).await.unwrap();
        assert!(app.quit);
        assert!(engine.session.lock().await.messages.is_empty());
    }

    #[test]
    fn context_events_only_track_the_main_conversation() {
        let mut app = App::new(&Config::default(), Selection::default());
        app.event(UiEvent::Context {
            context: "main".into(),
            tokens: 42,
        });
        assert_eq!(app.context_tokens, 42);
        app.event(UiEvent::Context {
            context: "subagent:x".into(),
            tokens: 99,
        });
        assert_eq!(app.context_tokens, 42);
    }

    #[tokio::test]
    async fn messages_can_be_queued_while_a_run_is_active() {
        let (_dir, engine, mut app, path) = setup();
        let cancel = tokio_util::sync::CancellationToken::new();
        app.busy = Some(super::super::app::Busy {
            cancel: cancel.clone(),
            task: tokio::spawn(async move {
                cancel.cancelled().await;
                Ok(String::new())
            }),
        });
        app.input.set("run this next".into());
        app.submit(&engine, &path).await.unwrap();
        assert!(app.input.text.is_empty());
        assert_eq!(
            app.queued_inputs.front().map(String::as_str),
            Some("run this next")
        );
        app.cancel_and_join().await;
    }

    #[tokio::test]
    async fn mouse_command_reports_and_changes_session_state_without_requiring_idle() {
        let (_dir, engine, mut app, path) = setup();
        assert!(app.mouse_enabled);

        app.command("/mouse", &engine, &path).await.unwrap();
        assert!(app.mouse_enabled);
        assert_eq!(app.status, "Mouse capture enabled");

        app.command("/mouse off", &engine, &path).await.unwrap();
        assert!(!app.mouse_enabled);
        app.command("/mouse toggle", &engine, &path).await.unwrap();
        assert!(app.mouse_enabled);
        app.command("/mouse on", &engine, &path).await.unwrap();
        assert!(app.mouse_enabled);
        app.command("/mouse off", &engine, &path).await.unwrap();
        assert!(!app.mouse_enabled);

        assert!(app.command("/mouse maybe", &engine, &path).await.is_err());
        assert!(!app.mouse_enabled);

        let cancel = tokio_util::sync::CancellationToken::new();
        let child_cancel = cancel.clone();
        app.busy = Some(super::super::app::Busy {
            cancel,
            task: tokio::spawn(async move {
                child_cancel.cancelled().await;
                Ok(String::new())
            }),
        });
        app.queued_inputs.push_back("already queued".into());
        app.input.set("/mouse on".into());
        app.submit(&engine, &path).await.unwrap();
        assert!(app.mouse_enabled);
        assert_eq!(app.queued_inputs.len(), 1);
        assert_eq!(
            app.queued_inputs.front().map(String::as_str),
            Some("already queued")
        );
        app.cancel_and_join().await;
    }

    #[tokio::test]
    async fn session_resets_and_reload_preserve_mouse_state() {
        let (_dir, engine, mut app, path) = setup();
        app.command("/mouse off", &engine, &path).await.unwrap();

        app.command("/clear", &engine, &path).await.unwrap();
        assert!(!app.mouse_enabled);
        app.command("/new", &engine, &path).await.unwrap();
        assert!(!app.mouse_enabled);
        app.command("/reload", &engine, &path).await.unwrap();
        assert!(!app.mouse_enabled);

        app.command("/mouse on", &engine, &path).await.unwrap();
        app.command("/clear", &engine, &path).await.unwrap();
        assert!(app.mouse_enabled);
        app.command("/reload", &engine, &path).await.unwrap();
        assert!(app.mouse_enabled);
    }

    #[tokio::test]
    async fn tab_switches_agents_silently_while_a_run_is_active() {
        let (_dir, engine, mut app, _path) = setup();
        {
            let mut config = engine.config.write().await;
            config.agents.insert(
                "alpha".into(),
                serde_json::from_value(serde_json::json!({"hidden":false})).unwrap(),
            );
            config.agents.insert(
                "beta".into(),
                serde_json::from_value(serde_json::json!({"hidden":false})).unwrap(),
            );
        }
        let cancel = tokio_util::sync::CancellationToken::new();
        app.busy = Some(super::super::app::Busy {
            cancel: cancel.clone(),
            task: tokio::spawn(async move {
                cancel.cancelled().await;
                Ok(String::new())
            }),
        });
        let entries = app.entries.len();
        let status = app.status.clone();
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &engine)
            .await
            .unwrap();
        assert_eq!(app.selection.agent.as_deref(), Some("alpha"));
        assert_eq!(app.entries.len(), entries);
        assert_eq!(app.status, status);
        app.cancel_and_join().await;
    }

    #[tokio::test]
    async fn tab_cycles_visible_agents_in_both_directions_preserving_the_draft() {
        let (_dir, engine, mut app, path) = setup();
        {
            let mut config = engine.config.write().await;
            config.agents.insert(
                "researcher".into(),
                serde_json::from_value(
                    serde_json::json!({"model":"openrouter:research-model","hidden":false}),
                )
                .unwrap(),
            );
            config.agents.insert(
                "reviewer".into(),
                serde_json::from_value(serde_json::json!({"hidden":true})).unwrap(),
            );
        }
        app.input.set("keep my draft 漢".into());
        app.input.left();
        let cursor = app.input.cursor;
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        app.handle_key(tab, &engine).await.unwrap();
        assert_eq!(app.selection.agent.as_deref(), Some("researcher"));
        assert_eq!(app.model_label, "openrouter:research-model");
        app.handle_key(tab, &engine).await.unwrap();
        assert_eq!(app.selection.agent.as_deref(), Some("reviewer"));
        app.handle_key(
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            &engine,
        )
        .await
        .unwrap();
        let mut released = tab;
        released.kind = KeyEventKind::Release;
        app.handle_key(released, &engine).await.unwrap();
        app.command("/help", &engine, &path).await.unwrap();
        app.handle_key(tab, &engine).await.unwrap();
        assert_eq!(app.selection.agent.as_deref(), Some("researcher"));
        assert_eq!(app.input.text, "keep my draft 漢");
        assert_eq!(app.input.cursor, cursor);
        assert!(engine.session.lock().await.messages.is_empty());
    }

    #[tokio::test]
    async fn agent_command_opens_picker_and_selects_without_losing_draft() {
        let (_dir, engine, mut app, path) = setup();
        {
            let mut config = engine.config.write().await;
            config.agents.insert(
                "researcher".into(),
                serde_json::from_value(serde_json::json!({
                    "model":"openrouter:research-model"
                }))
                .unwrap(),
            );
        }
        app.input.set("keep this draft".into());
        app.command("/agent", &engine, &path).await.unwrap();
        assert!(matches!(
            app.picker.as_ref().map(|picker| &picker.kind),
            Some(crate::tui::picker::PickerKind::Agents)
        ));
        app.paste("research");
        assert_eq!(app.picker.as_ref().unwrap().matches.len(), 1);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &engine)
            .await
            .unwrap();
        assert!(app.picker.is_none());
        assert_eq!(app.selection.agent.as_deref(), Some("researcher"));
        assert_eq!(app.input.text, "keep this draft");
        assert_eq!(app.model_label, "openrouter:research-model");
    }

    #[tokio::test]
    async fn agent_command_selects_named_and_default_modes_and_updates_scope_header_and_status() {
        let (_dir, engine, mut app, path) = setup();
        {
            let mut config = engine.config.write().await;
            config.agents.insert(
                "operator".into(),
                serde_json::from_value(serde_json::json!({
                    "default": true,
                    "model": "openrouter:base-model",
                    "system_prompt": "Operator instructions",
                    "tools": ["read_file", "shell"],
                    "mcp_servers": ["deep-mcp", "base-mcp"],
                    "modes": {
                        "deep": {
                            "model": "openrouter:deep-model",
                            "prompt": "Deep mode instructions",
                            "tools": ["read_file"],
                            "mcp_servers": ["deep-mcp"]
                        }
                    }
                }))
                .unwrap(),
            );
        }

        app.mode = Some("legacy-mode".into());
        app.command("/agent operator deep", &engine, &path)
            .await
            .unwrap();
        assert_eq!(app.selection.agent.as_deref(), Some("operator"));
        assert_eq!(app.selection.agent_mode.as_deref(), Some("deep"));
        assert!(app.mode.is_none());
        assert_eq!(app.model_label, "openrouter:deep-model");
        let scope = engine.scope(&app.selection, "main", None).await.unwrap();
        assert!(scope.system.contains("Operator instructions"));
        assert!(scope.system.contains("Deep mode instructions"));
        assert_eq!(
            scope.tools.as_deref(),
            Some(["read_file".to_owned()].as_slice())
        );
        assert_eq!(
            scope.mcps.as_deref(),
            Some(["deep-mcp".to_owned()].as_slice())
        );
        assert!(app
            .entries
            .last()
            .is_some_and(|entry| entry.text == "Switched agent to operator (deep)"));

        app.command("/agent default deep", &engine, &path)
            .await
            .unwrap();
        assert!(app.selection.agent.is_none());
        assert_eq!(app.selection.agent_mode.as_deref(), Some("deep"));
        assert!(app.selection.model.is_none());
        assert!(app.mode.is_none());
        assert_eq!(app.model_label, "openrouter:deep-model");
        let scope = engine.scope(&app.selection, "main", None).await.unwrap();
        assert_eq!(scope.model.provider, "openrouter");
        assert_eq!(scope.model.model, "deep-model");
        assert!(scope.system.contains("Operator instructions"));
        assert!(scope.system.contains("Deep mode instructions"));
        assert_eq!(
            scope.tools.as_deref(),
            Some(["read_file".to_owned()].as_slice())
        );
        assert_eq!(
            scope.mcps.as_deref(),
            Some(["deep-mcp".to_owned()].as_slice())
        );
        assert!(app
            .entries
            .last()
            .is_some_and(|entry| entry.text == "Switched agent to default (deep)"));
    }

    #[tokio::test]
    async fn agent_command_without_mode_clears_mode_override_and_restores_agent_model() {
        let (_dir, engine, mut app, path) = setup();
        {
            let mut config = engine.config.write().await;
            config.agents.insert(
                "operator".into(),
                serde_json::from_value(serde_json::json!({
                    "model": "openrouter:base-model",
                    "modes": {
                        "deep": {"model": "openrouter:deep-model"}
                    }
                }))
                .unwrap(),
            );
        }

        app.command("/agent operator deep", &engine, &path)
            .await
            .unwrap();
        app.command("/agent operator", &engine, &path)
            .await
            .unwrap();

        assert_eq!(app.selection.agent.as_deref(), Some("operator"));
        assert!(app.selection.agent_mode.is_none());
        assert_eq!(app.model_label, "openrouter:base-model");
        assert!(app
            .entries
            .last()
            .is_some_and(|entry| entry.text == "Switched agent to operator"));
    }

    #[tokio::test]
    async fn agent_command_rejects_unknown_mode_without_mutating_selection_or_draft() {
        let (_dir, engine, mut app, path) = setup();
        {
            let mut config = engine.config.write().await;
            config.agents.insert(
                "operator".into(),
                serde_json::from_value(serde_json::json!({
                    "model": "openrouter:base-model",
                    "modes": {"deep": {"model": "openrouter:deep-model"}}
                }))
                .unwrap(),
            );
        }

        app.command("/agent operator deep", &engine, &path)
            .await
            .unwrap();
        app.input.set("unsent draft".into());
        let selection = app.selection.clone();
        let mode = app.mode.clone();
        let status_entry_count = app.entries.len();

        assert!(app
            .command("/agent operator unknown", &engine, &path)
            .await
            .is_err());
        assert_eq!(app.selection.agent.as_deref(), selection.agent.as_deref());
        assert_eq!(
            app.selection.agent_mode.as_deref(),
            selection.agent_mode.as_deref()
        );
        assert_eq!(app.selection.model.as_deref(), selection.model.as_deref());
        assert_eq!(app.mode, mode);
        assert_eq!(app.input.text, "unsent draft");
        assert_eq!(app.entries.len(), status_entry_count);
    }

    #[tokio::test]
    async fn bare_agent_command_only_opens_picker_and_preserves_selection_and_draft() {
        let (_dir, engine, mut app, path) = setup();
        {
            let mut config = engine.config.write().await;
            config.agents.insert(
                "operator".into(),
                serde_json::from_value(serde_json::json!({
                    "modes": {"deep": {"model": "openrouter:deep-model"}}
                }))
                .unwrap(),
            );
        }
        app.selection.agent = Some("operator".into());
        app.selection.agent_mode = Some("deep".into());
        app.input.set("keep this draft".into());

        app.command("/agent", &engine, &path).await.unwrap();

        assert!(matches!(
            app.picker.as_ref().map(|picker| &picker.kind),
            Some(crate::tui::picker::PickerKind::Agents)
        ));
        assert_eq!(app.selection.agent.as_deref(), Some("operator"));
        assert_eq!(app.selection.agent_mode.as_deref(), Some("deep"));
        assert_eq!(app.input.text, "keep this draft");
    }

    #[tokio::test]
    async fn model_dialog_routes_search_paste_selection_and_cancel_without_submitting() {
        let (_dir, engine, mut app, path) = setup();
        {
            let mut config = engine.config.write().await;
            let provider = config.providers.get_mut("openrouter").unwrap();
            provider.base_url = "http://127.0.0.1:1".into();
            provider.api_key_env = None;
            provider.timeout_seconds = 1;
            config.models.insert(
                "thinker".into(),
                serde_json::from_value(serde_json::json!({
                    "model":"anthropic/claude-sonnet", "max_tokens":8192,
                    "reasoning":{"supported_efforts":["low","high"],"effort":"low"}
                }))
                .unwrap(),
            );
            config.agents.insert(
                "researcher".into(),
                serde_json::from_value(serde_json::json!({
                    "model":"openrouter:fallback", "modes":{"deep":{"model":"thinker"}}
                }))
                .unwrap(),
            );
        }
        app.selection.agent = Some("researcher".into());
        app.selection.agent_mode = Some("deep".into());
        app.input.set("unsent draft".into());
        app.command("/model", &engine, &path).await.unwrap();
        assert!(app.picker.is_some());
        assert_eq!(
            app.picker.as_ref().unwrap().current().unwrap().reference,
            "thinker"
        );
        app.paste("SNNT\r\n");
        assert_eq!(app.picker.as_ref().unwrap().matches.len(), 1);
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &engine)
            .await
            .unwrap();
        assert!(app.mode.is_none());
        assert!(!app
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &engine)
            .await
            .unwrap());
        assert!(app.picker.is_none());
        assert_eq!(app.selection.model.as_deref(), Some("thinker"));
        assert_eq!(app.effort_label, "low");
        assert_eq!(
            engine
                .scope(&app.selection, "main", None)
                .await
                .unwrap()
                .model
                .max_tokens,
            8192
        );
        app.command("/model", &engine, &path).await.unwrap();
        app.paste("no-match-xyz");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &engine)
            .await
            .unwrap();
        assert!(app.picker.is_some());
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &engine)
            .await
            .unwrap();
        assert!(app.picker.is_none());
        assert_eq!(app.selection.model.as_deref(), Some("thinker"));
        assert_eq!(app.input.text, "unsent draft");
        assert!(engine.session.lock().await.messages.is_empty());
    }

    #[tokio::test]
    async fn mcp_picker_toggles_runtime_state_and_yields_to_approvals() {
        let (_dir, engine, mut app, path) = setup();
        app.command(
            r#"/mcp add browser {"transport":"stdio","command":"never-started","enabled":false}"#,
            &engine,
            &path,
        )
        .await
        .unwrap();
        app.command(
            r#"/mcp add search {"transport":"stdio","command":"never-started","enabled":true}"#,
            &engine,
            &path,
        )
        .await
        .unwrap();
        let original = std::fs::read(&path).unwrap();
        app.input.set("keep the draft".into());
        app.command("/mcp", &engine, &path).await.unwrap();
        app.paste("brwsr");
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.picker.as_ref().unwrap().matches.len(), 1);
        assert!(!app.handle_key(enter, &engine).await.unwrap());
        assert_eq!(
            app.picker.as_ref().unwrap().current().unwrap().enabled,
            Some(true)
        );
        assert!(engine
            .switches
            .read()
            .await
            .mcp_enabled("browser", &*engine.config.read().await));
        // An approval arriving during the dialog keeps its keys and pasted text.
        let (reply, response) = tokio::sync::oneshot::channel();
        app.event(crate::model::UiEvent::Approval {
            title: "Approve?".into(),
            detail: "Test".into(),
            workflow: false,
            reply,
        });
        app.paste("not a search");
        app.handle_key(enter, &engine).await.unwrap();
        assert_eq!(app.picker.as_ref().unwrap().query.text, "brwsr");
        assert_eq!(
            app.picker.as_ref().unwrap().current().unwrap().enabled,
            Some(true)
        );
        app.handle_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            &engine,
        )
        .await
        .unwrap();
        assert!(matches!(
            response.await.unwrap(),
            crate::model::Decision::Reject
        ));
        app.handle_key(enter, &engine).await.unwrap();
        assert_eq!(
            app.picker.as_ref().unwrap().current().unwrap().enabled,
            Some(false)
        );
        app.handle_key(enter, &engine).await.unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &engine)
            .await
            .unwrap();
        assert!(app.picker.is_none());
        assert!(engine
            .switches
            .read()
            .await
            .mcp_enabled("browser", &*engine.config.read().await));
        assert_eq!(app.input.text, "keep the draft");
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(engine.session.lock().await.messages.is_empty());
    }

    #[tokio::test]
    async fn theme_picker_previews_restores_applies_and_reloads_without_writing_config() {
        let (_dir, engine, mut app, path) = setup();
        let configured = app.theme.clone();
        let original = std::fs::read(&path).unwrap();
        app.input.set("unsent draft".into());
        app.command("/theme", &engine, &path).await.unwrap();
        app.paste("hxx0r");
        assert_eq!(app.theme, themes::preset("haxx0r").unwrap());
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &engine)
            .await
            .unwrap();
        assert_eq!(app.theme, configured);
        app.command("/theme", &engine, &path).await.unwrap();
        app.paste("bnp");
        assert_eq!(app.theme, themes::preset("BnP").unwrap());
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &engine)
            .await
            .unwrap();
        assert!(app.picker.is_none());
        assert_eq!(app.theme, themes::preset("BnP").unwrap());
        app.command("/theme", &engine, &path).await.unwrap();
        app.paste("mama");
        assert_eq!(app.theme, themes::preset("mama_j").unwrap());
        app.handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &engine,
        )
        .await
        .unwrap();
        assert_eq!(app.theme, themes::preset("BnP").unwrap());
        assert!(app.command("/theme missing", &engine, &path).await.is_err());
        assert_eq!(app.theme, themes::preset("BnP").unwrap());
        app.command("/theme configured", &engine, &path)
            .await
            .unwrap();
        assert_eq!(app.theme, configured);
        app.theme.ascii = true;
        app.command("/theme blue", &engine, &path).await.unwrap();
        assert!(app.theme.ascii);
        app.command("/reload", &engine, &path).await.unwrap();
        assert_eq!(app.theme, configured);
        assert_eq!(app.input.text, "unsent draft");
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[tokio::test]
    async fn reload_reads_disk_changes_and_clears_old_selections_without_resetting_the_session() {
        let (_dir, engine, mut app, path) = setup();
        app.command("/model openrouter:temporary", &engine, &path)
            .await
            .unwrap();
        engine
            .switches
            .write()
            .await
            .tools
            .insert("web_fetch".into(), false);
        let session_id = engine.session.lock().await.id.clone();
        std::fs::write(
            &path,
            r#"{"workspace":".","theme":"diet_soda","model":{"model":"updated/model"}}"#,
        )
        .unwrap();
        app.command("/reload", &engine, &path).await.unwrap();
        assert_eq!(app.model_label, "openrouter:updated/model");
        assert!(app.selection.model.is_none());
        assert_eq!(app.theme, themes::preset("diet_soda").unwrap());
        assert!(engine.switches.read().await.tools.is_empty());
        assert_eq!(engine.session.lock().await.id, session_id);
        std::fs::write(&path, "broken JSON").unwrap();
        assert!(app.command("/reload", &engine, &path).await.is_err());
        assert_eq!(app.model_label, "openrouter:updated/model");
        assert_eq!(engine.config.read().await.model.model, "updated/model");
    }

    #[tokio::test]
    async fn reload_clears_queue_block_but_preserves_transcript_activity_and_truthful_note() {
        let (_dir, engine, mut app, path) = setup();
        app.message(
            "main".into(),
            Message::new("assistant", "visible transcript"),
        );
        app.event(UiEvent::Activity(crate::model::ActivityEvent {
            id: "activity".into(),
            parent_id: None,
            context: "main".into(),
            kind: crate::model::ActivityKind::Tool,
            phase: crate::model::ActivityPhase::Start,
            title: "visible activity".into(),
            external_id: None,
            status: None,
        }));
        app.focus = super::super::app::Focus::Activity;
        app.queue_blocked = Some(super::super::app::QueueBlocked {
            error: "start failed".into(),
            pending: 1,
        });
        app.status = "Queued 1 message(s) blocked | start failed; press Enter to retry".into();

        app.command("/reload", &engine, &path).await.unwrap();

        assert!(app.queue_blocked.is_none());
        assert_eq!(app.status, "Ready");
        assert!(app
            .entries
            .iter()
            .any(|entry| entry.text == "visible transcript"));
        assert_eq!(app.activities.len(), 1);
        assert_eq!(app.focus, super::super::app::Focus::Activity);
        assert!(app
            .entries
            .iter()
            .any(|entry| entry.text.contains("Configuration reloaded")));
    }

    #[test]
    fn help_documents_mouse_and_escape_behavior() {
        assert!(HELP.contains("/mouse [on|off|toggle]"));
        assert!(HELP.contains("Esc: return to input"));
        assert!(HELP.contains("Left-click: toggle a visible activity row"));
        assert!(HELP.contains("bare Esc cancels the active run"));
    }

    /// Drives a synthetic busy run so cancellation tests can observe a
    /// long-lived task without spinning up a real provider. The task is
    /// pre-cancelled so the run finishes the moment `cancel_and_join` is
    /// called.
    fn fake_busy(app: &mut App) {
        let cancel = tokio_util::sync::CancellationToken::new();
        app.busy = Some(super::super::app::Busy {
            cancel: cancel.clone(),
            task: tokio::spawn(async move {
                cancel.cancelled().await;
                Ok(String::new())
            }),
        });
    }

    #[tokio::test]
    async fn bare_escape_cancels_run_only_when_no_modal_is_open() {
        let (_dir, engine, mut app, _path) = setup();
        fake_busy(&mut app);
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.handle_key(esc, &engine).await.unwrap());
        assert_eq!(app.status, "Cancelling...");
        assert!(app.busy.is_some());
        app.cancel_and_join().await;
        assert!(app.busy.is_none());
    }

    #[tokio::test]
    async fn picker_escape_closes_picker_without_cancelling_run() {
        let (_dir, engine, mut app, path) = setup();
        app.command("/theme", &engine, &path).await.unwrap();
        assert!(app.picker.is_some());
        fake_busy(&mut app);
        let cancel_token = app.busy.as_ref().unwrap().cancel.clone();
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.handle_key(esc, &engine).await.unwrap());
        assert!(app.picker.is_none());
        assert!(!cancel_token.is_cancelled());
        app.cancel_and_join().await;
    }

    #[tokio::test]
    async fn help_escape_closes_help_without_cancelling_run() {
        let (_dir, engine, mut app, _path) = setup();
        app.command("/help", &engine, &_path).await.unwrap();
        assert!(app.help);
        fake_busy(&mut app);
        let cancel_token = app.busy.as_ref().unwrap().cancel.clone();
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.handle_key(esc, &engine).await.unwrap());
        assert!(!app.help);
        assert!(!cancel_token.is_cancelled());
        app.cancel_and_join().await;
    }

    #[tokio::test]
    async fn approval_escape_rejects_call_without_aborting_run() {
        let (_dir, engine, mut app, _path) = setup();
        fake_busy(&mut app);
        let cancel_token = app.busy.as_ref().unwrap().cancel.clone();
        let (reply, response) = tokio::sync::oneshot::channel();
        app.event(crate::model::UiEvent::Approval {
            title: "Approve?".into(),
            detail: "test".into(),
            workflow: false,
            reply,
        });
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.handle_key(esc, &engine).await.unwrap());
        assert!(matches!(
            response.await.unwrap(),
            crate::model::Decision::Reject
        ));
        assert!(app.approval.is_none());
        assert!(!cancel_token.is_cancelled(), "Esc must not abort the run");
        app.cancel_and_join().await;
    }

    #[tokio::test]
    async fn ctrl_c_aborts_active_run_when_no_modal_is_open() {
        let (_dir, engine, mut app, _path) = setup();
        fake_busy(&mut app);
        let cancel_token = app.busy.as_ref().unwrap().cancel.clone();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!app.handle_key(ctrl_c, &engine).await.unwrap());
        assert!(cancel_token.is_cancelled());
        app.cancel_and_join().await;
    }

    #[tokio::test]
    async fn release_events_are_dropped_even_when_shift_is_held() {
        let (_dir, engine, mut app, _path) = setup();
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut released = KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT);
        released.kind = KeyEventKind::Release;
        let mut shifted_char = KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT);
        shifted_char.kind = KeyEventKind::Release;
        // Tab release must not cycle the agent; shifted 'A' release must not
        // leak into the input buffer.
        app.handle_key(released, &engine).await.unwrap();
        app.handle_key(shifted_char, &engine).await.unwrap();
        assert!(app.selection.agent.is_none());
        assert!(app.input.text.is_empty());
        let _ = cancel;
    }

    #[tokio::test]
    async fn tab_is_blocked_with_a_clear_status_when_a_workflow_is_running() {
        let (_dir, engine, mut app, _path) = setup();
        fake_busy(&mut app);
        app.workflow_mode = Some("plan".into());
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        let status = app.status.clone();
        assert!(!app.handle_key(tab, &engine).await.unwrap());
        assert_eq!(app.status, "Agent cycling disabled in workflow mode");
        assert_ne!(app.status, status);
        assert!(app.selection.agent.is_none());
        let backtab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        let mut p = engine.config.write().await;
        p.agents.insert(
            "alpha".into(),
            serde_json::from_value(serde_json::json!({"hidden":false})).unwrap(),
        );
        drop(p);
        assert!(!app.handle_key(backtab, &engine).await.unwrap());
        assert!(app.selection.agent.is_none());
        // Workflow mode survives the blocked Tab: the run is still bound
        // to the workflow and the user must exit via q to clear it.
        assert_eq!(app.workflow_mode.as_deref(), Some("plan"));
        app.cancel_and_join().await;
    }

    #[tokio::test]
    async fn shifted_text_pastes_through_the_picker_and_filters_case_insensitively() {
        let (_dir, engine, mut app, path) = setup();
        {
            let mut config = engine.config.write().await;
            config.agents.insert(
                "researcher".into(),
                serde_json::from_value(serde_json::json!({"hidden":false})).unwrap(),
            );
            config.agents.insert(
                "REVIEWER".into(),
                serde_json::from_value(serde_json::json!({"hidden":false})).unwrap(),
            );
        }
        app.command("/agent", &engine, &path).await.unwrap();
        // Paste includes a shifted letter; the search filter is case
        // insensitive, so the filter must keep both candidate rows.
        app.paste("Re");
        let picker = app.picker.as_ref().unwrap();
        assert_eq!(picker.matches.len(), 2);
        assert_eq!(picker.query.text, "Re");
        // Shift+Tab in the picker moves selection backwards; Shift alone
        // must not be enough to interpret it as a back-tab from the chat.
        let shift_tab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        let selected_before = picker.selected;
        assert!(!app.handle_key(shift_tab, &engine).await.unwrap());
        let picker = app.picker.as_ref().unwrap();
        assert!(picker.selected <= selected_before);
        assert_eq!(picker.query.text, "Re");
        assert!(app.selection.agent.is_none());
    }

    #[tokio::test]
    async fn cycle_and_picker_skip_a_user_default_agent_when_no_real_default_exists() {
        // Legacy malformed: a user-named "default" agent with no real default
        // marked. The TUI must skip it so cycling/picking does not produce a
        // duplicate "default" entry that overlaps the synthetic scope.
        let (_dir, engine, mut app, _path) = setup();
        {
            let mut config = engine.config.write().await;
            config.agents.insert(
                "default".into(),
                serde_json::from_value(serde_json::json!({"hidden":false})).unwrap(),
            );
            config.agents.insert(
                "alpha".into(),
                serde_json::from_value(serde_json::json!({"hidden":false})).unwrap(),
            );
            config.agents.insert(
                "beta".into(),
                serde_json::from_value(serde_json::json!({"hidden":false})).unwrap(),
            );
        }
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &engine)
            .await
            .unwrap();
        let first = app.selection.agent.clone();
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &engine)
            .await
            .unwrap();
        let after = app.selection.agent.clone();
        assert!(after.as_deref() != Some("default"));
        assert_ne!(first, after);
        app.command("/agent", &engine, &std::path::PathBuf::from("/dev/null"))
            .await
            .unwrap();
        let picker = app.picker.as_ref().unwrap();
        let mut default_count = 0;
        for choice in &picker.choices {
            if choice.reference == "default" {
                default_count += 1;
            } else {
                assert!(
                    choice.reference == "alpha" || choice.reference == "beta",
                    "unexpected agent: {}",
                    choice.reference
                );
            }
        }
        assert_eq!(default_count, 1, "only the synthetic default may appear");
    }

    #[tokio::test]
    async fn cycle_and_picker_keep_user_default_agent_when_marked_default() {
        // The other legacy malformed case: a user-named "default" agent that
        // IS marked default. The TUI must surface it as the configured
        // default (with the marker), not hide it. Config validation will
        // separately reject the literal name in the future; here the TUI
        // matches the engine's resolved scope.
        let (_dir, engine, mut app, _path) = setup();
        {
            let mut config = engine.config.write().await;
            config.agents.insert(
                "default".into(),
                serde_json::from_value(serde_json::json!({
                    "default": true,
                    "hidden": false
                }))
                .unwrap(),
            );
            config.agents.insert(
                "alpha".into(),
                serde_json::from_value(serde_json::json!({"hidden":false})).unwrap(),
            );
        }
        // No synthetic "default" is added (default_agent.is_some()), and the
        // user-named "default" entry is no longer filtered out. The cycling
        // list therefore contains the configured default followed by every
        // other configured agent.
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &engine)
            .await
            .unwrap();
        // Tab from the bare default lands on the next entry (alpha); from
        // alpha Tab returns to the configured default.
        assert_eq!(app.selection.agent.as_deref(), Some("alpha"));
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &engine)
            .await
            .unwrap();
        assert_eq!(app.selection.agent.as_deref(), Some("default"));
        app.command("/agent", &engine, &std::path::PathBuf::from("/dev/null"))
            .await
            .unwrap();
        let picker = app.picker.as_ref().unwrap();
        let mut default_count = 0;
        let mut alpha_count = 0;
        for choice in &picker.choices {
            match choice.reference.as_str() {
                "default" => default_count += 1,
                "alpha" => alpha_count += 1,
                other => panic!("unexpected agent: {other}"),
            }
        }
        assert_eq!(default_count, 1, "the configured default must appear");
        assert_eq!(alpha_count, 1, "alpha must still appear");
        let default_choice = picker
            .choices
            .iter()
            .find(|c| c.reference == "default")
            .unwrap();
        assert!(default_choice.label.contains("(default)"));
    }

    #[tokio::test]
    async fn two_queued_messages_run_in_fifo_order() {
        let (_dir, engine, mut app, path) = setup();
        fake_busy(&mut app);
        app.input.set("first queued".into());
        app.submit(&engine, &path).await.unwrap();
        app.input.set("second queued".into());
        app.submit(&engine, &path).await.unwrap();
        assert_eq!(app.queued_inputs.len(), 2);
        assert_eq!(
            app.queued_inputs.front().map(String::as_str),
            Some("first queued")
        );
        app.cancel_and_join().await;
        // Drive finish_run with a busy task that has already finished.
        let busy_task = tokio::spawn(async { Ok::<String, anyhow::Error>(String::new()) });
        let _ = busy_task.await.unwrap();
        app.busy = Some(super::super::app::Busy {
            cancel: tokio_util::sync::CancellationToken::new(),
            task: tokio::spawn(async move { Ok(String::new()) }),
        });
        // Let the task settle so finish_run sees it as finished.
        tokio::task::yield_now().await;
        let (events_tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        drop(events_tx);
        assert!(app.finish_run(&engine, &mut rx).await);
        assert_eq!(
            app.queued_inputs.front().map(String::as_str),
            Some("second queued")
        );
        assert_eq!(app.queued_inputs.len(), 1);
        assert!(app.status.contains("Running"));
        assert!(app.status.contains("1 message(s) queued"));
        app.cancel_and_join().await;
    }

    #[tokio::test]
    async fn finish_run_marks_queue_blocked_when_workflow_file_is_missing() {
        // Active workflow finishes; the queued next item points at a missing
        // workflow file. The popped message is requeued at the front, the
        // queue is marked blocked, and busy is None — no busy retry loop.
        let (_dir, engine, mut app, _path) = setup();
        let dir = tempfile::tempdir().unwrap();
        let workflows_dir = dir.path().join("workflows");
        std::fs::create_dir_all(&workflows_dir).unwrap();
        let good = workflows_dir.join("good.json");
        std::fs::write(
            &good,
            r#"{
                "title": "good",
                "author": "tester",
                "steps": [{
                    "model": "openrouter:test",
                    "prompt": "hello",
                    "mcps": [],
                    "hitl": false
                }]
            }"#,
        )
        .unwrap();
        {
            let mut config = engine.config.write().await;
            config.workflows_dir = workflows_dir;
        }
        app.workflow_mode = Some("missing".into());
        app.queued_inputs.push_back("queued attempt".into());
        app.busy = Some(super::super::app::Busy {
            cancel: tokio_util::sync::CancellationToken::new(),
            task: tokio::spawn(async move { Ok(String::new()) }),
        });
        tokio::task::yield_now().await;
        let (_events_tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        drop(_events_tx);
        assert!(app.finish_run(&engine, &mut rx).await);
        assert_eq!(
            app.queued_inputs.front().map(String::as_str),
            Some("queued attempt")
        );
        assert_eq!(app.queued_inputs.len(), 1);
        assert!(app.status.contains("Queued 1 message(s) blocked"));
        assert!(app.status.contains("press Enter to retry"));
        let blocked = app
            .queue_blocked
            .as_ref()
            .expect("queue_blocked must be set on start failure");
        assert_eq!(blocked.pending, 1);
        assert!(!blocked.error.is_empty());
        assert!(app.busy.is_none());
        // Calling finish_run again on a blocked queue must NOT retry; the
        // user must explicitly press Enter.
        assert!(!app.finish_run(&engine, &mut rx).await);
        assert_eq!(
            app.queued_inputs.front().map(String::as_str),
            Some("queued attempt")
        );
    }

    #[tokio::test]
    async fn finish_run_preserves_fifo_and_marks_blocked_for_corrupt_workflow() {
        let (_dir, engine, mut app, _path) = setup();
        let dir = tempfile::tempdir().unwrap();
        let workflows_dir = dir.path().join("workflows");
        std::fs::create_dir_all(&workflows_dir).unwrap();
        let corrupt = workflows_dir.join("bad.json");
        std::fs::write(&corrupt, "this is not valid JSON").unwrap();
        {
            let mut config = engine.config.write().await;
            config.workflows_dir = workflows_dir;
        }
        // Two queued messages: the first must surface as an error with the
        // original FIFO order preserved, and the user must be told both are
        // still pending.
        app.workflow_mode = Some("bad".into());
        app.queued_inputs.push_back("first".into());
        app.queued_inputs.push_back("second".into());
        app.busy = Some(super::super::app::Busy {
            cancel: tokio_util::sync::CancellationToken::new(),
            task: tokio::spawn(async move { Ok(String::new()) }),
        });
        tokio::task::yield_now().await;
        let (_events_tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        drop(_events_tx);
        assert!(app.finish_run(&engine, &mut rx).await);
        assert_eq!(app.queued_inputs.front().map(String::as_str), Some("first"));
        assert_eq!(app.queued_inputs.back().map(String::as_str), Some("second"));
        assert_eq!(app.queued_inputs.len(), 2);
        assert!(app.status.contains("Queued 2 message(s) blocked"));
        assert!(app.busy.is_none());
        let blocked = app.queue_blocked.as_ref().unwrap();
        assert_eq!(blocked.pending, 2);
        // While in workflow mode the workflow-complete overlay is also raised
        // so the error is visible alongside the standard n/r/q choices.
        assert!(app.workflow_complete);
    }

    #[tokio::test]
    async fn mouse_wheel_routes_to_overlay_or_transcript_and_saturates() {
        let (_dir, _engine, mut app, _path) = setup();
        // Pre-fill scroll near the saturating boundary.
        app.scroll = usize::MAX;
        let wheel_up = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_mouse(wheel_up);
        assert_eq!(app.scroll, usize::MAX);
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.scroll, usize::MAX - 3);
        // Overlay scrolling while help is shown.
        app.help = true;
        app.overlay_scroll = 0;
        app.handle_mouse(wheel_up);
        assert_eq!(app.overlay_scroll, 3);
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.overlay_scroll, 0);
        app.help = false;
        // Approval overlay uses overlay_scroll, not scroll.
        let (reply, _response) = tokio::sync::oneshot::channel();
        app.event(crate::model::UiEvent::Approval {
            title: "x".into(),
            detail: "y".into(),
            workflow: false,
            reply,
        });
        let scroll_before = app.scroll;
        app.overlay_scroll = 1;
        app.handle_mouse(wheel_up);
        assert_eq!(app.overlay_scroll, 4);
        assert_eq!(app.scroll, scroll_before);
        // Non-wheel events are ignored (scroll is unchanged).
        let before = app.scroll;
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.scroll, before);
    }

    // ---- Wave 1 review: combined overlay routing and modal semantics ----

    fn esc() -> KeyEvent {
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
    }
    fn ctrl_c() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
    }

    /// Open a help overlay with a fake busy run, then verify the modal
    /// routing keeps `approval > help > workflow_complete > picker > bare`
    /// consistent across Esc and Ctrl+C.
    #[tokio::test]
    async fn help_modal_routes_keys_above_picker_and_run() {
        let (_dir, engine, mut app, _path) = setup();
        fake_busy(&mut app);
        let cancel_token = app.busy.as_ref().unwrap().cancel.clone();
        // Open a picker behind help.
        app.command("/theme", &engine, &_path).await.unwrap();
        assert!(app.picker.is_some());
        app.command("/help", &engine, &_path).await.unwrap();
        assert!(app.help);
        // Esc with help + picker + run: closes help, picker/run untouched.
        assert!(!app.handle_key(esc(), &engine).await.unwrap());
        assert!(!app.help);
        assert!(
            app.picker.is_some(),
            "Esc must not close the picker behind help"
        );
        assert!(!cancel_token.is_cancelled());
        // Reopen help, then Ctrl+C: closes help only.
        app.command("/help", &engine, &_path).await.unwrap();
        assert!(app.help);
        assert!(!app.handle_key(ctrl_c(), &engine).await.unwrap());
        assert!(!app.help);
        assert!(
            !cancel_token.is_cancelled(),
            "Ctrl+C must not cancel the run"
        );
        // Picker Esc closes picker without cancelling the run.
        assert!(!app.handle_key(esc(), &engine).await.unwrap());
        assert!(app.picker.is_none());
        assert!(!cancel_token.is_cancelled());
        app.cancel_and_join().await;
    }

    /// Bare `q` is a documented help-close key. With a picker open behind
    /// help and a run in flight it must close only help, leaving the picker
    /// and the active run alone.
    #[tokio::test]
    async fn bare_q_closes_help_without_touching_picker_or_run() {
        let (_dir, engine, mut app, _path) = setup();
        fake_busy(&mut app);
        let cancel_token = app.busy.as_ref().unwrap().cancel.clone();
        app.command("/theme", &engine, &_path).await.unwrap();
        assert!(app.picker.is_some());
        app.command("/help", &engine, &_path).await.unwrap();
        assert!(app.help);
        let bare_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(!app.handle_key(bare_q, &engine).await.unwrap());
        assert!(!app.help);
        assert!(
            app.picker.is_some(),
            "bare q must not close the picker behind help"
        );
        assert!(
            !cancel_token.is_cancelled(),
            "bare q must not cancel the run"
        );
        app.cancel_and_join().await;
    }

    /// Approval sits above every other modal: it intercepts Esc/Ctrl+C even
    /// when help, workflow-complete, or a picker are stacked behind.
    #[tokio::test]
    async fn approval_routes_keys_above_every_other_modal() {
        let (_dir, engine, mut app, _path) = setup();
        fake_busy(&mut app);
        let cancel_token = app.busy.as_ref().unwrap().cancel.clone();
        app.command("/help", &engine, &_path).await.unwrap();
        app.command("/theme", &engine, &_path).await.unwrap();
        let (reply, response) = tokio::sync::oneshot::channel();
        app.event(crate::model::UiEvent::Approval {
            title: "Approve?".into(),
            detail: "test".into(),
            workflow: false,
            reply,
        });
        // Esc rejects the approval and leaves help, picker, and the run alone.
        assert!(!app.handle_key(esc(), &engine).await.unwrap());
        assert!(matches!(
            response.await.unwrap(),
            crate::model::Decision::Reject
        ));
        assert!(app.help, "Esc with approval must not close help");
        assert!(
            app.picker.is_some(),
            "Esc with approval must not close picker"
        );
        assert!(!cancel_token.is_cancelled());
        // Tear down the stacked modals deterministically.
        app.help = false;
        app.picker = None;
        app.cancel_and_join().await;
    }

    /// Workflow-complete overlay sits above the picker. Picking a theme
    /// before completion must not be possible; pressing 'q' closes the
    /// workflow instead.
    #[tokio::test]
    async fn workflow_complete_modal_sits_above_picker() {
        let (_dir, engine, mut app, _path) = setup();
        // Drive the app into the workflow-complete overlay.
        app.workflow_mode = Some("plan".into());
        app.workflow_complete = true;
        app.last_workflow_input = Some("seed".into());
        // A picker is open behind the overlay.
        app.command("/theme", &engine, &_path).await.unwrap();
        assert!(app.picker.is_some());
        // Esc routes to the overlay (exits workflow mode), not the picker.
        assert!(!app.handle_key(esc(), &engine).await.unwrap());
        assert!(!app.workflow_complete);
        assert!(app.workflow_mode.is_none());
        assert!(
            app.picker.is_some(),
            "Esc must not close the picker behind overlay"
        );
        // Picker can now be closed with Esc.
        assert!(!app.handle_key(esc(), &engine).await.unwrap());
        assert!(app.picker.is_none());
    }

    #[tokio::test]
    async fn workflow_n_preserves_user_draft() {
        let (_dir, engine, mut app, _path) = setup();
        app.workflow_mode = Some("plan".into());
        app.workflow_complete = true;
        app.last_workflow_input = Some("seed".into());
        app.queued_inputs.push_back("queued first".into());
        app.queued_inputs.push_back("queued second".into());
        app.queue_blocked = Some(super::super::app::QueueBlocked {
            error: "synthetic".into(),
            pending: 2,
        });
        app.input.set("user draft I do not want to lose".into());
        let cursor = app.input.cursor;
        let enter = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);
        assert!(!app.handle_key(enter, &engine).await.unwrap());
        // 'n' clears workflow_complete and queue_blocked, but preserves the
        // user's typed draft, cursor, and queued messages.
        assert!(!app.workflow_complete);
        assert!(app.queue_blocked.is_none());
        assert_eq!(
            app.queued_inputs
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["queued first", "queued second"]
        );
        assert_eq!(app.input.text, "user draft I do not want to lose");
        assert_eq!(app.input.cursor, cursor);
    }

    #[tokio::test]
    async fn workflow_r_clears_queue_blocked_and_preserves_queued_inputs() {
        let (dir, engine, mut app, _path) = setup();
        let workflows_dir = dir.path().join("workflows");
        std::fs::create_dir_all(&workflows_dir).unwrap();
        std::fs::write(
            workflows_dir.join("plan.json"),
            r#"{
                "title": "plan",
                "author": "tester",
                "steps": [{
                    "model": "openrouter:test",
                    "prompt": "hello",
                    "mcps": [],
                    "hitl": false
                }]
            }"#,
        )
        .unwrap();
        engine.config.write().await.workflows_dir = workflows_dir;
        app.workflow_mode = Some("plan".into());
        app.workflow_complete = true;
        app.last_workflow_input = Some("repeat me".into());
        app.queued_inputs.push_back("queued first".into());
        app.queued_inputs.push_back("queued second".into());
        app.queue_blocked = Some(super::super::app::QueueBlocked {
            error: "synthetic".into(),
            pending: 2,
        });

        assert!(!app
            .handle_key(
                KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
                &engine
            )
            .await
            .unwrap());

        assert!(app.queue_blocked.is_none());
        assert_eq!(app.workflow_mode.as_deref(), Some("plan"));
        assert!(!app.workflow_complete);
        assert_eq!(
            app.queued_inputs
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["queued first", "queued second"]
        );
        app.cancel_and_join().await;
    }

    /// Tab is blocked for any non-empty `workflow_mode`: active, failed/idle,
    /// queue-blocked, and complete states all refuse to cycle the agent and
    /// must preserve workflow_mode/draft/selection. Outside workflow mode,
    /// Tab cycles normally. Workflow-complete's n/r/q overlay sits above
    /// the Tab branch and continues to work.
    #[tokio::test]
    async fn tab_is_blocked_across_all_workflow_mode_states() {
        let (_dir, engine, mut app, _path) = setup();
        {
            let mut config = engine.config.write().await;
            config.agents.insert(
                "alpha".into(),
                serde_json::from_value(serde_json::json!({"hidden":false})).unwrap(),
            );
            config.agents.insert(
                "beta".into(),
                serde_json::from_value(serde_json::json!({"hidden":false})).unwrap(),
            );
        }
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        let backtab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);

        // 1. Failed/idle: workflow_mode set, no busy run, no queue block,
        //    no workflow-complete overlay. Tab must not cycle nor clear it.
        app.workflow_mode = Some("plan".into());
        app.input.set("keep my draft 漢".into());
        let draft = app.input.text.clone();
        let cursor = app.input.cursor;
        assert!(!app.handle_key(tab, &engine).await.unwrap());
        assert!(!app.handle_key(backtab, &engine).await.unwrap());
        assert_eq!(app.status, "Agent cycling disabled in workflow mode");
        assert!(app.selection.agent.is_none());
        assert_eq!(app.workflow_mode.as_deref(), Some("plan"));
        assert_eq!(app.input.text, draft);
        assert_eq!(app.input.cursor, cursor);
        assert!(engine.session.lock().await.messages.is_empty());
        app.status = "Ready".into();

        // 2. Queue-blocked: queue_blocked set, workflow_mode still bound.
        //    Tab must not cycle nor clear workflow_mode nor the blocked queue.
        app.workflow_mode = Some("plan".into());
        app.queue_blocked = Some(super::super::app::QueueBlocked {
            error: "synthetic".into(),
            pending: 1,
        });
        app.queued_inputs.push_back("next".into());
        let queued = app.queued_inputs.clone();
        assert!(!app.handle_key(tab, &engine).await.unwrap());
        assert!(!app.handle_key(backtab, &engine).await.unwrap());
        assert_eq!(app.status, "Agent cycling disabled in workflow mode");
        assert!(app.selection.agent.is_none());
        assert_eq!(app.workflow_mode.as_deref(), Some("plan"));
        assert!(app.queue_blocked.is_some());
        assert_eq!(app.queued_inputs, queued);
        app.queue_blocked = None;
        app.queued_inputs.clear();
        app.status = "Ready".into();

        // 3. Workflow-complete: the overlay handles its own keys above Tab,
        //    so Tab reaches the workflow branch only if it slipped past —
        //    either way, workflow_mode must survive any blocked Tab here.
        app.workflow_mode = Some("plan".into());
        app.workflow_complete = true;
        app.last_workflow_input = Some("seed".into());
        let draft = "user draft".to_string();
        app.input.set(draft.clone());
        let before_status = app.status.clone();
        assert!(!app.handle_key(tab, &engine).await.unwrap());
        // The workflow-complete handler matched before the Tab branch, so the
        // overlay is still up. Tab must not have opened the cycling path nor
        // dropped workflow_mode.
        assert!(app.workflow_complete);
        assert_eq!(app.workflow_mode.as_deref(), Some("plan"));
        assert_eq!(app.input.text, draft);
        // The overlay absorbs Tab silently; status is whatever it was before.
        assert_eq!(app.status, before_status);
        // q clears the overlay and exits workflow mode the documented way.
        assert!(!app
            .handle_key(
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
                &engine
            )
            .await
            .unwrap());
        assert!(!app.workflow_complete);
        assert!(app.workflow_mode.is_none());
        app.status = "Ready".into();

        // 4. Normal mode: workflow_mode is None, Tab cycles the agent
        //    silently. This is the regression guard against the new
        //    branch accidentally swallowing ordinary cycling.
        app.workflow_mode = None;
        app.status = "Ready".into();
        let status = app.status.clone();
        let draft = "kept draft".to_string();
        app.input.set(draft.clone());
        let cursor = app.input.cursor;
        let before = app.selection.agent.clone();
        assert!(!app.handle_key(tab, &engine).await.unwrap());
        assert_ne!(app.selection.agent, before);
        assert_eq!(app.status, status);
        assert_eq!(app.input.text, draft);
        assert_eq!(app.input.cursor, cursor);
        assert_eq!(app.workflow_mode, None);
        // BackTab returns to the original selection.
        assert!(!app.handle_key(backtab, &engine).await.unwrap());
        assert_eq!(app.selection.agent, before);
        assert!(engine.session.lock().await.messages.is_empty());
    }

    /// Empty Enter while the queue is blocked retries the front item
    /// without disturbing the user's draft.
    #[tokio::test]
    async fn empty_enter_retries_blocked_queue_preserving_draft() {
        let (_dir, engine, mut app, path) = setup();
        let dir = tempfile::tempdir().unwrap();
        let workflows_dir = dir.path().join("workflows");
        std::fs::create_dir_all(&workflows_dir).unwrap();
        // Workflow file exists so retries succeed; the queue still has the
        // blocked item from a prior start that failed for a different reason.
        let good = workflows_dir.join("good.json");
        std::fs::write(
            &good,
            r#"{
                "title": "good",
                "author": "tester",
                "steps": [{
                    "model": "openrouter:test",
                    "prompt": "hello",
                    "mcps": [],
                    "hitl": false
                }]
            }"#,
        )
        .unwrap();
        {
            let mut config = engine.config.write().await;
            config.workflows_dir = workflows_dir;
        }
        app.workflow_mode = Some("good".into());
        app.queued_inputs.push_back("queued".into());
        // Pre-mark blocked so the next Enter triggers retry_queued.
        app.queue_blocked = Some(super::super::app::QueueBlocked {
            error: "synthetic".into(),
            pending: 1,
        });
        app.input.set("".into());
        // Empty Enter retries the front; the user's draft (empty here) is
        // preserved as empty because the retry path does not consume input.
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let submit = app.handle_key(enter, &engine).await.unwrap();
        assert!(submit);
        app.submit(&engine, &path).await.unwrap();
        assert!(app.queue_blocked.is_none());
        assert!(app.busy.is_some());
        assert!(app.queued_inputs.is_empty());
        // Cancel and drain.
        app.workflow_mode = None;
        app.cancel_and_join().await;

        // Second part: a typed draft followed by Enter does NOT trigger
        // retry; the new draft is submitted normally and joins the queue
        // behind the still-blocked front item.
        app.workflow_mode = Some("good".into());
        app.queued_inputs.push_back("front".into());
        app.queue_blocked = Some(super::super::app::QueueBlocked {
            error: "synthetic".into(),
            pending: 1,
        });
        // Mark busy so the new draft gets queued (not started).
        let cancel = tokio_util::sync::CancellationToken::new();
        app.busy = Some(super::super::app::Busy {
            cancel: cancel.clone(),
            task: tokio::spawn(async move {
                cancel.cancelled().await;
                Ok(String::new())
            }),
        });
        let draft = "this is the user draft they want to keep";
        app.input.set(draft.into());
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let submit = app.handle_key(enter, &engine).await.unwrap();
        assert!(submit);
        app.submit(&engine, &path).await.unwrap();
        assert_eq!(app.input.text, "");
        assert_eq!(app.queued_inputs.front().map(String::as_str), Some("front"));
        assert_eq!(app.queued_inputs.back().map(String::as_str), Some(draft));
        assert!(app.queue_blocked.is_some());
        app.cancel_and_join().await;
    }

    /// When the underlying start succeeds, Enter clears the block and the
    /// run starts; subsequent `submit` calls fall through to the normal
    /// busy/queued path.
    #[tokio::test]
    async fn enter_retry_succeeds_and_clears_blocked_state() {
        let (_dir, engine, mut app, _path) = setup();
        let dir = tempfile::tempdir().unwrap();
        let workflows_dir = dir.path().join("workflows");
        std::fs::create_dir_all(&workflows_dir).unwrap();
        let good = workflows_dir.join("good.json");
        std::fs::write(
            &good,
            r#"{
                "title": "good",
                "author": "tester",
                "steps": [{
                    "model": "openrouter:test",
                    "prompt": "hello",
                    "mcps": [],
                    "hitl": false
                }]
            }"#,
        )
        .unwrap();
        {
            let mut config = engine.config.write().await;
            config.workflows_dir = workflows_dir;
        }
        app.workflow_mode = Some("good".into());
        // Pre-mark blocked so the next Enter triggers retry_queued.
        app.queue_blocked = Some(super::super::app::QueueBlocked {
            error: "synthetic".into(),
            pending: 1,
        });
        app.queued_inputs.push_back("go".into());
        app.input.set("".into());
        // The event loop calls submit when handle_key returns Ok(true);
        // mirror that here so the retry path actually runs.
        let submit = app
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &engine)
            .await
            .unwrap();
        assert!(submit, "Enter on empty input must request submission");
        app.submit(&engine, &_path).await.unwrap();
        assert!(
            app.queue_blocked.is_none(),
            "queue_blocked should be cleared: status={}, busy={:?}, workflow_mode={:?}, queued={:?}, input={:?}",
            app.status,
            app.busy.is_some(),
            app.workflow_mode,
            app.queued_inputs,
            app.input.text
        );
        assert!(app.busy.is_some());
        // Clear workflow_mode so cancel_and_join drains the busy task.
        app.workflow_mode = None;
        app.cancel_and_join().await;
    }

    /// `/clear` and `/new` clear any blocked/retry state along with the
    /// session, queue, and workflow pointers.
    #[tokio::test]
    async fn clear_resets_blocked_queue_and_workflow_state() {
        let (_dir, engine, mut app, path) = setup();
        app.workflow_mode = Some("plan".into());
        app.workflow_complete = true;
        app.last_workflow_input = Some("seed".into());
        app.queued_inputs.push_back("a".into());
        app.queued_inputs.push_back("b".into());
        app.queue_blocked = Some(super::super::app::QueueBlocked {
            error: "synthetic".into(),
            pending: 2,
        });
        app.input.set("draft to drop".into());
        app.command("/clear", &engine, &path).await.unwrap();
        assert!(app.queue_blocked.is_none());
        assert!(app.queued_inputs.is_empty());
        assert!(app.workflow_mode.is_none());
        assert!(!app.workflow_complete);
        assert!(app.last_workflow_input.is_none());
        assert_eq!(app.input.text, "");
    }

    /// `/clear` and `/new` route through the App `reset_view` helper:
    /// chat entries, streams, activity spine (nodes, indexes, pending
    /// ends, timeline) and scroll all clear, while runtime settings
    /// (selection, theme) survive. `/reload` does NOT call `reset_view`:
    /// it preserves the transcript and activity spine so the user's
    /// mid-session reload keeps the view intact.
    #[tokio::test]
    async fn clear_clears_view_state_and_reload_preserves_it() {
        let (_dir, engine, mut app, path) = setup();
        // Populate every view field the helper clears.
        app.message("main".into(), Message::new("user", "keep me"));
        app.event(UiEvent::Delta {
            context: "main".into(),
            text: "partial".into(),
        });
        app.event(UiEvent::Activity(crate::model::ActivityEvent {
            id: "tool-1".into(),
            parent_id: None,
            context: "main".into(),
            kind: crate::model::ActivityKind::Tool,
            phase: crate::model::ActivityPhase::Start,
            title: "tool-1 title".into(),
            external_id: Some("req-1".into()),
            status: None,
        }));
        app.scroll = 12;
        app.history_index = 1;
        app.input_history.push("prior".into());
        let theme_before = app.theme.clone();
        let selection_agent_before = app.selection.agent.clone();
        let selection_model_before = app.selection.model.clone();

        // /clear must clear every view field but preserve runtime
        // settings.
        app.command("/clear", &engine, &path).await.unwrap();
        assert!(app.entries.is_empty(), "/clear clears entries");
        assert!(app.streams.is_empty(), "/clear clears streams");
        assert!(app.timeline.is_empty(), "/clear clears timeline");
        assert!(app.activities.is_empty(), "/clear clears activities");
        assert!(
            app.activity_index.is_empty(),
            "/clear clears activity_index"
        );
        assert!(
            app.pending_activity_ends.is_empty(),
            "/clear clears pending_activity_ends"
        );
        assert!(
            app.activity_by_context.is_empty(),
            "/clear clears activity_by_context"
        );
        assert!(
            app.activity_by_external.is_empty(),
            "/clear clears activity_by_external"
        );
        assert_eq!(app.scroll, 0, "/clear resets scroll");
        assert_eq!(app.history_index, 0, "/clear resets history view");
        assert_eq!(app.theme, theme_before, "/clear preserves theme");
        assert_eq!(
            app.selection.agent, selection_agent_before,
            "/clear preserves selection.agent"
        );
        assert_eq!(
            app.selection.model, selection_model_before,
            "/clear preserves selection.model"
        );

        // /new (alias for /clear) hits the same path: every view field
        // goes back to empty.
        app.message("main".into(), Message::new("user", "again"));
        app.event(UiEvent::Delta {
            context: "main".into(),
            text: "again".into(),
        });
        assert!(!app.entries.is_empty(), "pre-/new has content to clear");
        app.command("/new", &engine, &path).await.unwrap();
        assert!(app.entries.is_empty(), "/new clears entries");
        assert!(app.streams.is_empty(), "/new clears streams");
        assert!(app.timeline.is_empty(), "/new clears timeline");
        assert!(app.activities.is_empty(), "/new clears activities");

        // /reload must NOT clear the view: populate the view once more,
        // then reload and confirm the counts are identical to what was
        // there just before the reload call. The earlier `/clear`/`/new`
        // calls reset model/effort/runtime state; the transcript and
        // activity spine are what we care about here.
        app.message("main".into(), Message::new("user", "survive"));
        app.event(UiEvent::Delta {
            context: "main".into(),
            text: "delta".into(),
        });
        app.event(UiEvent::Activity(crate::model::ActivityEvent {
            id: "tool-survives".into(),
            parent_id: None,
            context: "main".into(),
            kind: crate::model::ActivityKind::Tool,
            phase: crate::model::ActivityPhase::Start,
            title: "tool-survives".into(),
            external_id: Some("req-survives".into()),
            status: None,
        }));
        app.scroll = 5;
        let entries_before_reload = app.entries.len();
        let streams_before_reload = app.streams.len();
        let timeline_before_reload = app.timeline.len();
        let activities_before_reload = app.activities.len();
        assert!(entries_before_reload >= 1);
        assert!(activities_before_reload >= 1);
        app.command("/reload", &engine, &path).await.unwrap();
        // /reload appends one status note ("Configuration reloaded; ...")
        // — that note is existing behavior, not part of the view-state
        // reset contract. The transcript is otherwise unchanged.
        assert_eq!(
            app.entries.len(),
            entries_before_reload + 1,
            "/reload preserves transcript entries (plus the reload status note)"
        );
        assert_eq!(
            app.streams.len(),
            streams_before_reload,
            "/reload preserves streams"
        );
        assert_eq!(
            app.timeline.len(),
            timeline_before_reload + 1,
            "/reload preserves timeline (plus the reload status note)"
        );
        assert_eq!(
            app.activities.len(),
            activities_before_reload,
            "/reload preserves activity spine"
        );
        assert_eq!(app.scroll, 5, "/reload preserves scroll");
        // The status note itself is present.
        let has_note = app
            .entries
            .iter()
            .any(|e| e.text.contains("Configuration reloaded"));
        assert!(has_note, "/reload still surfaces its status note");
    }

    /// Two queued items are surfaced in FIFO order with both pending counts
    /// after a corrupt-workflow start; a subsequent successful drain runs
    /// them in order.
    #[tokio::test]
    async fn two_queued_items_keep_fifo_across_blocked_then_success() {
        let (_dir, engine, mut app, _path) = setup();
        let dir = tempfile::tempdir().unwrap();
        let workflows_dir = dir.path().join("workflows");
        std::fs::create_dir_all(&workflows_dir).unwrap();
        let good = workflows_dir.join("good.json");
        std::fs::write(
            &good,
            r#"{
                "title": "good",
                "author": "tester",
                "steps": [{
                    "model": "openrouter:test",
                    "prompt": "hello",
                    "mcps": [],
                    "hitl": false
                }]
            }"#,
        )
        .unwrap();
        let bad = workflows_dir.join("bad.json");
        std::fs::write(&bad, "this is not valid JSON").unwrap();
        {
            let mut config = engine.config.write().await;
            config.workflows_dir = workflows_dir;
        }
        app.workflow_mode = Some("bad".into());
        app.queued_inputs.push_back("first".into());
        app.queued_inputs.push_back("second".into());
        app.busy = Some(super::super::app::Busy {
            cancel: tokio_util::sync::CancellationToken::new(),
            task: tokio::spawn(async move { Ok(String::new()) }),
        });
        tokio::task::yield_now().await;
        let (_events_tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        drop(_events_tx);
        assert!(app.finish_run(&engine, &mut rx).await);
        assert_eq!(app.queued_inputs.front().map(String::as_str), Some("first"));
        assert_eq!(app.queued_inputs.back().map(String::as_str), Some("second"));
        assert_eq!(app.queued_inputs.len(), 2);
        // Repair the workflow by switching to a good one; the user presses
        // Enter to retry, which now succeeds. The front of the queue runs
        // immediately and the second item stays queued.
        app.workflow_mode = Some("good".into());
        app.queue_blocked = Some(super::super::app::QueueBlocked {
            error: "synthetic".into(),
            pending: 2,
        });
        app.input.set("".into());
        let submit = app
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &engine)
            .await
            .unwrap();
        assert!(submit);
        app.submit(&engine, &_path).await.unwrap();
        assert!(app.queue_blocked.is_none());
        assert!(app.busy.is_some());
        // The remaining queued message is still in place behind the active run.
        assert_eq!(
            app.queued_inputs.front().map(String::as_str),
            Some("second")
        );
        app.workflow_mode = None;
        app.cancel_and_join().await;
    }

    /// Wheel events with an open picker scroll the picker instead of the
    /// hidden transcript.
    #[tokio::test]
    async fn mouse_wheel_routes_to_open_picker() {
        let (_dir, engine, mut app, path) = setup();
        {
            let mut config = engine.config.write().await;
            for i in 0..6 {
                let mut model = config.model.clone();
                model.model = format!("model-{i}");
                config.models.insert(format!("alias-{i}"), model);
            }
        }
        app.command("/model", &engine, &path).await.unwrap();
        let scroll_before = app.scroll;
        assert!(app.picker.is_some());
        // Whatever row the picker is parked on, capture it and confirm wheel
        // events move it instead of the hidden transcript.
        let initial = app.picker.as_ref().unwrap().selected;
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        let after_down = app.picker.as_ref().unwrap().selected;
        assert!(
            after_down >= initial,
            "ScrollDown must move selection forward or saturate"
        );
        assert_eq!(app.scroll, scroll_before);
        // ScrollUp moves back, also saturating.
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        let after_up = app.picker.as_ref().unwrap().selected;
        assert!(
            after_up <= after_down,
            "ScrollUp must move selection backward or saturate"
        );
        // Non-wheel events are still ignored while the picker is open.
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.picker.as_ref().unwrap().selected, after_up);
        // Close the picker; wheel now scrolls the transcript again. The
        // transcript convention is the inverse of the picker: ScrollDown
        // brings newer content into view (scroll decreases toward 0), and
        // ScrollUp goes back into history (scroll grows).
        assert!(!app
            .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &engine)
            .await
            .unwrap());
        assert!(app.picker.is_none());
        app.scroll = 6;
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.scroll, 3);
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.scroll, 6);
    }

    /// Fallible picker selection (model/agent) reopens the dialog on
    /// failure so the user can retry without losing the search context.
    #[tokio::test]
    async fn picker_preserves_state_on_fallible_model_selection() {
        let (_dir, engine, mut app, path) = setup();
        {
            let mut config = engine.config.write().await;
            // Model is registered but references an unknown provider, so
            // `select_model` (which calls engine.scope) fails on selection.
            config.models.insert(
                "broken".into(),
                serde_json::from_value(serde_json::json!({
                    "model": "does-not-resolve",
                    "provider": "ghost-provider",
                    "max_tokens": 1
                }))
                .unwrap(),
            );
        }
        app.command("/model", &engine, &path).await.unwrap();
        app.paste("brkn");
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(!app.handle_key(enter, &engine).await.unwrap());
        // Picker must still be open so the user can search again.
        assert!(app.picker.is_some(), "picker must reopen on scope failure");
        // The transcript received an error message; the picker reopens
        // so the user can retry without retyping the search.
        let error_entry = app
            .entries
            .iter()
            .rev()
            .find(|e| e.role == "error")
            .map(|e| e.text.clone());
        assert!(error_entry.is_some(), "expected an error entry on failure");
        assert!(
            app.status.contains("Model not selected"),
            "status must explain why the model was not selected: {}",
            app.status
        );
    }

    #[tokio::test]
    async fn picker_preserves_state_on_fallible_agent_selection() {
        let (_dir, engine, mut app, path) = setup();
        {
            let mut config = engine.config.write().await;
            // The agent's model points at an unknown provider so engine.scope
            // fails with a validation error at picker-select time.
            config.agents.insert(
                "broken".into(),
                serde_json::from_value(serde_json::json!({
                    "model": "ghost-provider:foo"
                }))
                .unwrap(),
            );
        }
        app.command("/agent", &engine, &path).await.unwrap();
        app.paste("brkn");
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(!app.handle_key(enter, &engine).await.unwrap());
        assert!(
            app.picker.is_some(),
            "agent picker must reopen on scope failure"
        );
        assert!(
            app.selection.agent.is_none(),
            "selection must not be changed"
        );
    }

    // ---- Wave 2 review: queue-blocked invariants, overlay scroll, status ----

    /// A non-empty draft entered while the queue is blocked and busy is
    /// None must queue behind the parked front, leave `busy` untouched,
    /// and clear the input only after the new item is safely appended.
    /// The previous code path clobbered the parked front by calling
    /// `start_input`, leaving an orphan run that the user could not
    /// retry.
    #[tokio::test]
    async fn queue_blocked_with_draft_and_no_busy_queues_behind_parked_front() {
        let (_dir, engine, mut app, path) = setup();
        let dir = tempfile::tempdir().unwrap();
        let workflows_dir = dir.path().join("workflows");
        std::fs::create_dir_all(&workflows_dir).unwrap();
        let good = workflows_dir.join("good.json");
        std::fs::write(
            &good,
            r#"{
                "title": "good",
                "author": "tester",
                "steps": [{
                    "model": "openrouter:test",
                    "prompt": "hello",
                    "mcps": [],
                    "hitl": false
                }]
            }"#,
        )
        .unwrap();
        {
            let mut config = engine.config.write().await;
            config.workflows_dir = workflows_dir;
        }
        app.workflow_mode = Some("good".into());
        app.queued_inputs.push_back("parked-front".into());
        app.queue_blocked = Some(super::super::app::QueueBlocked {
            error: "synthetic".into(),
            pending: 1,
        });
        let draft = "user draft that must be queued, not started";
        app.input.set(draft.into());
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.handle_key(enter, &engine).await.unwrap());
        app.submit(&engine, &path).await.unwrap();
        // Draft is appended behind the parked front: FIFO preserved.
        assert_eq!(app.queued_inputs.len(), 2);
        assert_eq!(
            app.queued_inputs.front().map(String::as_str),
            Some("parked-front")
        );
        assert_eq!(app.queued_inputs.back().map(String::as_str), Some(draft));
        // No orphan run started; the parking flag is still up so the user
        // can retry the parked front.
        assert!(app.busy.is_none(), "must not start an orphan run");
        assert!(
            app.queue_blocked.is_some(),
            "parking flag must remain until the user retries"
        );
        // Input is cleared only after the draft is safely queued.
        assert_eq!(app.input.text, "");
        // The queued draft is in the user's history so re-running is possible.
        assert!(app.input_history.iter().any(|s| s == draft));
        app.cancel_and_join().await;
    }

    /// A non-empty draft entered while the queue is blocked and busy is
    /// Some must still queue behind the parked front without disturbing
    /// the in-flight run.
    #[tokio::test]
    async fn queue_blocked_with_draft_and_busy_queues_behind_parked_front() {
        let (_dir, engine, mut app, path) = setup();
        fake_busy(&mut app);
        // Snapshot the running task: any replacement would change its
        // finished state by the time submit returns.
        app.workflow_mode = Some("good".into());
        app.queued_inputs.push_back("parked-front".into());
        app.queue_blocked = Some(super::super::app::QueueBlocked {
            error: "synthetic".into(),
            pending: 1,
        });
        let draft = "draft while busy and blocked";
        app.input.set(draft.into());
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.handle_key(enter, &engine).await.unwrap());
        app.submit(&engine, &path).await.unwrap();
        assert_eq!(app.queued_inputs.len(), 2);
        assert_eq!(
            app.queued_inputs.front().map(String::as_str),
            Some("parked-front")
        );
        assert_eq!(app.queued_inputs.back().map(String::as_str), Some(draft));
        assert!(app.queue_blocked.is_some());
        // busy must still be set after submit and the original task must
        // not have been swapped for a fresh one — if submit had started
        // a new run, the prior handle would have been dropped.
        let busy_after = app.busy.as_ref().expect("busy must remain Some");
        assert!(!busy_after.task.is_finished());
        assert_eq!(app.input.text, "");
        app.cancel_and_join().await;
    }

    /// `retry_queued` must refuse to start a new run when one is already
    /// in flight, leaving the parked front in place. The previous
    /// implementation called `start_input` unconditionally and would
    /// overwrite `self.busy`.
    #[tokio::test]
    async fn retry_queued_refuses_and_reparks_when_busy() {
        let (_dir, engine, mut app, _path) = setup();
        let dir = tempfile::tempdir().unwrap();
        let workflows_dir = dir.path().join("workflows");
        std::fs::create_dir_all(&workflows_dir).unwrap();
        let good = workflows_dir.join("good.json");
        std::fs::write(
            &good,
            r#"{
                "title": "good",
                "author": "tester",
                "steps": [{
                    "model": "openrouter:test",
                    "prompt": "hello",
                    "mcps": [],
                    "hitl": false
                }]
            }"#,
        )
        .unwrap();
        {
            let mut config = engine.config.write().await;
            config.workflows_dir = workflows_dir;
        }
        app.workflow_mode = Some("good".into());
        app.queued_inputs.push_back("parked".into());
        app.queue_blocked = Some(super::super::app::QueueBlocked {
            error: "synthetic".into(),
            pending: 1,
        });
        // Inject a busy run; retry_queued must observe it and refuse.
        let cancel = tokio_util::sync::CancellationToken::new();
        app.busy = Some(super::super::app::Busy {
            cancel: cancel.clone(),
            task: tokio::spawn(async move {
                cancel.cancelled().await;
                Ok(String::new())
            }),
        });
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.handle_key(enter, &engine).await.unwrap());
        app.submit(&engine, &_path).await.unwrap();
        // Queue state is intact.
        assert_eq!(
            app.queued_inputs.front().map(String::as_str),
            Some("parked")
        );
        assert!(app.queue_blocked.is_some());
        assert!(app
            .status
            .contains("Queue blocked; wait for the active run"));
        // The in-flight task is still running and not finished — busy
        // was never overwritten. If retry_queued had called start_input
        // it would have taken busy and spawned a new task, leaving the
        // current one to be dropped.
        let busy_after = app.busy.as_ref().expect("busy must remain Some");
        assert!(!busy_after.task.is_finished());
        app.cancel_and_join().await;
    }

    /// Workflow-complete `q` and `Esc` both exit workflow mode AND clear
    /// the parking flag, so the parked front (which references a workflow
    /// that no longer applies) is not left orphaned behind the exit.
    #[tokio::test]
    async fn workflow_complete_q_and_esc_clear_queue_blocked() {
        let (_dir, engine, mut app, _path) = setup();
        app.workflow_mode = Some("plan".into());
        app.workflow_complete = true;
        app.last_workflow_input = Some("seed".into());
        app.queued_inputs.push_back("parked-front".into());
        app.queue_blocked = Some(super::super::app::QueueBlocked {
            error: "synthetic".into(),
            pending: 1,
        });
        let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(!app.handle_key(q, &engine).await.unwrap());
        assert!(!app.workflow_complete);
        assert!(app.workflow_mode.is_none());
        assert!(app.last_workflow_input.is_none());
        assert!(
            app.queue_blocked.is_none(),
            "q on workflow-complete must clear queue_blocked"
        );
        // The parked item is preserved (not lost) so the session data
        // survives the exit; only the parking flag is discarded.
        assert_eq!(
            app.queued_inputs.front().map(String::as_str),
            Some("parked-front")
        );

        // Reset and verify Esc behaves identically. The parked front from the
        // q branch is preserved; the new push_back adds behind it.
        app.workflow_mode = Some("plan".into());
        app.workflow_complete = true;
        app.last_workflow_input = Some("seed".into());
        app.queue_blocked = Some(super::super::app::QueueBlocked {
            error: "synthetic".into(),
            pending: 1,
        });
        app.queued_inputs.push_back("parked-again".into());
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.handle_key(esc, &engine).await.unwrap());
        assert!(app.workflow_mode.is_none());
        assert!(app.queue_blocked.is_none());
        assert_eq!(
            app.queued_inputs.front().map(String::as_str),
            Some("parked-front")
        );
        assert_eq!(
            app.queued_inputs.back().map(String::as_str),
            Some("parked-again")
        );
    }

    /// `/reload` clears the parking flag along with the workflow pointer:
    /// the parked error references the old config and would be stuck
    /// against a workflow/model that may no longer exist on disk.
    #[tokio::test]
    async fn reload_clears_queue_blocked() {
        let (_dir, engine, mut app, path) = setup();
        app.workflow_mode = Some("plan".into());
        app.workflow_complete = true;
        app.last_workflow_input = Some("seed".into());
        app.queued_inputs.push_back("parked".into());
        app.queue_blocked = Some(super::super::app::QueueBlocked {
            error: "synthetic".into(),
            pending: 1,
        });
        app.command("/reload", &engine, &path).await.unwrap();
        assert!(app.queue_blocked.is_none());
        assert!(app.workflow_mode.is_none());
        assert!(!app.workflow_complete);
        assert!(app.last_workflow_input.is_none());
        // Session data survives — parked item remains in queued_inputs.
        assert_eq!(
            app.queued_inputs.front().map(String::as_str),
            Some("parked")
        );
    }

    /// The `/agent` CLI form and the agent picker both clear the parking
    /// flag the same way: leaving workflow mode means the parked front
    /// (which references the old workflow) cannot be retried against
    /// the new agent.
    #[tokio::test]
    async fn agent_command_and_picker_clear_queue_blocked() {
        let (_dir, engine, mut app, path) = setup();
        {
            let mut config = engine.config.write().await;
            config.agents.insert(
                "alpha".into(),
                serde_json::from_value(serde_json::json!({"hidden":false})).unwrap(),
            );
        }
        app.workflow_mode = Some("plan".into());
        app.workflow_complete = true;
        app.last_workflow_input = Some("seed".into());
        app.queue_blocked = Some(super::super::app::QueueBlocked {
            error: "synthetic".into(),
            pending: 1,
        });
        app.queued_inputs.push_back("parked".into());
        // CLI form. /agent alpha drops the workflow pointer and the
        // parking flag together.
        app.command("/agent alpha", &engine, &path).await.unwrap();
        assert!(app.queue_blocked.is_none());
        assert!(app.workflow_mode.is_none());
        assert_eq!(app.selection.agent.as_deref(), Some("alpha"));
        assert_eq!(
            app.queued_inputs.front().map(String::as_str),
            Some("parked")
        );

        // Picker form: re-enter workflow mode with a parking flag, then
        // open the picker. The agent picker's apply path clears the
        // parking flag along with the workflow pointer. We deliberately
        // do NOT raise the workflow-complete overlay so the Enter key
        // reaches the picker (the overlay is modal and would otherwise
        // intercept it).
        app.workflow_mode = Some("plan".into());
        app.workflow_complete = false;
        app.last_workflow_input = Some("seed".into());
        app.queue_blocked = Some(super::super::app::QueueBlocked {
            error: "synthetic".into(),
            pending: 1,
        });
        app.command("/agent", &engine, &path).await.unwrap();
        app.paste("alpha");
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(!app.handle_key(enter, &engine).await.unwrap());
        assert!(app.queue_blocked.is_none());
        assert!(app.workflow_mode.is_none());
        assert_eq!(app.selection.agent.as_deref(), Some("alpha"));
    }

    /// Workflow-complete overlay accepts PgUp/PgDn/Home/End to scroll the
    /// body without disturbing the transcript. The home/end keys reset
    /// the overlay offset to the visible window.
    #[tokio::test]
    async fn workflow_complete_overlay_accepts_overlay_scroll_keys() {
        let (_dir, engine, mut app, _path) = setup();
        app.workflow_mode = Some("plan".into());
        app.workflow_complete = true;
        app.queue_blocked = Some(super::super::app::QueueBlocked {
            error: "very long synthetic error that spans many lines so the overlay must scroll to show it all\nmore\nmore\nmore".into(),
            pending: 1,
        });
        let transcript_scroll_before = app.scroll;
        let pgdn = KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE);
        assert!(!app.handle_key(pgdn, &engine).await.unwrap());
        assert_eq!(app.overlay_scroll, 10);
        let pgup = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);
        assert!(!app.handle_key(pgup, &engine).await.unwrap());
        assert_eq!(app.overlay_scroll, 0);
        assert_eq!(app.scroll, transcript_scroll_before);
        // Home/End pin to start/max; the render path clamps to the
        // visible window so max sentinel is fine.
        app.overlay_scroll = 5;
        let end = KeyEvent::new(KeyCode::End, KeyModifiers::NONE);
        assert!(!app.handle_key(end, &engine).await.unwrap());
        assert_eq!(app.overlay_scroll, usize::MAX);
        let home = KeyEvent::new(KeyCode::Home, KeyModifiers::NONE);
        assert!(!app.handle_key(home, &engine).await.unwrap());
        assert_eq!(app.overlay_scroll, 0);
    }

    /// Mouse wheel events while the workflow-complete overlay is shown
    /// route to `overlay_scroll` instead of moving the hidden transcript
    /// behind the overlay.
    #[tokio::test]
    async fn mouse_wheel_scrolls_workflow_complete_overlay_not_transcript() {
        let (_dir, _engine, mut app, _path) = setup();
        app.workflow_mode = Some("plan".into());
        app.workflow_complete = true;
        let scroll_before = app.scroll;
        // ScrollUp is the "back/up" wheel direction and grows the
        // overlay offset, matching the existing approval/help overlay
        // routing.
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.overlay_scroll, 3);
        assert_eq!(app.scroll, scroll_before);
        // ScrollDown returns toward the top of the overlay body.
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.overlay_scroll, 0);
        // Non-wheel events are ignored.
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.overlay_scroll, 0);
        assert_eq!(app.scroll, scroll_before);
    }

    /// When the spawned busy task panics or is aborted, `finish_run`
    /// receives a JoinError and must update the status away from the
    /// stale "Running | Ctrl+C to cancel" line.
    #[tokio::test]
    async fn join_error_sets_a_sane_status_instead_of_leaving_running_stale() {
        let (_dir, engine, mut app, _path) = setup();
        // Spawn a task that panics immediately so the JoinError variant
        // is exercised (not cancellation).
        let task = tokio::spawn(async {
            panic!("synthetic panic for join-error coverage");
        });
        let cancel = tokio_util::sync::CancellationToken::new();
        app.busy = Some(super::super::app::Busy { cancel, task });
        app.status = "Running | Ctrl+C to cancel".into();
        tokio::task::yield_now().await;
        let (_events_tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        drop(_events_tx);
        assert!(app.finish_run(&engine, &mut rx).await);
        assert!(
            !app.status.contains("Running"),
            "status must not lie about an active run: {}",
            app.status
        );
        assert!(
            app.status.contains("Run failed"),
            "status must surface the failure: {}",
            app.status
        );
        assert!(app.busy.is_none());
    }

    /// `UiEvent::Activity(Start)` is the lifecycle producer wired through the
    /// activity spine. Wave 1 records the node and timeline row but does not
    /// render it: this test asserts the spine contract end-to-end (no chat
    /// entry, but a timeline row + activity node, leaving the rest of the TUI
    /// state — status, input, scroll, overlay — untouched so the renderer and
    /// commands layers remain free to consume the spine later).
    #[tokio::test]
    async fn ui_event_activity_records_into_the_spine_without_touching_chat_state() {
        use crate::model::{ActivityEvent, ActivityKind, ActivityPhase, UiEvent};
        let (_dir, _engine, mut app, _path) = setup();
        let entries_before = app.entries.len();
        let status_before = app.status.clone();
        let input_before = app.input.text.clone();
        let scroll_before = app.scroll;
        let overlay_before = app.overlay_scroll;
        app.event(UiEvent::Activity(ActivityEvent {
            id: "act-1".into(),
            parent_id: None,
            context: "main".into(),
            kind: ActivityKind::Tool,
            phase: ActivityPhase::Start,
            title: "web_search".into(),
            external_id: Some("req-1".into()),
            status: None,
        }));
        // Chat transcript is untouched.
        assert_eq!(app.entries.len(), entries_before);
        assert_eq!(app.status, status_before);
        assert_eq!(app.input.text, input_before);
        assert_eq!(app.scroll, scroll_before);
        assert_eq!(app.overlay_scroll, overlay_before);
        // Spine contract: one timeline row, one activity node.
        assert_eq!(app.timeline.len(), 1);
        assert!(
            matches!(&app.timeline[0], super::super::app::TimelineItem::Activity(id) if id == "act-1")
        );
        let node = app.activity("act-1").expect("activity node inserted");
        assert_eq!(node.start.kind, ActivityKind::Tool);
        assert!(!node.expanded);
    }

    /// When q/Esc on the workflow-complete overlay clears the parking
    /// flag, no orphan busy handle or workflow pointer is left behind.
    /// The next finish_run tick observes `busy == None` and
    /// `queue_blocked == None` so the drain loop does not retry a
    /// vanished workflow.
    #[tokio::test]
    async fn workflow_complete_exit_leaves_no_orphaned_handles() {
        let (_dir, engine, mut app, _path) = setup();
        fake_busy(&mut app);
        app.workflow_mode = Some("plan".into());
        app.workflow_complete = true;
        app.last_workflow_input = Some("seed".into());
        app.queued_inputs.push_back("parked".into());
        app.queue_blocked = Some(super::super::app::QueueBlocked {
            error: "synthetic".into(),
            pending: 1,
        });
        let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(!app.handle_key(q, &engine).await.unwrap());
        // No orphan busy handle was leaked through the exit; the original
        // busy task is still the only one we own and can be cancelled.
        assert!(app.busy.is_some());
        assert!(app.queue_blocked.is_none());
        assert!(app.workflow_mode.is_none());
        assert!(!app.workflow_complete);
        assert!(app.last_workflow_input.is_none());
        // Cancelling the original busy task drains cleanly.
        app.cancel_and_join().await;
        assert!(app.busy.is_none());
    }

    #[tokio::test]
    async fn alternate_enter_and_ctrl_j_insert_newlines_without_submitting() {
        let (_dir, engine, mut app, _path) = setup();

        let alt_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        assert!(!app.handle_key(alt_enter, &engine).await.unwrap());
        let ctrl_j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL);
        assert!(!app.handle_key(ctrl_j, &engine).await.unwrap());

        assert_eq!(app.input.text, "\n\n");
        assert!(!app.quit);
    }

    #[tokio::test]
    async fn ctrl_d_quits_only_with_empty_input_and_preserves_nonempty_text() {
        let (_dir, engine, mut app, _path) = setup();
        let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);

        app.input.set("retain this draft".into());
        assert!(!app.handle_key(ctrl_d, &engine).await.unwrap());
        assert!(!app.quit);
        assert_eq!(app.input.text, "retain this draft");

        app.input.set(String::new());
        assert!(!app.handle_key(ctrl_d, &engine).await.unwrap());
        assert!(app.quit);
    }

    #[tokio::test]
    async fn conversation_scroll_keys_work_without_activity_focus_and_do_not_change_history() {
        let (_dir, engine, mut app, _path) = setup();
        app.input_history = vec!["older request".into(), "newer request".into()];
        app.history_index = app.input_history.len();
        app.input.set("draft".into());
        assert_eq!(app.input.text, "draft");

        let page_up = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);
        let page_down = KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE);
        let ctrl_home = KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL);
        let ctrl_end = KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL);
        assert!(!app.handle_key(page_up, &engine).await.unwrap());
        assert_eq!(app.scroll, 10);
        assert!(!app.handle_key(page_down, &engine).await.unwrap());
        assert_eq!(app.scroll, 0);
        assert!(!app.handle_key(ctrl_home, &engine).await.unwrap());
        assert_eq!(app.scroll, usize::MAX);
        assert!(!app.handle_key(ctrl_end, &engine).await.unwrap());
        assert_eq!(app.scroll, 0);
        assert_eq!(app.history_index, app.input_history.len());
        assert_eq!(app.input.text, "draft");
    }

    #[tokio::test]
    async fn slash_commands_are_available_mid_run_without_entering_the_queue() {
        let (_dir, engine, mut app, path) = setup();
        fake_busy(&mut app);

        for command in ["/tools", "/mcp", "/theme", "/help", "/cost"] {
            app.input.set(command.into());
            app.submit(&engine, &path).await.unwrap();
            assert!(app.queued_inputs.is_empty(), "{command} must not be queued");
            assert!(app.busy.is_some(), "{command} must not stop the run");
        }

        app.cancel_and_join().await;
    }

    #[tokio::test]
    async fn clear_and_new_drop_queued_messages() {
        let (_dir, engine, mut app, path) = setup();
        app.queued_inputs.push_back("first pending".into());
        app.queued_inputs.push_back("second pending".into());
        app.command("/clear", &engine, &path).await.unwrap();
        assert!(app.queued_inputs.is_empty());

        app.queued_inputs.push_back("third pending".into());
        app.command("/new", &engine, &path).await.unwrap();
        assert!(app.queued_inputs.is_empty());
    }
}
