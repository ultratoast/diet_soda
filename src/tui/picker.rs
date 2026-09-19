//! Shared searchable model, MCP, and theme dialogs. Catalog tasks are cancelled when the dialog closes;
//! search keys are cached and matching runs only after input/catalog changes.
use super::Input;
use crate::{
    config::{themes, Config, ModelConfig, Theme},
    engine::Engine,
    provider::CatalogModel,
    tools::Switches,
};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::BTreeSet;
use tokio::task::JoinSet;

pub(super) struct Choice {
    pub reference: String,
    pub label: String,
    search: String,
    configured: bool,
    pub enabled: Option<bool>,
}

pub(super) enum PickerKind {
    Models,
    Mcps,
    Themes {
        original: Box<Theme>,
        choices: Vec<(String, Theme)>,
    },
}

pub(super) enum PickerAction {
    None,
    Select(String),
    Close,
}

pub(super) struct Picker {
    pub kind: PickerKind,
    pub query: Input,
    pub choices: Vec<Choice>,
    pub matches: Vec<usize>,
    pub selected: usize,
    pub errors: Vec<String>,
    known: BTreeSet<String>,
    pending: JoinSet<(String, Result<Vec<CatalogModel>>)>,
}

impl Picker {
    fn empty(kind: PickerKind) -> Self {
        Self {
            kind,
            query: Input::default(),
            choices: vec![],
            matches: vec![],
            selected: 0,
            errors: vec![],
            known: BTreeSet::new(),
            pending: JoinSet::new(),
        }
    }

    pub fn models(config: &Config, current: &ModelConfig, reference: Option<&str>) -> Self {
        let mut picker = Self::empty(PickerKind::Models);
        for (alias, model) in &config.models {
            picker.add_configured(alias, model);
        }
        let default = format!("{}:{}", config.model.provider, config.model.model);
        picker.add_configured(&default, &config.model);
        let current_id = format!("{}:{}", current.provider, current.model);
        let reference = reference.unwrap_or(&current_id);
        picker.add_configured(reference, current);
        picker.filter(Some(reference));
        picker
    }

    pub fn mcps(config: &Config, switches: &Switches) -> Self {
        let mut picker = Self::empty(PickerKind::Mcps);
        for (name, server) in &config.mcp_servers {
            picker.add(name.clone(), format!("{name} | {}", server.uuid), true);
            picker.choices.last_mut().unwrap().enabled = Some(switches.mcp_enabled(name, config));
        }
        picker.filter(None);
        picker
    }

    pub fn themes(current: &Theme, configured: &Theme) -> Self {
        let mut choices = vec![("configured".into(), configured.clone())];
        for (name, _) in themes::PRESETS {
            let mut theme = themes::preset(name).unwrap();
            theme.ascii = current.ascii;
            theme.syntax_highlighting = current.syntax_highlighting;
            choices.push((name.to_string(), theme));
        }
        let selected = choices
            .iter()
            .find(|(_, theme)| theme == current)
            .map(|(name, _)| name.clone());
        let mut picker = Self::empty(PickerKind::Themes {
            original: Box::new(current.clone()),
            choices,
        });
        picker.add(
            "configured".into(),
            "configured | Theme from config.json".into(),
            true,
        );
        for (name, description) in themes::PRESETS {
            picker.add(name.to_string(), format!("{name} | {description}"), true);
        }
        picker.filter(selected.as_deref());
        picker
    }

    pub fn preview_theme(&self) -> Option<&Theme> {
        if let PickerKind::Themes { choices, .. } = &self.kind {
            let reference = &self.current()?.reference;
            choices
                .iter()
                .find(|(name, _)| name == reference)
                .map(|(_, theme)| theme)
        } else {
            None
        }
    }

