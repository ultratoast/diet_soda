//! Bounded subprocess I/O. Read both output pipes while waiting, and kill Unix
//! process groups on cancellation so shell grandchildren do not outlive a run.
//!
//! Subprocesses never inherit the harness's ambient environment. Model-driven
//! shell and command tools therefore cannot accidentally leak provider API
//! keys, arbitrary secrets, or unrelated user variables into a child process.
//! Every subprocess is rebuilt from an explicit cross-platform baseline plus
//! the configured ambient allowlist plus the per-call overlay; callers may
//! opt in to a small allowlist of ambient variables (used by `gh`, which
//! needs authentication tokens).
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

/// Variables the harness is allowed to forward from its own environment into
/// the subprocess. The model-invoked `shell` and command tools opt out of this
/// entirely so provider credentials and arbitrary ambient secrets cannot leak;
/// the `gh` builtin opts in to a narrow set of GitHub-related tokens.
#[derive(Debug, Clone)]
pub struct EnvPolicy {
    pub allow_from_ambient: &'static [&'static str],
}

impl EnvPolicy {
    pub const fn new(allow_from_ambient: &'static [&'static str]) -> Self {
        EnvPolicy { allow_from_ambient }
    }
}

const NO_AMBIENT: &[&str] = &[];
const GH_AMBIENT: &[&str] = &["GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN", "GH_HOST"];

/// GitHub credential variables whose present values are treated as secrets for
/// transcript redaction. `GH_HOST` is intentionally absent: it is a hostname,
/// not a credential, and is only forwarded to the `gh` ambient allowlist.
pub const GH_TOKEN_VARS: &[&str] = &["GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN"];

/// Per-call environment overlay. `overlay` entries are stored verbatim and any
/// `${NAME}` references they contain are expanded against the harness process
/// environment at execution time.
#[derive(Debug, Clone)]
pub struct EnvRequest {
    pub policy: EnvPolicy,
    pub overlay: BTreeMap<String, String>,
}

impl Default for EnvRequest {
    fn default() -> Self {
        Self {
            policy: EnvPolicy::new(NO_AMBIENT),
            overlay: BTreeMap::new(),
        }
    }
}

impl EnvRequest {
    pub fn shell() -> Self {
        Self {
            policy: EnvPolicy::new(NO_AMBIENT),
            overlay: BTreeMap::new(),
        }
    }
    pub fn gh() -> Self {
        Self {
            policy: EnvPolicy::new(GH_AMBIENT),
            overlay: BTreeMap::new(),
        }
    }
    pub fn custom(overlay: BTreeMap<String, String>) -> Self {
        Self {
            policy: EnvPolicy::new(NO_AMBIENT),
            overlay,
        }
    }
}

/// Variables that every subprocess receives when they are present in the
/// harness's own environment. Cross-platform: Unix and Windows ship disjoint
/// sets so the baseline is platform-appropriate without leaking each side's
/// irrelevant variables into the other.
#[cfg(unix)]
const BASELINE: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "LC_NUMERIC",
    "LC_TIME",
    "TERM",
    "TMPDIR",
    "XDG_CONFIG_HOME",
];
#[cfg(windows)]
const BASELINE: &[&str] = &[
    "PATH",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "SystemRoot",
    "SystemDrive",
    "TEMP",
    "TMP",
    "PATHEXT",
    "COMSPEC",
    // Application/config paths the model is likely to need to read.
    "APPDATA",
    "LOCALAPPDATA",
    "USERNAME",
    "USERDOMAIN",
    "OS",
    "PROCESSOR_ARCHITECTURE",
];

/// Build the environment for a subprocess: an explicit platform baseline plus
/// the optional ambient allowlist plus the per-call overlay. Anything else in
/// the harness's environment is dropped on purpose. The child's `PWD` is
/// pinned unconditionally to the request's `cwd` so a stale inherited shell
/// variable cannot redirect `cd ..` or path resolution into an unexpected
/// parent — tools that read `$PWD` see the same path the harness actually
/// spawned the child under.
pub fn isolated_env(request: &EnvRequest, cwd: &Path) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    for name in BASELINE {
        if let Some(value) = std::env::var_os(name) {
            env.insert((*name).to_owned(), value.to_string_lossy().into_owned());
        }
    }
    for name in request.policy.allow_from_ambient {
        if let Some(value) = std::env::var_os(*name) {
            env.insert((*name).to_owned(), value.to_string_lossy().into_owned());
        }
    }
    for (key, raw) in &request.overlay {
        env.insert(key.clone(), crate::config::expand_env(raw)?);
    }
    // PWD is intentionally set from the requested cwd, not inherited from
    // the harness. A stale shell PWD or a symlink-resolved parent must not be
    // able to redirect the child's view of its working directory.
    env.insert("PWD".into(), cwd.to_string_lossy().into_owned());
    Ok(env)
}

fn apply_env(command: &mut Command, env: &BTreeMap<String, String>) {
    command.env_clear();
    for (key, value) in env {
        command.env(key, value);
    }
}

pub struct ProcessRequest<'a> {
    pub command: &'a str,
    pub args: &'a [String],
    pub cwd: &'a Path,
    /// Pre-built environment. Callers should pass the result of
    /// [`isolated_env`] so the subprocess inherits only the platform baseline,
    /// the ambient allowlist, and the per-call overlay. The runner always
    /// clears the inherited environment before applying the supplied one.
    pub env: &'a BTreeMap<String, String>,
    pub input: Option<Vec<u8>>,
    pub timeout: u64,
    pub limit: usize,
}

pub async fn run(request: ProcessRequest<'_>, cancel: &CancellationToken) -> Result<ProcessOutput> {
    // Reject zero-byte output limits up front so a side-effecting command
    // (e.g. `rm`, `git push`) never runs when the caller asked for no output
    // — running it would silently perform the side effect with no way to
    // surface what the command did.
    if request.limit == 0 {
        bail!("Output limit must be positive");
    }
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
    apply_env(&mut command, request.env);
    #[cfg(windows)]
    crate::winjob::prepare_command(&mut command);
    let mut child = command.spawn()?;
    #[cfg(unix)]
    let _group = ProcessGroup(child.id().unwrap());
    // Keep the job guard alive for the whole run. Dropping it closes the
    // kill-on-close job and terminates any surviving descendants, matching the
    // Unix `ProcessGroup` drop. Any setup/assignment error propagates here
    // after `assign` has itself ensured no suspended child survives.
    #[cfg(windows)]
    let _job = crate::winjob::JobObject::assign(&mut child)?;
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
    result
}
