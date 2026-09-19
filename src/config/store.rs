//! Small atomic edits for /model add and /mcp add. Never serialize a runtime
//! Config back to disk: its paths are absolute and it may contain UI overrides.
use super::{valid_name, Config};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};

pub fn insert_named(path: &Path, section: &str, name: &str, mut value: Value) -> Result<Config> {
    if !["models", "mcp_servers"].contains(&section) || !valid_name(name) {
        bail!("Invalid configuration section or name");
    }
    let path = fs::canonicalize(path)?;
    let original = fs::read(&path)?;
    let mut document: Value = serde_json::from_slice(&original)?;
    if section == "mcp_servers" && value.get("uuid").is_none() {
        value
            .as_object_mut()
            .context("MCP definition must be a JSON object")?
            .insert("uuid".into(), json!(uuid::Uuid::new_v4()));
    }
    if document.get(section).is_none() {
        document[section] = if section == "models" {
            json!([])
        } else {
            json!({})
        };
    }
    if document[section].is_array() {
        let entries = document[section].as_array_mut().unwrap();
        if entries.iter().any(|entry| entry["name"] == name) {
            bail!("{name} already exists in {section}; edit the JSON file to change it");
        }
        value
            .as_object_mut()
            .context("Named definition must be a JSON object")?
            .insert("name".into(), json!(name));
        entries.push(value);
    } else {
        let entries = document[section]
            .as_object_mut()
            .context("Expected a configuration array or object")?;
        if entries.contains_key(name) {
            bail!("{name} already exists in {section}; edit the JSON file to change it");
        }
        entries.insert(name.into(), value);
    }
    serde_json::from_value::<Config>(document.clone())?.validate()?;

    let temp = path.with_file_name(format!(".config-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.set_permissions(fs::metadata(&path)?.permissions())?;
        let mut bytes = serde_json::to_vec_pretty(&document)?;
        bytes.push(b'\n');
        file.write_all(&bytes)?;
        file.sync_all()?;
        if fs::read(&path)? != original {
            bail!("Configuration changed during the edit; retry the command");
        }
        // Same-directory rename prevents observers from seeing a half-written config.
        fs::rename(&temp, &path)?;
        Config::load(&path)
    })();
    if temp.exists() {
        let _ = fs::remove_file(temp);
    }
    result
}
