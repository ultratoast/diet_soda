//! Read-only model discovery through each provider's configured API endpoint.
use super::RemoteProvider;
use crate::tools::read_response;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{collections::BTreeMap, time::Duration};

#[derive(Debug)]
pub struct CatalogModel {
    pub id: String,
    pub name: String,
}

impl RemoteProvider {
    /// Catalog calls share the connection pool but never generate tokens. Bound
    /// the entire operation, including pagination, so a picker cannot hang.
    pub async fn list_models(&self) -> Result<Vec<CatalogModel>> {
        tokio::time::timeout(
            Duration::from_secs(self.config.timeout_seconds.min(15)),
            self.model_pages(),
        )
        .await
        .context("Model list timed out")?
    }

    async fn model_pages(&self) -> Result<Vec<CatalogModel>> {
        let url = format!("{}/models", self.config.base_url.trim_end_matches('/'));
        let mut models = BTreeMap::new();
        let mut cursor = None;
        for _ in 0..100 {
            let mut request = self.client.get(&url);
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
                if id.is_empty() || id.chars().any(char::is_control) {
                    bail!("Model list contains an invalid id");
                }
                let name = entry["display_name"]
                    .as_str()
                    .or_else(|| entry["name"].as_str())
                    .unwrap_or(id);
                models.insert(
                    id.to_owned(),
                    CatalogModel {
                        id: id.into(),
                        name: name.into(),
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
