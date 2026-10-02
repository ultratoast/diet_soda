//! Read-only model discovery through each provider's configured API endpoint.
use super::RemoteProvider;
use crate::tools::read_response;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{collections::BTreeMap, time::Duration};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub struct CatalogModel {
    pub id: String,
    pub name: String,
    /// Model context window in tokens, if the catalog advertises one.
    pub context_window: Option<u32>,
    /// Model max output/completion tokens, if the catalog advertises one.
    pub max_output: Option<u32>,
}

/// Return the first positive integer found by following each key-path (a slice
/// of object keys) in order. Non-present keys, non-integers, zero, and values
/// above u32::MAX are skipped. Used to read model limits from provider catalog
/// entries whose field names differ across providers.
fn first_positive_u32(entry: &Value, paths: &[&[&str]]) -> Option<u32> {
    for path in paths {
        let mut node = entry;
        let mut found = true;
        for key in *path {
            match node.get(key) {
                Some(next) => node = next,
                None => {
                    found = false;
                    break;
                }
            }
        }
        if found {
            if let Some(n) = node.as_u64() {
                if n > 0 && n <= u32::MAX as u64 {
                    return Some(n as u32);
                }
            }
        }
    }
    None
}

impl RemoteProvider {
    /// Catalog calls never generate tokens. Bound the entire operation,
    /// including DNS, connection setup, and pagination, so a picker cannot hang.
    pub async fn list_models(&self) -> Result<Vec<CatalogModel>> {
        let cancel = CancellationToken::new();
        tokio::time::timeout(
            Duration::from_secs(self.config.timeout_seconds.min(15)),
            self.model_pages(&cancel),
        )
        .await
        .context("Model list timed out")?
    }

    async fn model_pages(&self, cancel: &CancellationToken) -> Result<Vec<CatalogModel>> {
        let url = format!("{}/models", self.config.base_url.trim_end_matches('/'));
        crate::config::validate_url(&url).context("Invalid provider model-list URL")?;
        let parsed_url = reqwest::Url::parse(&url)?;
        let client = crate::tools::guarded_http_client(
            &parsed_url,
            self.config.allow_private_networks,
            Some(self.config.timeout_seconds.min(15)),
            cancel,
        )
        .await?;
        let mut models = BTreeMap::new();
        let mut cursor = None;
        for _ in 0..100 {
            let mut request = client.get(&url);
            if let Some(cursor) = &cursor {
                request = request.query(&[("after_id", cursor)]);
            }
            let response = self
                .authenticate(request)?
                .send()
                .await
                .context("Model list connection failed")?;
            if !response.status().is_success() {
                bail!("Model list returned HTTP {}", response.status());
            }
            let (bytes, truncated) = read_response(response, 10_000_000).await?;
            if truncated {
                bail!("Model list exceeds 10 MB");
            }
            let page: Value = serde_json::from_slice(&bytes).context("Invalid model list JSON")?;
            let data = page["data"]
                .as_array()
                .context("Model list omitted data array")?;
            for entry in data {
                let id = entry["id"]
                    .as_str()
                    .context("Model list entry omitted id")?;
                if id.is_empty() || id.chars().any(crate::text::is_unsafe_terminal_char) {
                    bail!("Model list contains an invalid id");
                }
                let name = entry["display_name"]
                    .as_str()
                    .or_else(|| entry["name"].as_str())
                    .unwrap_or(id);
                let context_window = first_positive_u32(
                    entry,
                    &[&["context_length"], &["context_window"], &["max_input_tokens"]],
                );
                let max_output = first_positive_u32(
                    entry,
                    &[
                        &["top_provider", "max_completion_tokens"],
                        &["max_output_tokens"],
                        &["max_tokens"],
                    ],
                );
                models.insert(
                    id.to_owned(),
                    CatalogModel {
                        id: id.into(),
                        name: name.into(),
                        context_window,
                        max_output,
                    },
                );
            }
            if page["has_more"] != true {
                return Ok(models.into_values().collect());
            }
            let next = page["last_id"]
                .as_str()
                .context("Model list omitted pagination cursor")?;
            if next.is_empty() || cursor.as_deref() == Some(next) {
                bail!("Model list pagination did not advance");
            }
            cursor = Some(next.to_owned());
        }
        bail!("Model list exceeds 100 pages")
    }
}

#[cfg(test)]
mod tests {
    use super::first_positive_u32;
    use serde_json::json;

    #[test]
    fn first_positive_u32_reads_top_level_value() {
        assert_eq!(
            first_positive_u32(&json!({"context_length": 128000}), &[&["context_length"]]),
            Some(128000)
        );
    }

    #[test]
    fn first_positive_u32_reads_nested_value() {
        assert_eq!(
            first_positive_u32(
                &json!({"top_provider": {"max_completion_tokens": 4000}}),
                &[&["top_provider", "max_completion_tokens"]]
            ),
            Some(4000)
        );
    }

    #[test]
    fn first_positive_u32_uses_fallback_path() {
        assert_eq!(
            first_positive_u32(
                &json!({"max_tokens": 8000}),
                &[&["max_output_tokens"], &["max_tokens"]]
            ),
            Some(8000)
        );
    }

    #[test]
    fn first_positive_u32_skips_null_and_uses_fallback() {
        assert_eq!(
            first_positive_u32(
                &json!({
                    "top_provider": {"max_completion_tokens": null},
                    "max_tokens": 8000
                }),
                &[&["top_provider", "max_completion_tokens"], &["max_tokens"]]
            ),
            Some(8000)
        );
    }

    #[test]
    fn first_positive_u32_rejects_zero_negative_and_oversize_values() {
        assert_eq!(
            first_positive_u32(&json!({"context_length": 0}), &[&["context_length"]]),
            None
        );
        assert_eq!(
            first_positive_u32(&json!({"context_length": -1}), &[&["context_length"]]),
            None
        );
        assert_eq!(
            first_positive_u32(
                &json!({"context_length": 99999999999999_u64}),
                &[&["context_length"]]
            ),
            None
        );
    }
}
