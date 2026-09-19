//! Process-based plugin hooks: one JSON event on stdin and an optional JSON
//! gate response on stdout. After hooks report failures without undoing work.
use crate::{
    config::Config,
    process::{self, ProcessRequest},
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

pub async fn emit(
    config: &Config,
    event: &str,
    payload: Value,
    cancel: &CancellationToken,
) -> Result<()> {
    for hook in config
        .hooks
        .iter()
        .filter(|h| h.enabled && h.event == event)
    {
        let result = process::run(
            ProcessRequest {
                command: &hook.command,
                args: &hook.args,
                cwd: &config.workspace,
                env: &hook.env,
                input: Some(serde_json::to_vec(
                    &json!({"version":1,"event":event,"payload":payload}),
                )?),
                timeout: hook.timeout_seconds,
                limit: 64_000,
            },
            cancel,
        )
        .await
        .with_context(|| format!("Plugin hook {event}"))?;
        if result.exit_code != Some(0) {
            bail!("Plugin hook {event} failed (exit {:?})", result.exit_code);
        }
        if !result.stdout.trim().is_empty() {
            let reply: Value = serde_json::from_str(&result.stdout)
                .with_context(|| format!("Plugin hook {event} returned invalid JSON"))?;
            if let Some(reason) = reply["deny"].as_str() {
                bail!("Plugin hook denied {event}: {reason}");
            }
        }
    }
    Ok(())
}
