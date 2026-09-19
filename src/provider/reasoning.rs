//! Wire-specific effort settings and lossless continuation metadata. Signed
//! thinking blocks must be sent back in order when a model resumes tool use.
use crate::config::{ModelConfig, ProviderKind};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub(super) fn apply_effort(
    body: &mut Value,
    model: &ModelConfig,
    provider: &ProviderKind,
) -> Result<()> {
    let Some(reasoning) = &model.reasoning else {
        return Ok(());
    };
    reasoning.validate(provider)?;
    let Some(effort) = reasoning.effort else {
        return Ok(());
    };
    match provider {
        ProviderKind::Openrouter => body["reasoning"] = json!({"effort":effort}),
        ProviderKind::Openai | ProviderKind::Litellm => body["reasoning_effort"] = json!(effort),
        ProviderKind::Anthropic => body["output_config"] = json!({"effort":effort}),
    }
    Ok(())
}

pub(super) fn append_block_delta(blocks: &mut BTreeMap<u64, Value>, event: &Value) -> Result<()> {
    let block = blocks
        .get_mut(&event["index"].as_u64().unwrap_or(0))
        .context("Content delta before block start")?;
    for field in ["text", "thinking", "signature"] {
        if let Some(part) = event["delta"][field].as_str() {
            append_string(&mut block[field], part);
        }
    }
    Ok(())
}

pub(super) fn append_details(details: &mut BTreeMap<u64, Value>, delta: &Value) {
    let Some(items) = delta.as_array() else {
        return;
    };
    for item in items {
        let index = item["index"].as_u64().unwrap_or(details.len() as u64);
        let entry = details.entry(index).or_insert_with(|| json!({}));
        if let Some(fields) = item.as_object() {
            for (key, value) in fields {
                if value.is_null() {
                    continue;
                }
                if ["text", "summary", "data", "signature"].contains(&key.as_str()) {
                    if let Some(part) = value.as_str() {
                        append_string(&mut entry[key], part);
                    }
                } else {
                    entry[key] = value.clone();
                }
            }
        }
    }
}
fn append_string(value: &mut Value, part: &str) {
    if let Value::String(text) = value {
        text.push_str(part);
    } else {
        *value = Value::String(part.into());
    }
}
