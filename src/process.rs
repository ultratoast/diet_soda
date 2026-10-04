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
use std::{
    collections::{BTreeMap, VecDeque},
    path::Path,
    process::Stdio,
    time::Duration,
};
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
    // Head+tail ring buffer: keep the first `limit / 2` bytes and the last
    // `limit - limit / 2` bytes, discarding the middle so memory stays bounded
    // by `limit` no matter how much the process writes. The tail is a deque so
    // discarding old bytes is O(1) per byte; a Vec would shift the whole tail
    // on every 8 KB chunk, which is quadratic for large limits (up to
    // `MAX_RESPONSE_BYTES`).
    let head_budget = limit / 2;
    let tail_budget = limit - head_budget;
    let mut head: Vec<u8> = Vec::new();
    let mut tail: VecDeque<u8> = VecDeque::new();
    let mut total = 0usize;
    let mut discarded = 0usize;
    let mut buf = [0; 8192];
    loop {
        let count = reader.read(&mut buf).await?;
        if count == 0 {
            break;
        }
        total += count;
        let mut data = &buf[..count];
        if head.len() < head_budget {
            let take = (head_budget - head.len()).min(data.len());
            head.extend_from_slice(&data[..take]);
            data = &data[take..];
        }
        if data.is_empty() {
            continue;
        }
        tail.extend(data);
        if tail.len() > tail_budget {
            let excess = tail.len() - tail_budget;
            discarded += excess;
            for _ in 0..excess {
                tail.pop_front();
            }
        }
    }
    let tail = tail.make_contiguous();
    if discarded == 0 {
        let mut output = String::from_utf8_lossy(&head).into_owned();
        output.push_str(&String::from_utf8_lossy(tail));
        return Ok((output, false));
    }
    let marker = format!(
        "\n[truncated: showing first {} and last {} of {} bytes]\n",
        head.len(),
        tail.len(),
        total
    );
    let mut output = String::from_utf8_lossy(&head).into_owned();
    output.push_str(&marker);
    output.push_str(&String::from_utf8_lossy(tail));
    Ok((output, true))
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
    /// Permit this child to use the host network. False runs it inside the
    /// platform network-denial sandbox and fails closed if unavailable.
    pub network_access: bool,
}

/// Wrap a child in a platform network-denial sandbox unless its configuration
/// explicitly grants network access. A missing/failed sandbox is an error; the
/// requested program is never started without the restriction.
pub fn sandbox_command(
    command: &str,
    args: &[String],
    network_access: bool,
) -> Result<(String, Vec<String>)> {
    if network_access {
        return Ok((command.to_owned(), args.to_vec()));
    }
    #[cfg(target_os = "linux")]
    {
        let mut sandbox_args = vec![
            "--user".into(),
            "--map-root-user".into(),
            "--net".into(),
            "--fork".into(),
            "--".into(),
            command.into(),
        ];
        sandbox_args.extend_from_slice(args);
        Ok(("unshare".into(), sandbox_args))
    }
    #[cfg(target_os = "macos")]
    {
        let mut sandbox_args = vec![
            "-p".into(),
            "(version 1) (deny network*) (allow default)".into(),
            "--".into(),
            command.into(),
        ];
        sandbox_args.extend_from_slice(args);
        Ok(("/usr/bin/sandbox-exec".into(), sandbox_args))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (command, args);
        bail!("Network-denied subprocesses are supported only on Linux and macOS");
    }
}

pub async fn run(request: ProcessRequest<'_>, cancel: &CancellationToken) -> Result<ProcessOutput> {
    // Reject zero-byte output limits up front so a side-effecting command
    // (e.g. `rm`, `git push`) never runs when the caller asked for no output
    // — running it would silently perform the side effect with no way to
    // surface what the command did.
    if request.limit == 0 {
        bail!("Output limit must be positive");
    }
    let (program, args) = sandbox_command(request.command, request.args, request.network_access)?;
    let mut command = Command::new(program);
    #[cfg(unix)]
    command.process_group(0);
    command
        .args(args)
        .current_dir(request.cwd)
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_env(&mut command, request.env);
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
    let output = result?;
    if !request.network_access && sandbox_launcher_failed(&output) {
        if let Some(name) = missing_program(&output.stderr) {
            bail!("Command not found: {name}");
        }
        bail!("Network sandbox failed closed: {}", output.stderr.trim());
    }
    Ok(output)
}

fn sandbox_launcher_failed(output: &ProcessOutput) -> bool {
    output.stderr.starts_with("unshare: failed to execute ")
        || output.stderr.starts_with("unshare: unshare failed")
        || output.stderr.starts_with("sandbox-exec:")
}

