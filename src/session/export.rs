//! Human-readable transcript export uses the redacted event log, including
//! child conversations and tool results, instead of only the parent's history.
use super::Session;
use crate::model::Message;
use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde_json::Value;
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
};

impl Session {
    pub fn export(&self, directory: &Path) -> Result<PathBuf> {
        self.export_at(directory, Local::now())
    }

    pub fn export_at(&self, directory: &Path, now: DateTime<Local>) -> Result<PathBuf> {
        self.checkpoint()?;
        fs::create_dir_all(directory)?;
        let title = now.format("%m:%d:%Y-%H:%M:%S").to_string();
        let filename = if cfg!(windows) {
            title.replace(':', "-")
        } else {
            title.clone()
        };
        let (path, file) = create_unique(directory, &filename)?;
        let result = (|| -> Result<()> {
            let mut output = BufWriter::new(file);
            writeln!(
                output,
                "{title}\nSession: {}\nSpend: {}\n",
                self.id,
                self.spend.display()
            )?;
            for line in BufReader::new(File::open(&self.path)?).split(b'\n') {
                let line = line?;
                // Interrupted tails have already been recorded by recovery events.
                let Ok(event) = serde_json::from_slice::<Value>(&line) else {
                    continue;
                };
                let context = event["context"].as_str().unwrap_or("main");
                if event["type"] == "message" {
                    let message: Message = serde_json::from_value(event["data"].clone())?;
                    writeln!(
                        output,
                        "[{}] {} ({context})\n{}",
                        event["at"].as_str().unwrap_or(""),
                        message.role,
                        message.content
                    )?;
                    for call in message.tool_calls {
                        let arguments = serde_json::from_str::<Value>(&call.arguments)
                            .and_then(|value| serde_json::to_string_pretty(&value))
                            .unwrap_or(call.arguments);
                        writeln!(
                            output,
                            "Tool: {}\nCall ID: {}\nArguments:\n{}",
                            call.name, call.id, arguments
                        )?;
                    }
                    writeln!(output)?;
                } else if ["workflow_start", "workflow_error", "approval", "clear"]
                    .iter()
                    .any(|kind| event["type"] == *kind)
                {
                    writeln!(
                        output,
                        "[{}] {} ({context}): {}\n",
                        event["at"].as_str().unwrap_or(""),
                        event["type"].as_str().unwrap_or("event"),
                        event["data"]
                    )?;
                }
            }
            output.flush()?;
            output.get_ref().sync_all()?;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = fs::remove_file(&path);
            return Err(error).context("Exporting session");
        }
        Ok(path)
    }
}

fn create_unique(directory: &Path, title: &str) -> Result<(PathBuf, File)> {
    for suffix in 0..1000 {
        let name = if suffix == 0 {
            format!("{title}.txt")
        } else {
            format!("{title}-{suffix}.txt")
        };
        let path = directory.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!("Too many exports with this timestamp")
}
