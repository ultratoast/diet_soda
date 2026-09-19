//! Bounded subprocess I/O. Read both output pipes while waiting, and kill Unix
//! process groups on cancellation so shell grandchildren do not outlive a run.
use anyhow::{bail, Result};
use serde::Serialize;
use std::{collections::BTreeMap, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Serialize)]
pub struct ProcessOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

#[cfg(unix)]
pub struct ProcessGroup(pub u32);
#[cfg(unix)]
impl ProcessGroup {
    pub fn terminate(&self) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(self.0 as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}
#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.terminate();
    }
}

async fn bounded_read(mut reader: impl AsyncRead + Unpin, limit: usize) -> Result<(String, bool)> {
    let mut output = vec![];
    let mut buf = [0; 8192];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut buf).await?;
        if count == 0 {
            break;
        }
        let keep = count.min(limit.saturating_sub(output.len()));
        output.extend_from_slice(&buf[..keep]);
        truncated |= keep < count;
    }
    Ok((String::from_utf8_lossy(&output).into_owned(), truncated))
}

pub struct ProcessRequest<'a> {
    pub command: &'a str,
    pub args: &'a [String],
    pub cwd: &'a Path,
    pub env: &'a BTreeMap<String, String>,
    pub input: Option<Vec<u8>>,
    pub timeout: u64,
    pub limit: usize,
}

pub async fn run(request: ProcessRequest<'_>, cancel: &CancellationToken) -> Result<ProcessOutput> {
    let mut command = Command::new(request.command);
    #[cfg(unix)]
    command.process_group(0);
    command
        .args(request.args)
        .current_dir(request.cwd)
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in request.env {
        command.env(key, crate::config::expand_env(value)?);
    }
    let mut child = command.spawn()?;
    #[cfg(unix)]
    let _group = ProcessGroup(child.id().unwrap());
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let work = async {
        let write = async {
            if let Some(input) = request.input {
                stdin.write_all(&input).await?;
            }
            drop(stdin);
            Ok::<_, anyhow::Error>(())
        };
        let (_, out, err, status) = tokio::try_join!(
            write,
            bounded_read(stdout, request.limit),
            bounded_read(stderr, request.limit),
            async { Ok::<_, anyhow::Error>(child.wait().await?) }
        )?;
        Ok::<_, anyhow::Error>(ProcessOutput {
            exit_code: status.code(),
            stdout: out.0,
            stderr: err.0,
            truncated: out.1 || err.1,
        })
    };
    let result = tokio::select! {
        _ = cancel.cancelled() => Err(anyhow::anyhow!("Cancelled")),
        result = tokio::time::timeout(Duration::from_secs(request.timeout), work) => match result { Ok(result) => result, Err(_) => Err(anyhow::anyhow!("Command timed out")) },
    };
    if result.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    if request.limit == 0 {
        bail!("Output limit must be positive");
    }
    result
}
