//! Process-based plugin hooks: one JSON event on stdin and an optional JSON
//! gate response on stdout. After hooks report failures without undoing work.
use crate::{
    config::Config,
    process::{self, EnvRequest, ProcessRequest},
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// True when at least one enabled hook is registered for `event`. Callers use
/// this to skip constructing an expensive payload that no hook could observe.
pub fn has_listener(config: &Config, event: &str) -> bool {
    config
        .hooks
        .iter()
        .any(|hook| hook.enabled && hook.event == event)
}

/// [`emit`] with lazy payload construction. The closure runs exactly once and
/// synchronously, only after confirming an enabled hook listens for `event`;
/// otherwise it is never invoked and no payload is built. The listener check
/// happens before any await, so no lock is held while building the payload.
pub async fn emit_lazy(
    config: &Config,
    event: &str,
    payload: impl FnOnce() -> Value,
    cancel: &CancellationToken,
) -> Result<()> {
    if !has_listener(config, event) {
        return Ok(());
    }
    emit(config, event, payload(), cancel).await
}

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
        let isolated =
            process::isolated_env(&EnvRequest::custom(hook.env.clone()), &config.workspace)?;
        let result = process::run(
            ProcessRequest {
                command: &hook.command,
                args: &hook.args,
                cwd: &config.workspace,
                env: &isolated,
                input: Some(serde_json::to_vec(
                    &json!({"version":1,"event":event,"payload":payload}),
                )?),
                timeout: hook.timeout_seconds,
                limit: 64_000,
                network_access: hook.network_access,
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
