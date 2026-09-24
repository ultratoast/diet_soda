#![cfg(any(target_os = "linux", target_os = "macos"))]

use diet_soda::process::{self, EnvRequest, ProcessRequest};
use std::path::Path;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

fn python() -> &'static str {
    "python3"
}

async fn connect_probe(port: u16, network_access: bool) -> process::ProcessOutput {
    let cwd = std::env::current_dir().unwrap();
    let env = process::isolated_env(&EnvRequest::shell(), &cwd).unwrap();
    let args = vec![
        "-c".to_owned(),
        format!(
            "import socket; s=socket.socket(); s.settimeout(2);\ntry:\n s.connect(('127.0.0.1',{port})); print('CONNECTED')\nexcept OSError:\n print('BLOCKED')"
        ),
    ];
    process::run(
        ProcessRequest {
            command: python(),
            args: &args,
            cwd: Path::new(&cwd),
            env: &env,
            input: None,
            timeout: 5,
            limit: 4096,
            network_access,
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn child_network_is_denied_unless_explicitly_granted() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let denied = connect_probe(port, false).await;
    assert!(
        !denied.stdout.contains("CONNECTED"),
        "child reached listener: {denied:?}"
    );
    // If the host cannot create the network sandbox (for example, a Linux
    // system that forbids unprivileged network namespaces), fail closed: the
    // target must not have run and the launcher must report an error.
    assert!(
        denied.stdout.contains("BLOCKED") || denied.exit_code.is_some_and(|code| code != 0),
        "sandbox neither blocked the connection nor failed closed: {denied:?}"
    );

    let allowed = connect_probe(port, true).await;
    assert_eq!(allowed.exit_code, Some(0), "{allowed:?}");
    assert!(allowed.stdout.contains("CONNECTED"), "{allowed:?}");
}

#[test]
fn network_sandbox_command_preserves_argv_and_is_fail_closed_by_default() {
    let args = vec!["--flag".into(), "two words".into()];
    let (program, wrapped) = process::sandbox_command("example", &args, false).unwrap();
    #[cfg(target_os = "linux")]
    {
        assert_eq!(program, "unshare");
        assert!(wrapped.iter().any(|arg| arg == "--net"));
    }
    #[cfg(target_os = "macos")]
    {
        assert_eq!(program, "/usr/bin/sandbox-exec");
        assert!(wrapped.iter().any(|arg| arg.contains("deny network*")));
    }
    assert!(wrapped.ends_with(&["example".into(), "--flag".into(), "two words".into()]));
    assert_eq!(
        process::sandbox_command("example", &args, true).unwrap(),
        ("example".into(), args)
    );
}
