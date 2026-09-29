//! Process-tree cancellation tests for the real subprocess runner.

use diet_soda::process::{self, EnvRequest, ProcessRequest};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[cfg(unix)]
mod unix {
    use super::*;
    use nix::{errno::Errno, sys::signal::kill, unistd::Pid};
    use std::{fs, path::Path};

    const POLL_TIMEOUT: Duration = Duration::from_secs(8);
    const RUN_TIMEOUT: Duration = Duration::from_secs(12);

    async fn run_process(
        cwd: &Path,
        env: &std::collections::BTreeMap<String, String>,
        timeout: u64,
        cancel: &CancellationToken,
    ) -> anyhow::Result<process::ProcessOutput> {
        let args = vec![
            "-c".into(),
            "printf '%s' \"$$\" > parent.pid; sleep 120 & child=$!; printf '%s' \"$child\" > child.pid; wait"
                .into(),
        ];
        process::run(
            ProcessRequest {
                command: "/bin/sh",
                args: &args,
                cwd,
                env,
                input: None,
                timeout,
                limit: 4096,
                network_access: false,
            },
            cancel,
        )
        .await
    }

    async fn wait_for_pids(cwd: &Path) -> anyhow::Result<(i32, i32)> {
        tokio::time::timeout(POLL_TIMEOUT, async {
            loop {
                let parent = fs::read_to_string(cwd.join("parent.pid"))
                    .ok()
                    .and_then(|pid| pid.parse().ok());
                let child = fs::read_to_string(cwd.join("child.pid"))
                    .ok()
                    .and_then(|pid| pid.parse().ok());
                if let (Some(parent), Some(child)) = (parent, child) {
                    return Ok((parent, child));
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for shell PID files"))?
    }

    fn process_exists(pid: i32) -> anyhow::Result<bool> {
        match kill(Pid::from_raw(pid), None) {
            Ok(()) => Ok(true),
            Err(Errno::EPERM) => Ok(true),
            Err(Errno::ESRCH) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    async fn wait_for_processes_to_exit(pids: (i32, i32)) -> anyhow::Result<()> {
        tokio::time::timeout(POLL_TIMEOUT, async {
            loop {
                if !process_exists(pids.0)? && !process_exists(pids.1)? {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("process tree remained alive: {:?}", pids))?
    }

    #[tokio::test]
    async fn cancellation_kills_shell_and_grandchild() {
        let temp = tempfile::tempdir().unwrap();
        let env = process::isolated_env(&EnvRequest::shell(), temp.path()).unwrap();
        let cancel = CancellationToken::new();
        let child_cancel = cancel.clone();
        let cwd = temp.path().to_owned();
        let task = tokio::spawn(async move { run_process(&cwd, &env, 120, &child_cancel).await });

        let pids = wait_for_pids(temp.path()).await;
        cancel.cancel();
        let result = tokio::time::timeout(RUN_TIMEOUT, task)
            .await
            .expect("cancelled process runner did not return")
            .expect("process runner task panicked");

        assert!(result.is_err(), "cancellation unexpectedly succeeded");
        let pids = pids.expect("shell did not publish both process IDs");
        wait_for_processes_to_exit(pids)
            .await
            .expect("cancellation leaked a process tree");
    }

    #[tokio::test]
    async fn configured_timeout_kills_shell_and_grandchild() {
        let temp = tempfile::tempdir().unwrap();
        let env = process::isolated_env(&EnvRequest::shell(), temp.path()).unwrap();
        let cwd = temp.path().to_owned();
        let task =
            tokio::spawn(
                async move { run_process(&cwd, &env, 1, &CancellationToken::new()).await },
            );

        let pids = wait_for_pids(temp.path()).await;
        let result = tokio::time::timeout(RUN_TIMEOUT, task)
            .await
            .expect("timed-out process runner did not return")
            .expect("process runner task panicked");

        assert!(result.is_err(), "configured timeout unexpectedly succeeded");
        let pids = pids.expect("shell did not publish both process IDs");
        wait_for_processes_to_exit(pids)
            .await
            .expect("configured timeout leaked a process tree");
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{fs, path::Path, process::Command};

    const POLL_TIMEOUT: Duration = Duration::from_secs(15);
    const RUN_TIMEOUT: Duration = Duration::from_secs(20);

    async fn run_process(
        cwd: &Path,
        env: &std::collections::BTreeMap<String, String>,
        timeout: u64,
        cancel: &CancellationToken,
    ) -> anyhow::Result<process::ProcessOutput> {
        let args = vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            "$p = Start-Process powershell.exe '-NoProfile -NonInteractive -Command Start-Sleep -Seconds 120' -PassThru; Set-Content parent.pid $PID; Set-Content child.pid $p.Id; Wait-Process $p.Id".into(),
        ];
        process::run(
            ProcessRequest {
                command: "powershell.exe",
                args: &args,
                cwd,
                env,
                input: None,
                timeout,
                limit: 4096,
                network_access: false,
            },
            cancel,
        )
        .await
    }

    async fn wait_for_pids(cwd: &Path) -> anyhow::Result<(u32, u32)> {
        tokio::time::timeout(POLL_TIMEOUT, async {
            loop {
                let parent = fs::read_to_string(cwd.join("parent.pid"))
                    .ok()
                    .and_then(|pid| pid.trim().parse().ok());
                let child = fs::read_to_string(cwd.join("child.pid"))
                    .ok()
                    .and_then(|pid| pid.trim().parse().ok());
                if let (Some(parent), Some(child)) = (parent, child) {
                    return Ok((parent, child));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for PowerShell PID files"))?
    }

    fn process_exists(pid: u32) -> anyhow::Result<bool> {
        Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ exit 0 }} else {{ exit 1 }}"),
            ])
            .status()
            .map(|status| status.success())
            .map_err(Into::into)
    }

    async fn wait_for_processes_to_exit(pids: (u32, u32)) -> anyhow::Result<()> {
        tokio::time::timeout(POLL_TIMEOUT, async {
            loop {
                if !process_exists(pids.0)? && !process_exists(pids.1)? {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("process tree remained alive: {:?}", pids))?
    }

    async fn run_tree_test(timeout: u64, cancel_run: bool) {
        let temp = tempfile::tempdir().unwrap();
        let env = process::isolated_env(&EnvRequest::shell(), temp.path()).unwrap();
        let cancel = CancellationToken::new();
        let child_cancel = cancel.clone();
        let cwd = temp.path().to_owned();
        let task =
            tokio::spawn(async move { run_process(&cwd, &env, timeout, &child_cancel).await });
        let pids = wait_for_pids(temp.path()).await;
        if cancel_run {
            cancel.cancel();
        }
        let result = tokio::time::timeout(RUN_TIMEOUT, task)
            .await
            .expect("process runner did not return")
            .expect("process runner task panicked");
        assert!(result.is_err());
        let pids = pids.expect("PowerShell did not publish both process IDs");
        wait_for_processes_to_exit(pids).await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_kills_powershell_descendant() {
        run_tree_test(120, true).await;
    }

    #[tokio::test]
    async fn configured_timeout_kills_powershell_descendant() {
        run_tree_test(1, false).await;
    }
}
