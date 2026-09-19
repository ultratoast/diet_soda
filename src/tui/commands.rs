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
PageUp/PageDown: scroll history or dialogs
Ctrl+Home/End: history top/bottom | Tab/Shift+Tab: next/previous agent
Ctrl+C: cancel | Ctrl+D: quit with empty input
Approvals: y approve, n reject, r retry, s skip, q abort
Pickers: type to fuzzy-filter, Up/Down browse, Enter select/toggle, Esc close
Esc: close dialogs or reject approval

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
        let (name, _) = split_head(rest);
        let selection = Selection {
            agent: (name != "default").then(|| name.into()),
            agent_mode: None,
            ..Selection::default()
        };
        engine.scope(&selection, "main", None).await?;
        self.selection = selection;
        self.mode = None;
        self.workflow_mode = None;
        self.note(format!("Switched agent to {name}"));
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
        self.require_idle()?;
        // Match the /agent picker: every configured agent, hidden or not, so a
        // config with one visible agent still lets Tab reach the others. The
        // bare "default" scope is offered only when no agent is marked default;
        // otherwise it would duplicate that agent under a second name.
        let (agents, default_agent): (Vec<String>, Option<String>) = {
            let config = engine.config.read().await;
            let default_agent = config.default_agent_name();
            let agents = std::iter::once("default".to_owned())
                .filter(|_| default_agent.is_none())
                .chain(config.agents.keys().cloned())
                .collect();
            (agents, default_agent)
        };
        if agents.len() < 2 {
            self.status = "Only one agent is configured".into();
            return Ok(());
        }
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
        // agent_command records the "Switched agent" note; do not add a second one.
        self.agent_command(&selected, engine).await?;
        self.refresh_model(engine).await?;
        self.status = format!("agent: {selected} | Tab / Shift+Tab to cycle");
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
        self.entries.clear();
        self.streams.clear();
        self.input = Default::default();
        self.input_history.clear();
        self.history_index = 0;
        self.spend = Default::default();
        self.scroll = 0;
        self.history_generation += 1;
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
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

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
        assert!(app.command("/effort low", &engine, &path).await.is_err());
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
}
