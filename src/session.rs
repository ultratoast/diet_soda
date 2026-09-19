//! Append-only session storage and plain-text export. Each event is one write;
//! fsync happens at conversation checkpoints rather than once per tiny event.
mod export;
use crate::model::{Message, Spend, Usage};
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde_json::{json, Value};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

pub struct Session {
    pub id: String,
    pub path: PathBuf,
    pub messages: Vec<Message>,
    pub spend: Spend,
    file: File,
    redactions: Vec<String>,
}
impl Session {
    pub fn open(dir: &Path, id: Option<&str>) -> Result<Self> {
        let id = id
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if !crate::config::valid_name(&id) {
            bail!("Invalid session ID");
        }
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{id}.jsonl"));
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        file.try_lock_exclusive()
            .context("Session is already open in another process")?;
        let mut messages = vec![];
        let mut spend = Spend::default();
        let text = std::fs::read(&path)?;
        let lines: Vec<&[u8]> = text.split(|b| *b == b'\n').collect();
        let ignored: Vec<usize> = lines
            .iter()
            .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
            .filter(|v| v["type"] == "recovery")
            .filter_map(|v| v["data"]["ignored_line"].as_u64().map(|n| n as usize))
            .collect();
        let mut recover = None;
        for (index, line) in lines.iter().enumerate() {
            if ignored.contains(&index) || line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let event: Value = match serde_json::from_slice(line) {
                Ok(value) => value,
                Err(_) if index + 1 == lines.len() && !text.ends_with(b"\n") => {
                    recover = Some(index);
                    continue;
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("Corrupt session line {}", index + 1))
                }
            };
            match event["type"].as_str() {
                Some("message") if event["context"] == "main" => {
                    messages.push(serde_json::from_value(event["data"].clone())?)
                }
                Some("usage") => {
                    spend.add(&serde_json::from_value::<Usage>(event["data"].clone())?)
                }
                Some("clear") => messages.clear(),
                _ => {}
            }
        }
        if !text.is_empty() && !text.ends_with(b"\n") {
            file.write_all(b"\n")?;
        }
        let mut session = Self {
            id,
            path,
            messages,
            spend,
            file,
            redactions: vec![],
        };
        if let Some(index) = recover {
            session.append("recovery", "main", json!({"ignored_line":index}))?;
        }
        if text.is_empty() {
            session.append("session", "main", json!({"id":session.id,"version":1}))?;
        }
        // Complete interrupted tool exchanges so resumed requests remain provider-valid.
        let missing: Vec<String> = session
            .messages
            .iter()
            .flat_map(|m| m.tool_calls.iter())
            .filter(|t| {
                !session
                    .messages
                    .iter()
                    .any(|m| m.tool_call_id.as_ref() == Some(&t.id))
            })
            .map(|t| t.id.clone())
            .collect();
        for id in missing {
            session.record_message(
                "main",
                Message::tool(
                    &id,
                    "Tool execution interrupted before a result was recorded.",
                ),
            )?;
        }
        Ok(session)
    }
    pub fn append(&mut self, kind: &str, context: &str, data: Value) -> Result<()> {
        let mut event =
            json!({"type":kind,"at":chrono::Utc::now().to_rfc3339(),"context":context,"data":data});
        redact(&mut event, &self.redactions);
        let mut bytes = serde_json::to_vec(&event)?;
        bytes.push(b'\n');
        self.file.write_all(&bytes)?;
        Ok(())
    }
    /// Durability boundary for completed turns/steps and explicit exports.
    pub fn checkpoint(&self) -> Result<()> {
        self.file.sync_data()?;
        Ok(())
    }
    pub fn add_redactions(&mut self, values: Vec<String>) {
        self.redactions.extend(values);
        self.redactions.sort_by_key(|s| std::cmp::Reverse(s.len()));
        self.redactions.dedup();
    }
    pub fn record_message(&mut self, context: &str, message: Message) -> Result<()> {
        self.append("message", context, serde_json::to_value(&message)?)?;
        if context == "main" {
            self.messages.push(message);
        }
        Ok(())
    }
    pub fn usage(&mut self, context: &str, usage: &Usage) -> Result<()> {
        self.append("usage", context, serde_json::to_value(usage)?)?;
        self.spend.add(usage);
        Ok(())
    }
    pub fn clear(&mut self) -> Result<()> {
        self.append("clear", "main", json!({}))?;
        self.messages.clear();
        Ok(())
    }
}

fn redact(value: &mut Value, secrets: &[String]) {
    match value {
        Value::String(s) => {
            for secret in secrets {
                let escaped = serde_json::to_string(secret).expect("Strings serialize as JSON");
                *s = s.replace(&escaped[1..escaped.len() - 1], "[REDACTED]");
                *s = s.replace(secret, "[REDACTED]");
            }
        }
        Value::Array(a) => {
            for v in a {
                redact(v, secrets);
            }
        }
        Value::Object(o) => {
            for v in o.values_mut() {
                redact(v, secrets);
            }
        }
        _ => {}
    }
}