    pub fn title(&self) -> &'static str {
        match self.kind {
            PickerKind::Models => "Models",
            PickerKind::Mcps => "MCP servers",
            PickerKind::Themes { .. } => "Themes",
        }
    }

    pub fn hint(&self) -> String {
        match self.kind {
            PickerKind::Models if self.loading() => "Loading provider model lists...".into(),
            PickerKind::Models if !self.errors.is_empty() => {
                format!("Some catalogs unavailable: {}", self.errors.join("; "))
            }
            PickerKind::Models => {
                "Type to fuzzy-filter by alias, provider, model ID or name".into()
            }
            PickerKind::Mcps if self.choices.is_empty() => {
                "No configured servers; add one with /mcp add <name> <JSON>".into()
            }
            PickerKind::Mcps => {
                "Enter toggles immediately; connects lazily. Esc closes; changes remain.".into()
            }
            PickerKind::Themes { .. } => {
                "Live preview | Enter applies for this session | Esc restores previous theme".into()
            }
        }
    }

    fn add_configured(&mut self, reference: &str, model: &ModelConfig) {
        let id = format!("{}:{}", model.provider, model.model);
        self.known.insert(id.clone());
        if self.choices.iter().any(|m| m.reference == reference) {
            return;
        }
        let label = if reference == id {
            id
        } else {
            format!("{reference} | {id}")
        };
        self.add(reference.into(), label, true);
    }

    fn add(&mut self, reference: String, label: String, configured: bool) {
        // Catalog labels are remote text, not terminal escape sequences.
        let label: String = label.chars().filter(|c| !c.is_control()).collect();
        let search = label.to_lowercase();
        self.choices.push(Choice {
            reference,
            label,
            search,
            configured,
            enabled: None,
        });
    }

    pub fn load(&mut self, config: &Config, engine: &Engine) {
        for (name, provider) in &config.providers {
            let name = name.clone();
            let provider = provider.clone();
            let engine = engine.clone();
            self.pending
                .spawn(async move { (name, engine.list_models(provider).await) });
        }
    }

    pub fn loading(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Poll from the UI tick, never await a network request on the input loop.
    pub fn poll(&mut self) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        let selected = self.current().map(|m| m.reference.clone());
        let mut changed = false;
        while let Some(result) = self.pending.try_join_next() {
            changed = true;
            match result {
                Ok((provider, Ok(models))) => {
                    for model in models {
                        let reference = format!("{provider}:{}", model.id);
                        if self.known.insert(reference.clone()) {
                            let label = if model.name == model.id {
                                reference.clone()
                            } else {
                                format!("{reference} | {}", model.name)
                            };
                            self.add(reference, label, false);
                        }
                    }
                }
                Ok((provider, Err(error))) => self.errors.push(format!("{provider}: {error}")),
                Err(_) => self.errors.push("Model list task failed".into()),
            }
        }
        if changed {
            self.filter(selected.as_deref());
        }
        changed
    }

    pub fn current(&self) -> Option<&Choice> {
        self.matches
            .get(self.selected)
            .map(|&index| &self.choices[index])
    }

    fn filter(&mut self, keep: Option<&str>) {
        let query = self.query.text.to_lowercase();
        let mut matches: Vec<_> = self
            .choices
            .iter()
            .enumerate()
            .filter_map(|(index, model)| {
                fuzzy_score(&model.search, &query).map(|score| (index, score))
            })
            .collect();
        matches.sort_unstable_by(|(a, a_score), (b, b_score)| {
            b_score
                .cmp(a_score)
                .then_with(|| {
                    self.choices[*b]
                        .configured
                        .cmp(&self.choices[*a].configured)
                })
                .then_with(|| self.choices[*a].label.cmp(&self.choices[*b].label))
        });
        self.matches = matches.into_iter().map(|(index, _)| index).collect();
        self.selected = keep
            .and_then(|reference| {
                self.matches
                    .iter()
                    .position(|&index| self.choices[index].reference == reference)
            })
            .unwrap_or(0);
    }

    pub fn paste(&mut self, text: &str) {
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        self.query.insert(&text);
        self.filter(None);
    }

    pub fn key(&mut self, key: KeyEvent) -> PickerAction {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return PickerAction::Close,
            KeyCode::Char('c') if control => return PickerAction::Close,
            KeyCode::Enter => {
                return self
                    .current()
                    .map(|m| PickerAction::Select(m.reference.clone()))
                    .unwrap_or(PickerAction::None)
            }
            KeyCode::Up | KeyCode::BackTab => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => self.selected = self.selected.saturating_add(1),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(10),
            KeyCode::PageDown => self.selected = self.selected.saturating_add(10),
            KeyCode::Home if control => self.selected = 0,
            KeyCode::End if control => self.selected = self.matches.len().saturating_sub(1),
            KeyCode::Left => self.query.left(),
            KeyCode::Right => self.query.right(),
            KeyCode::Home => self.query.cursor = 0,
            KeyCode::End => self.query.cursor = self.query.text.len(),
            KeyCode::Backspace => {
                self.query.backspace();
                self.filter(None);
            }
            KeyCode::Delete => {
                self.query.delete();
                self.filter(None);
            }
            KeyCode::Char('u') if control => {
                self.query.take();
                self.filter(None);
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.insert(&c.to_string());
                self.filter(None);
            }
            _ => {}
        }
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
        PickerAction::None
    }
}

