//! Slash commands are deliberately plain text plus JSON for structured additions.
//! Each handler either changes local UI state or calls one well-defined service.
use super::app::App;
use crate::{
    config::{store, Config, Effort},
    engine::{Engine, Selection},
    session::Session,
    skills, workflow,
};
use anyhow::{bail, Context, Result};
use std::path::Path;

pub(super) const NAMES: &[&str] = &[
    "/help",
    "/model",
    "/agent",
    "/mode",
    "/workflow",
    "/tools",
    "/mcp",
    "/skills",
    "/install-skill",
    "/effort",
    "/export",
    "/cost",
    "/clear",
    "/new",
    "/reload",
    "/quit",
    ":q",
];
pub(super) const HELP: &str = r#"Commands
/mode [name|default]        Show or change application mode
/agent [name|default] [mode] Select an agent and its optional mode
/model [alias|provider:id]  Show or switch the active model
/model add <alias> <JSON>   Add a model to config and select it
/model add <alias> <ref>    Add an alias for provider:model-id
/effort [level|default]    Show or change supported reasoning effort
/mcp [name on|off|restart] List, activate, deactivate, or restart MCP
/mcp add <name> <JSON>     Add a server; generate UUID if omitted
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
Ctrl+Home/End: history top/bottom | Tab: command completion
Ctrl+C: cancel | Ctrl+D: quit with empty input
Approvals: y approve, n reject, r retry, s skip, q abort
Esc: close help or reject approval

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
            self.note(format!("Current: {}\nAliases: {}\n/model <reference> or /model add <alias> <JSON|reference>",self.model_label,config.models.keys().cloned().collect::<Vec<_>>().join(", ")));
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
            config.resolve_model(rest)?;
            self.selection.model = Some(rest.into());
            self.selection.effort = None;
        }
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
            self.note(
                config
                    .mcp_servers
                    .iter()
                    .map(|(name, server)| {
                        format!(
                            "{} {name} ({})",
                            state(switches.mcp_enabled(name, &config)),
                            server.uuid
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
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
            self.note(format!(
                "Agents: {}",
                engine
                    .config
                    .read()
                    .await
                    .agents
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            return Ok(());
        }
        let (name, mode) = split_head(rest);
        let selection = Selection {
            agent: (name != "default").then(|| name.into()),
            agent_mode: (!mode.is_empty()).then(|| mode.into()),
            ..Selection::default()
        };
        engine.scope(&selection, "main", None).await?;
        self.selection = selection;
        self.mode = None;
        self.workflow_mode = None;
        Ok(())
    }

    async fn mode_command(&mut self, rest: &str, engine: &Engine) -> Result<()> {
        let config = engine.config.read().await;
        if rest.is_empty() {
            self.note(format!(
                "Modes: {}",
                config.modes.keys().cloned().collect::<Vec<_>>().join(", ")
            ));
        } else if rest == "default" {
            self.mode = None;
            self.workflow_mode = None;
            self.selection = Selection::default();
        } else {
            let mode = config.modes.get(rest).context("Unknown mode")?;
            self.selection = Selection {
                agent: mode.agent.clone(),
                agent_mode: mode.agent_mode.clone(),
                ..Selection::default()
            };
            self.workflow_mode = mode.workflow.clone();
            self.mode = Some(rest.into());
        }
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
    use crate::model::{Message, Usage};

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
        assert_eq!(json["models"]["thinker"]["reasoning"]["effort"], "low");
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
}