/// Extract the missing program name from a sandbox launcher's ENOENT stderr,
/// so a missing binary is reported as "Command not found" rather than as a
/// sandbox failure. Handles the macOS `sandbox-exec: execvp() of '<name>'
/// failed: No such file or directory` form (name is single-quoted) and the
/// Linux `unshare: failed to execute <name>: No such file or directory` form
/// (name is unquoted, so take everything up to the LAST occurrence of the
/// ENOENT suffix). Returns None for any other launcher error.
fn missing_program(stderr: &str) -> Option<&str> {
    let stderr = stderr.trim();
    const ENOENT: &str = ": No such file or directory";

    if let Some(rest) = stderr.strip_prefix("sandbox-exec: execvp() of '") {
        let (name, suffix) = rest.rsplit_once("' failed")?;
        if !name.is_empty() && suffix.ends_with(ENOENT) {
            return Some(name);
        }
        return None;
    }

    if let Some(rest) = stderr.strip_prefix("unshare: failed to execute ") {
        if let Some(idx) = rest.rfind(ENOENT) {
            let name = &rest[..idx];
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        bounded_read, isolated_env, missing_program, run, EnvRequest, ProcessRequest,
    };
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn bounded_read_returns_outputs_within_the_limit_untouched() {
        let (output, truncated) = bounded_read(&b"hello\n"[..], 10).await.unwrap();
        assert!(!truncated);
        assert_eq!(output, "hello\n");
    }

    #[tokio::test]
    async fn bounded_read_output_exactly_at_limit_is_untouched() {
        // 6-byte input, limit 6: head keeps 3, tail keeps 3, nothing discarded.
        let (output, truncated) = bounded_read(&b"abcdef"[..], 6).await.unwrap();
        assert!(!truncated);
        assert_eq!(output, "abcdef");
    }

    #[tokio::test]
    async fn bounded_read_zero_and_one_byte_limits_do_not_panic() {
        let (output, truncated) = bounded_read(&b"hello"[..], 0).await.unwrap();
        assert!(truncated);
        assert!(output.contains("[truncated: showing first 0 and last 0 of 5 bytes]"));

        let (output, truncated) = bounded_read(&b"hello"[..], 1).await.unwrap();
        assert!(truncated);
        assert!(output.ends_with('o'));
    }

    #[tokio::test]
    async fn bounded_read_empty_input_returns_empty_untruncated() {
        let (output, truncated) = bounded_read(&b""[..], 8).await.unwrap();
        assert!(!truncated);
        assert_eq!(output, "");
    }

    #[tokio::test]
    async fn bounded_read_multibyte_split_at_head_tail_boundary_does_not_panic() {
        // 64 bytes; limit 10 splits at byte 5 (head) and byte 59 (tail), both
        // mid-code-point. The lossy decode must not panic.
        let input = "é".repeat(32);
        let (output, truncated) = bounded_read(input.as_bytes(), 10).await.unwrap();
        assert!(truncated);
        assert!(output.contains("[truncated"));
    }

    #[tokio::test]
    async fn run_stderr_only_overflow_sets_truncated() {
        let cwd = std::env::temp_dir();
        let env = isolated_env(&EnvRequest::shell(), &cwd).unwrap();
        // ~160 KB written to stderr only; stdout stays empty.
        let args = vec![
            "-c".to_string(),
            "i=0; while [ \"$i\" -lt 4000 ]; do printf '0123456789012345678901234567890123456789'; i=$((i+1)); done >&2"
                .to_string(),
        ];
        let request = ProcessRequest {
            command: "/bin/sh",
            args: &args,
            cwd: &cwd,
            env: &env,
            input: None,
            timeout: 30,
            limit: 64,
            network_access: true,
        };
        let output = run(request, &CancellationToken::new()).await.unwrap();
        assert_eq!(output.exit_code, Some(0));
        assert_eq!(output.stdout, "");
        assert!(output.truncated, "stderr-only overflow must set truncated");
        assert!(output.stderr.contains("[truncated"));
    }

    #[tokio::test]
    async fn bounded_read_keeps_head_and_tail_with_marker() {
        let input = "aaaaaaTAIL";
        let (output, truncated) = bounded_read(input.as_bytes(), 8).await.unwrap();
        assert!(truncated);
        assert_eq!(
            output,
            "aaaa\n[truncated: showing first 4 and last 4 of 10 bytes]\nTAIL"
        );
    }

    #[test]
    fn missing_program_macos_single_name() {
        assert_eq!(
            missing_program("sandbox-exec: execvp() of 'rg' failed: No such file or directory"),
            Some("rg")
        );
    }

    #[test]
    fn missing_program_macos_name_with_spaces() {
        assert_eq!(
            missing_program("sandbox-exec: execvp() of 'ls -R' failed: No such file or directory"),
            Some("ls -R")
        );
    }

    #[test]
    fn missing_program_macos_empty_name() {
        assert_eq!(
            missing_program("sandbox-exec: execvp() of '' failed: No such file or directory"),
            None
        );
    }

    #[test]
    fn missing_program_linux_form() {
        assert_eq!(
            missing_program("unshare: failed to execute rg: No such file or directory"),
            Some("rg")
        );
    }

    #[test]
    fn missing_program_linux_trailing_newline() {
        assert_eq!(
            missing_program("unshare: failed to execute rg: No such file or directory\n"),
            Some("rg")
        );
    }

    #[test]
    fn missing_program_unrelated_sandbox_exec_error() {
        assert_eq!(missing_program("sandbox-exec: some other error"), None);
    }

    #[test]
    fn missing_program_unshare_failure() {
        assert_eq!(missing_program("unshare: unshare failed: ..."), None);
    }
}