/// Case-folded subsequence matching: contiguous runs and word starts rank above
/// scattered letters. Separate search words may match in any order.
fn fuzzy_score(text: &str, query: &str) -> Option<i64> {
    let mut score = 0;
    for word in query.split_whitespace() {
        let mut remaining = word.chars().peekable();
        let mut previous = None;
        let mut boundary = true;
        let mut word_score = 0;
        for (index, c) in text.chars().enumerate() {
            if remaining.peek() == Some(&c) {
                word_score += 10 + if boundary { 8 } else { 0 };
                word_score += match previous {
                    Some(last) if last + 1 == index => 12,
                    Some(last) => -((index - last - 1) as i64),
                    None => -(index as i64),
                };
                previous = Some(index);
                remaining.next();
                if remaining.peek().is_none() {
                    break;
                }
            }
            boundary = !c.is_alphanumeric();
        }
        if remaining.peek().is_some() {
            return None;
        }
        score += word_score;
    }
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_search_ranks_contiguous_matches_and_edits_unicode() {
        let config = Config::default();
        let mut picker = Picker::models(&config, &config.model, None);
        picker.add(
            "openrouter:anthropic/claude-sonnet".into(),
            "openrouter:anthropic/claude-sonnet | Sonnet".into(),
            false,
        );
        picker.add(
            "openrouter:other".into(),
            "openrouter:something-odd-named-net".into(),
            false,
        );
        picker.paste("SoNNeT");
        assert_eq!(picker.matches.len(), 2);
        assert_eq!(
            picker.current().unwrap().reference,
            "openrouter:anthropic/claude-sonnet"
        );
        picker.paste(" anthropic");
        assert_eq!(picker.matches.len(), 1);
        picker.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        picker.paste("GPT41M");
        assert_eq!(
            picker.current().unwrap().reference,
            "openrouter:openai/gpt-4.1-mini"
        );
        picker.paste("漢");
        assert!(picker.matches.is_empty());
        assert!(matches!(
            picker.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            PickerAction::None
        ));
        picker.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(picker.matches.len(), 1);
    }

    #[tokio::test]
    async fn catalog_updates_preserve_selection_and_closing_cancels_pending_work() {
        let config = Config::default();
        let mut picker = Picker::models(&config, &config.model, None);
        let selected = picker.current().unwrap().reference.clone();
        picker.pending.spawn(async {
            (
                "openrouter".into(),
                Ok(vec![
                    CatalogModel {
                        id: "openai/gpt-4.1-mini".into(),
                        name: "Duplicate".into(),
                    },
                    CatalogModel {
                        id: "new/model".into(),
                        name: "New Model".into(),
                    },
                ]),
            )
        });
        picker
            .pending
            .spawn(async { ("offline".into(), Err(anyhow::anyhow!("Unavailable"))) });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while picker.loading() {
                picker.poll();
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(picker.choices.len(), 2);
        assert_eq!(picker.current().unwrap().reference, selected);
        assert_eq!(picker.errors, ["offline: Unavailable"]);

        let (send, receive) = tokio::sync::oneshot::channel::<()>();
        picker.pending.spawn(async move {
            let _sender = send;
            std::future::pending::<(String, Result<Vec<CatalogModel>>)>().await
        });
        drop(picker);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), receive)
                .await
                .unwrap()
                .is_err()
        );
    }
}
