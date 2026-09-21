//! UI state and user intent. Provider/tool orchestration stays in Engine.
use super::{
    picker::{Picker, PickerAction, PickerKind},
    Input,
};
use crate::{
    config::{Config, Theme},
    engine::{Engine, Selection},
    model::{Decision, Message, Spend, UiEvent},
    tools,
    workflow::{self, Workflow},
};
use anyhow::{bail, Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use std::{
    collections::{BTreeMap, VecDeque},
    path::Path,
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

pub(super) struct Entry {
    pub role: String,
    pub context: String,
    pub text: String,
    pub revision: u64,
}
pub(super) struct Approval {
    pub title: String,
    pub detail: String,
    pub workflow: bool,
    pub reply: oneshot::Sender<Decision>,
}
pub(super) struct Busy {
    pub(super) task: JoinHandle<Result<String>>,
    pub(super) cancel: CancellationToken,
}

pub(super) struct App {
    pub entries: Vec<Entry>,
    pub streams: BTreeMap<String, usize>,
    pub input: Input,
    pub input_history: Vec<String>,
    pub history_index: usize,
    pub spend: Spend,
    pub context_limit: u32,
    pub context_tokens: u64,
    pub workspace: String,
    pub queued_inputs: VecDeque<String>,
    pub theme: Theme,
    pub selection: Selection,
    pub mode: Option<String>,
    pub workflow_mode: Option<String>,
    pub workflow_complete: bool,
    pub last_workflow_input: Option<String>,
    pub status: String,
    pub model_label: String,
    pub effort_label: String,
    pub scroll: usize,
    pub overlay_scroll: usize,
    pub help: bool,
    pub picker: Option<Picker>,
    pub approval: Option<Approval>,
    pub busy: Option<Busy>,
    pub quit: bool,
    /// Reset invalidates cached entries even if the new conversation has the same length.
    pub history_generation: u64,
}

impl App {
    pub fn new(config: &Config, selection: Selection) -> Self {
        let mut selection = selection;
        if selection.agent.is_none() {
            selection.agent = config.default_agent_name();
        }
        Self {
            entries: vec![],
            streams: BTreeMap::new(),
            input: Input::default(),
            input_history: vec![],
            history_index: 0,
            spend: Spend::default(),
            context_limit: 0,
            context_tokens: 0,
            workspace: config.workspace.display().to_string(),
            queued_inputs: VecDeque::new(),
            theme: config.theme.clone(),
            selection,
            mode: None,
            workflow_mode: None,
            workflow_complete: false,
            last_workflow_input: None,
            status: "Ready".into(),
            model_label: format!("{}:{}", config.model.provider, config.model.model),
            effort_label: "default".into(),
            scroll: 0,
            overlay_scroll: 0,
            help: false,
            picker: None,
            approval: None,
            busy: None,
            quit: false,
            history_generation: 0,
        }
    }
    pub fn note(&mut self, text: impl Into<String>) {
        self.push("status", "main", text.into());
    }
    pub fn error(&mut self, text: impl Into<String>) {
        self.push("error", "main", text.into());
    }
    fn push(&mut self, role: &str, context: &str, text: String) -> usize {
        self.entries.push(Entry {
            role: role.into(),
            context: context.into(),
            text,
            revision: 0,
        });
        self.entries.len() - 1
    }
    pub fn message(&mut self, context: String, message: Message) {
        let mut text = message.content;
        for call in message.tool_calls {
            // Show what the agent is doing, not a raw JSON blob; unknown tools
            // fall back to pretty-printed arguments inside describe_call.
            let arguments = serde_json::from_str::<serde_json::Value>(&call.arguments)
                .unwrap_or_else(|_| serde_json::Value::String(call.arguments.clone()));
            text.push_str(&format!(
                "\n\nTool: {}\n{}",
                call.name,
                tools::describe_call(&call.name, &arguments)
            ));
        }
        if message.role == "assistant" {
            if let Some(index) = self.streams.remove(&context) {
                self.entries[index].text = text;
                self.entries[index].revision += 1;
                return;
            }
        }
        if message.role == "assistant" {
            self.scroll = 0;
        }
        self.push(&message.role, &context, text);
    }
    pub fn event(&mut self, event: UiEvent) {
        match event {
            UiEvent::Model {
                context,
                provider,
                model,
                effort,
            } => {
                self.model_label = format!("{provider}:{model}");
                self.effort_label = effort
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "default".into());
                self.status = format!("Generating | {context}");
            }
            UiEvent::Delta { context, text } => {
                let index = match self.streams.get(&context) {
                    Some(index) => *index,
                    None => {
                        self.scroll = 0;
                        let index = self.push("assistant", &context, String::new());
                        self.streams.insert(context, index);
                        index
                    }
                };
                self.entries[index].text.push_str(&text);
                self.entries[index].revision += 1;
            }
            UiEvent::Message { context, message } => self.message(context, message),
            UiEvent::Status(status) => self.status = status,
            UiEvent::Spend(spend) => self.spend = spend,
            UiEvent::Context { context, tokens } => {
                if context == "main" {
                    self.context_tokens = tokens;
                }
            }
            UiEvent::Approval {
                title,
                detail,
                workflow,
                reply,
            } => {
                self.overlay_scroll = 0;
                if !reply.is_closed() {
                    self.approval = Some(Approval {
                        title,
                        detail,
                        workflow,
                        reply,
                    });
                }
            }
        }
    }
    pub async fn refresh_model(&mut self, engine: &Engine) -> Result<()> {
        let scope = engine.scope(&self.selection, "main", None).await?;
        self.model_label = format!("{}:{}", scope.model.provider, scope.model.model);
        self.effort_label = scope
            .model
            .reasoning
            .and_then(|r| r.effort)
            .map(|e| e.to_string())
            .unwrap_or_else(|| "default".into());
        self.context_limit = scope.model.max_tokens;
        self.workspace = engine.config.read().await.workspace.display().to_string();
        Ok(())
    }
    pub fn require_idle(&self) -> Result<()> {
        if self.busy.is_some() {
            bail!("Wait for the active run or cancel it with Ctrl+C");
        }
        Ok(())
    }

    pub async fn submit(&mut self, engine: &Engine, config_path: &Path) -> Result<()> {
        let text = self.input.take();
        let text = text.trim().to_owned();
        if text.is_empty() {
            return Ok(());
        }
        self.input_history.push(text.clone());
        self.history_index = self.input_history.len();
        self.scroll = 0;
        if text.starts_with('/') || text == ":q" {
            return self.command(&text, engine, config_path).await;
        }
        if self.busy.is_some() {
            self.queued_inputs.push_back(text);
            self.status = format!("Queued {} message(s)", self.queued_inputs.len());
            return Ok(());
        }
        self.start_input(engine, text).await?;
        self.status = "Running | Ctrl+C to cancel".into();
        Ok(())
    }

    async fn start_input(&mut self, engine: &Engine, text: String) -> Result<()> {
        if let Some(path) = self.workflow_mode.clone() {
            self.start_workflow(engine, &path, text).await?;
        } else {
            let engine = engine.clone();
            let selection = self.selection.clone();
            let cancel = CancellationToken::new();
            let token = cancel.clone();
            self.busy = Some(Busy {
                cancel,
                task: tokio::spawn(async move { engine.turn(text, selection, token).await }),
            });
        }
        Ok(())
    }
    pub async fn start_workflow(
        &mut self,
        engine: &Engine,
        name: &str,
        input: String,
    ) -> Result<()> {
        self.require_idle()?;
        let config = engine.config.read().await.clone();
        let workflow = Workflow::load(&workflow::workflow_path(name, &config), &config)?;
        let engine = engine.clone();
        let selection = self.selection.clone();
        let workflow_input = input.clone();
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        self.busy = Some(Busy {
            cancel,
            task: tokio::spawn(async move {
                workflow::run(&engine, workflow, input, selection, token).await
            }),
        });
        self.last_workflow_input = Some(workflow_input);
        self.workflow_mode = Some(name.to_owned());
        self.workflow_complete = false;
        Ok(())
    }
    pub async fn finish_run(
        &mut self,
        engine: &Engine,
        events: &mut mpsc::UnboundedReceiver<UiEvent>,
    ) -> bool {
        if !self.busy.as_ref().is_some_and(|b| b.task.is_finished()) {
            return false;
        }
        let busy = self.busy.take().unwrap();
        while let Ok(event) = events.try_recv() {
            self.event(event);
        }
        match busy.task.await {
            Ok(Ok(_)) => {
                if self.workflow_mode.is_some() {
                    self.workflow_complete = true;
                    self.status = "Workflow complete | n new | r repeat | q exit workflow".into();
                } else {
                    self.status = "Ready".into();
                }
            }
            Ok(Err(e)) => {
                self.error(format!("{e:#}"));
                self.status = "Run ended".into();
            }
            Err(e) => self.error(format!("Run failed: {e}")),
        }
        self.streams.clear();
        self.approval = None;
        if let Err(e) = self.refresh_model(engine).await {
            self.error(e.to_string());
        }
        if let Some(input) = self.queued_inputs.pop_front() {
            if let Err(error) = self.start_input(engine, input).await {
                self.error(format!("{error:#}"));
            } else {
                self.status = format!(
                    "Running | {} message(s) queued | Ctrl+C to cancel",
                    self.queued_inputs.len()
                );
            }
        }
        true
    }
    pub async fn cancel_and_join(&mut self) {
        if let Some(mut busy) = self.busy.take() {
            busy.cancel.cancel();
            self.approval = None;
            if tokio::time::timeout(Duration::from_secs(5), &mut busy.task)
                .await
                .is_err()
            {
                busy.task.abort();
                let _ = busy.task.await;
            }
        }
    }

    /// Modal input takes priority over chat editing and application shortcuts.
    /// Return true only when the chat input should be submitted by the loop.
    pub async fn handle_key(&mut self, key: KeyEvent, engine: &Engine) -> Result<bool> {
        if key.kind == KeyEventKind::Release && !key.modifiers.contains(KeyModifiers::SHIFT) {
            return Ok(false);
        }
        if self.busy.is_some()
            && matches!(key.code, KeyCode::Esc | KeyCode::Char('c'))
            && (key.code == KeyCode::Esc || key.modifiers.contains(KeyModifiers::CONTROL))
        {
            self.cancel_active_run();
            return Ok(false);
        }
        if self.workflow_complete {
            match key.code {
                KeyCode::Char('n') => {
                    self.workflow_complete = false;
                    self.input = Input::default();
                    self.status = "Enter input for a new workflow run".into();
                }
                KeyCode::Char('r') => {
                    let workflow = self
                        .workflow_mode
                        .clone()
                        .context("Workflow is not selected")?;
                    let input = self.last_workflow_input.clone().unwrap_or_default();
                    self.start_workflow(engine, &workflow, input).await?;
                }
                KeyCode::Char('q') | KeyCode::Esc => {
                    self.workflow_complete = false;
                    self.workflow_mode = None;
                    self.status = "Exited workflow mode".into();
                }
                _ => {}
            }
            return Ok(false);
        }
        if self.approval.is_none() && !self.help {
            if let Some(picker) = &mut self.picker {
                match picker.key(key) {
                    PickerAction::Close => {
                        if let PickerKind::Themes { original, .. } = &picker.kind {
                            self.theme = *original.clone();
                        }
                        self.picker = None;
                    }
                    PickerAction::Select(reference) => match picker.kind {
                        PickerKind::Models => {
                            self.select_model(&reference, engine).await?;
                            self.refresh_model(engine).await?;
                            self.picker = None;
                            self.status = format!("Selected {}", self.model_label);
                        }
                        PickerKind::Agents => {
                            let name = if reference == "default" {
                                None
                            } else {
                                Some(reference.clone())
                            };
                            let selection = Selection {
                                agent: name,
                                agent_mode: None,
                                ..Selection::default()
                            };
                            engine.scope(&selection, "main", None).await?;
                            self.selection = selection;
                            self.mode = None;
                            self.workflow_mode = None;
                            self.refresh_model(engine).await?;
                            self.note(format!("Switched agent to {reference}"));
                            self.picker = None;
                            self.status = format!("Selected agent {reference}");
                        }
                        PickerKind::Mcps => {
                            let config = engine.config.read().await;
                            let mut switches = engine.switches.write().await;
                            let enabled = !switches.mcp_enabled(&reference, &config);
                            switches.mcps.insert(reference.clone(), enabled);
                            if let Some(choice) =
                                picker.choices.iter_mut().find(|c| c.reference == reference)
                            {
                                choice.enabled = Some(enabled);
                            }
                            self.status =
                                format!("MCP {reference}: {}", if enabled { "on" } else { "off" });
                        }
                        PickerKind::Themes { .. } => {
                            if let Some(theme) = picker.preview_theme() {
                                self.theme = theme.clone();
                            }
                            self.picker = None;
                            self.status = format!("Theme: {reference}");
                        }
                    },
                    PickerAction::None => {
                        if let Some(theme) = picker.preview_theme() {
                            self.theme = theme.clone();
                        }
                    }
                }
                return Ok(false);
            }
            if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
                let reverse =
                    key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT);
                self.cycle_agent(engine, reverse).await?;
                return Ok(false);
            }
        }
        Ok(self.edit_key(key))
    }

    /// Mouse-wheel scrolling is reserved for the conversation, never input history.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        let delta = match mouse.kind {
            MouseEventKind::ScrollUp => 3,
            MouseEventKind::ScrollDown => -3,
            _ => return,
        };
        if self.approval.is_some() || self.help {
            if delta > 0 {
                self.overlay_scroll = self.overlay_scroll.saturating_add(delta as usize);
            } else {
                self.overlay_scroll = self.overlay_scroll.saturating_sub((-delta) as usize);
            }
        } else if delta > 0 {
            self.scroll = self.scroll.saturating_add(delta as usize);
        } else {
            self.scroll = self.scroll.saturating_sub((-delta) as usize);
        }
    }

    fn cancel_active_run(&mut self) {
        if let Some(busy) = &self.busy {
            busy.cancel.cancel();
            self.status = "Cancelling...".into();
        }
        if let Some(approval) = self.approval.take() {
            let _ = approval.reply.send(Decision::Abort);
        }
    }

    pub fn paste(&mut self, text: &str) {
        if self.approval.is_some() || self.help {
            return;
        }
        if let Some(picker) = &mut self.picker {
            picker.paste(text);
            if let Some(theme) = picker.preview_theme() {
                self.theme = theme.clone();
            }
        } else {
            self.input.insert(&text.replace('\r', "\n"));
        }
    }

    fn edit_key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if control && key.code == KeyCode::Char('c') {
            self.cancel_active_run();
            return false;
        }
        if self.approval.is_some() || self.help {
            match key.code {
                KeyCode::PageDown => {
                    self.overlay_scroll = self.overlay_scroll.saturating_add(10);
                    return false;
                }
                KeyCode::PageUp => {
                    self.overlay_scroll = self.overlay_scroll.saturating_sub(10);
                    return false;
                }
                KeyCode::Home => {
                    self.overlay_scroll = 0;
                    return false;
                }
                KeyCode::End => {
                    self.overlay_scroll = usize::MAX;
                    return false;
                }
                _ => {}
            }
        }
        if let Some(approval) = &self.approval {
            let decision = match key.code {
                KeyCode::Char('y') => Some(Decision::Approve),
                KeyCode::Char('n') | KeyCode::Esc => Some(Decision::Reject),
                KeyCode::Char('q') => Some(Decision::Abort),
                KeyCode::Char('r') if approval.workflow => Some(Decision::Retry),
                KeyCode::Char('s') if approval.workflow => Some(Decision::Skip),
                _ => None,
            };
            if let Some(decision) = decision {
                if decision == Decision::Abort {
                    if let Some(busy) = &self.busy {
                        busy.cancel.cancel();
                    }
                }
                let _ = self.approval.take().unwrap().reply.send(decision);
            }
            return false;
        }
        if self.help {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::F(1)) {
                self.help = false;
            }
            return false;
        }
        match key.code {
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
            {
                self.input.insert("\n")
            }
            KeyCode::Enter => return true,
            KeyCode::Char('j') if control => self.input.insert("\n"),
            KeyCode::Char('d') if control && self.input.text.is_empty() => self.quit = true,
            KeyCode::Char('u') if control => {
                self.input.take();
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.input.insert(&c.to_string())
            }
            KeyCode::Backspace if control => self.input.delete_word_backward(),
            KeyCode::Char('h') if control => self.input.delete_word_backward(),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left if control => self.input.word_left(),
            KeyCode::Right if control => self.input.word_right(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home if control => self.scroll = usize::MAX,
            KeyCode::End if control => self.scroll = 0,
            KeyCode::Home => {
                self.input.cursor = self.input.text[..self.input.cursor]
                    .rfind('\n')
                    .map(|i| i + 1)
                    .unwrap_or(0)
            }
            KeyCode::End => {
                self.input.cursor += self.input.text[self.input.cursor..]
                    .find('\n')
                    .unwrap_or(self.input.text.len() - self.input.cursor)
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Up if self.history_index > 0 => {
                self.history_index -= 1;
                self.input
                    .set(self.input_history[self.history_index].clone());
            }
            KeyCode::Down => {
                self.history_index = (self.history_index + 1).min(self.input_history.len());
                self.input.set(
                    self.input_history
                        .get(self.history_index)
                        .cloned()
                        .unwrap_or_default(),
                );
            }
            KeyCode::F(1) => {
                self.help = true;
                self.overlay_scroll = 0;
            }
            _ => {}
        }
        false
    }
}
