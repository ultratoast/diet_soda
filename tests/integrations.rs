mod support;
use diet_soda::{
    config::{Config, HookConfig, McpConfig, McpTransport, ToolConfig},
    hooks,
    mcp::McpManager,
    process::{self, ProcessRequest},
    skills, tools,
};
use serde_json::json;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    collections::BTreeMap,
    fs,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use support::*;
use tokio_util::sync::CancellationToken;

static PROXY_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn python_command() -> &'static str {
    if cfg!(windows) {
        "python"
    } else {
        "python3"
    }
}

fn gate_server_config(name: &str, marker: &std::path::Path) -> (String, McpConfig) {
    let mut env = BTreeMap::new();
    env.insert("MCP_GATE_MARKER".into(), marker.display().to_string());
    (
        name.into(),
        McpConfig {
            uuid: format!("{name}-id"),
            enabled: true,
            hitl: false,
            timeout_seconds: 5,
            allow_private_networks: true,
            network_access: false,
            transport: McpTransport::Stdio {
                command: python_command().into(),
                args: vec![format!(
                    "{}/tests/fixtures/mcp_gate_server.py",
                    env!("CARGO_MANIFEST_DIR")
                )],
                env,
            },
        },
    )
}

async fn wait_for_marker(path: &std::path::Path, lines: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if fs::read_to_string(path)
                .map(|contents| contents.lines().count() >= lines)
                .unwrap_or(false)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture did not spawn within the bounded wait");
}

fn marker_count(path: &std::path::Path) -> usize {
    fs::read_to_string(path).unwrap_or_default().lines().count()
}

fn stderr_fixture_config(tmp: &std::path::Path, env: BTreeMap<String, String>) -> Config {
    let mut config = config("http://localhost:12345", tmp);
    config.mcp_servers.insert(
        "stderr".into(),
        McpConfig {
            uuid: "stderr-id".into(),
            enabled: true,
            hitl: false,
            timeout_seconds: 2,
            allow_private_networks: true,
            network_access: false,
            transport: McpTransport::Stdio {
                command: python_command().into(),
                args: vec![format!(
                    "{}/tests/fixtures/mcp_server.py",
                    env!("CARGO_MANIFEST_DIR")
                )],
                env,
            },
        },
    );
    config
}

async fn shutdown_mcp(manager: &McpManager) {
    tokio::time::timeout(Duration::from_secs(2), manager.shutdown())
        .await
        .expect("MCP fixture shutdown must complete within the bounded wait");
}

struct ProxyEnvGuard {
    previous: [(&'static str, Option<std::ffi::OsString>); 4],
}

impl ProxyEnvGuard {
    fn install(unreachable_proxy: &str) -> Self {
        let names = ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"];
        let previous = names.map(|name| (name, std::env::var_os(name)));
        for name in names {
            std::env::set_var(
                name,
                if name == "NO_PROXY" {
                    ""
                } else {
                    unreachable_proxy
                },
            );
        }
        Self { previous }
    }
}

impl Drop for ProxyEnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.previous {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

#[tokio::test]
async fn mcp_stdio_initializes_discovers_invokes_restarts_and_shuts_down() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config("http://localhost:12345", tmp.path());
    config.mcp_servers.insert(
        "fixture".into(),
        McpConfig {
            uuid: "fixture-id".into(),
            enabled: true,
            hitl: false,
            timeout_seconds: 5,
            allow_private_networks: true,
            network_access: false,
            transport: McpTransport::Stdio {
                command: python_command().into(),
                args: vec![format!(
                    "{}/tests/fixtures/mcp_server.py",
                    env!("CARGO_MANIFEST_DIR")
                )],
                env: BTreeMap::new(),
            },
        },
    );
    let manager = McpManager::default();
    let cancel = CancellationToken::new();
    let tools = manager.tools("fixture", &config, &cancel).await.unwrap();
    assert_eq!(tools[0].spec.name, "mcp_fixture__echo");
    let result = manager
        .call(&tools[0], json!({"text":"hello"}), &config, &cancel)
        .await
        .unwrap();
    assert_eq!(result["content"][0]["text"], "hello");
    let cancelled = CancellationToken::new();
    let trigger = cancelled.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        trigger.cancel();
    });
    assert!(manager
        .call(&tools[0], json!({"text":"__wait__"}), &config, &cancelled)
        .await
        .is_err());
    // Cancellation discards the connection; a subsequent call starts a fresh server.
    let result = manager
        .call(&tools[0], json!({"text":"fresh"}), &config, &cancel)
        .await
        .unwrap();
    assert_eq!(result["content"][0]["text"], "fresh");
    manager.stop("fixture").await;
    assert_eq!(
        manager
            .tools("fixture", &config, &cancel)
            .await
            .unwrap()
            .len(),
        1
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn mcp_initialization_failure_includes_fixture_stderr() {
    let tmp = tempfile::tempdir().unwrap();
    let mut env = BTreeMap::new();
    env.insert("MCP_FIXTURE_FAILURE".into(), "initialize".into());
    env.insert(
        "MCP_FIXTURE_STDERR_PREFIX".into(),
        "initialization diagnostic".into(),
    );
    let config = stderr_fixture_config(tmp.path(), env);
    let manager = McpManager::default();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        manager.tools("stderr", &config, &CancellationToken::new()),
    )
    .await
    .expect("initialization failure must be bounded");

    let error = match result {
        Ok(_) => panic!("initialization should fail"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("MCP server stderr:"), "{error}");
    assert!(error.contains("initialization diagnostic"), "{error}");
    shutdown_mcp(&manager).await;
}

#[tokio::test]
async fn mcp_call_transport_failure_includes_fixture_stderr() {
    let tmp = tempfile::tempdir().unwrap();
    let mut env = BTreeMap::new();
    env.insert("MCP_FIXTURE_FAILURE".into(), "call".into());
    env.insert(
        "MCP_FIXTURE_STDERR_PREFIX".into(),
        "call transport diagnostic".into(),
    );
    let config = stderr_fixture_config(tmp.path(), env);
    let manager = McpManager::default();
    let cancel = CancellationToken::new();
    let tools = tokio::time::timeout(
        Duration::from_secs(2),
        manager.tools("stderr", &config, &cancel),
    )
    .await
    .expect("MCP initialization must be bounded")
    .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        manager.call(
            &tools[0],
            json!({"text":"transport-failure"}),
            &config,
            &cancel,
        ),
    )
    .await
    .expect("MCP call failure must be bounded");

    let error = match result {
        Ok(_) => panic!("call should fail"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("MCP server stderr:"), "{error}");
    assert!(error.contains("call transport diagnostic"), "{error}");
    shutdown_mcp(&manager).await;
}

#[tokio::test]
async fn mcp_success_with_stderr_preserves_protocol_and_result() {
    let tmp = tempfile::tempdir().unwrap();
    let mut env = BTreeMap::new();
    env.insert(
        "MCP_FIXTURE_STDERR_PREFIX".into(),
        "routine diagnostic".into(),
    );
    let config = stderr_fixture_config(tmp.path(), env);
    let manager = McpManager::default();
    let cancel = CancellationToken::new();
    let tools = tokio::time::timeout(
        Duration::from_secs(2),
        manager.tools("stderr", &config, &cancel),
    )
    .await
    .expect("MCP initialization must be bounded")
    .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        manager.call(
            &tools[0],
            json!({"text":"still-successful"}),
            &config,
            &cancel,
        ),
    )
    .await
    .expect("MCP call must be bounded")
    .unwrap();

    assert_eq!(result["content"][0]["text"], "still-successful");
    shutdown_mcp(&manager).await;
}

#[tokio::test]
async fn mcp_stderr_retains_only_the_tail() {
    let tmp = tempfile::tempdir().unwrap();
    let mut env = BTreeMap::new();
    env.insert("MCP_FIXTURE_FAILURE".into(), "initialize".into());
    env.insert(
        "MCP_FIXTURE_STDERR_PREFIX".into(),
        format!("HEAD_MARKER{}", "x".repeat(9 * 1024)),
    );
    env.insert("MCP_FIXTURE_STDERR_SUFFIX".into(), "TAIL_MARKER".into());
    let config = stderr_fixture_config(tmp.path(), env);
    let manager = McpManager::default();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        manager.tools("stderr", &config, &CancellationToken::new()),
    )
    .await
    .expect("large stderr failure must be bounded");

    let error = match result {
        Ok(_) => panic!("large stderr initialization should fail"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("TAIL_MARKER"), "{error}");
    assert!(
        !error.contains("HEAD_MARKER"),
        "stderr head was retained: {error}"
    );
    shutdown_mcp(&manager).await;
}

#[tokio::test]
async fn mcp_invalid_utf8_stderr_is_lossy_without_panicking() {
    let tmp = tempfile::tempdir().unwrap();
    let mut env = BTreeMap::new();
    env.insert("MCP_FIXTURE_FAILURE".into(), "initialize".into());
    env.insert("MCP_FIXTURE_INVALID_STDERR".into(), "1".into());
    let config = stderr_fixture_config(tmp.path(), env);
    let manager = McpManager::default();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        manager.tools("stderr", &config, &CancellationToken::new()),
    )
    .await
    .expect("invalid UTF-8 failure must be bounded");

    let error = match result {
        Ok(_) => panic!("invalid UTF-8 initialization should fail"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("fixture diagnostic"), "{error}");
    assert!(
        error.contains('\u{fffd}'),
        "invalid UTF-8 was not lossy: {error}"
    );
    shutdown_mcp(&manager).await;
}

#[tokio::test]
async fn mcp_connection_gates_are_independent_per_server() {
    let tmp = tempfile::tempdir().unwrap();
    let slow_marker = tmp.path().join("slow.marker");
    let fast_marker = tmp.path().join("fast.marker");
    let mut config = config("http://localhost:12345", tmp.path());
    let (slow_name, mut slow) = gate_server_config("slow", &slow_marker);
    slow.transport = match slow.transport {
        McpTransport::Stdio {
            command,
            args,
            mut env,
        } => {
            env.insert("MCP_GATE_INITIALIZE_DELAY".into(), "0.7".into());
            McpTransport::Stdio { command, args, env }
        }
        _ => unreachable!(),
    };
    let (fast_name, fast) = gate_server_config("fast", &fast_marker);
    config.mcp_servers.insert(slow_name, slow);
    config.mcp_servers.insert(fast_name, fast);
    let manager = Arc::new(McpManager::default());
    let cancel = CancellationToken::new();
    let slow_task = tokio::spawn({
        let manager = manager.clone();
        let config = config.clone();
        let cancel = cancel.clone();
        async move { manager.tools("slow", &config, &cancel).await }
    });
    wait_for_marker(&slow_marker, 1).await;
    let fast_tools = tokio::time::timeout(
        Duration::from_millis(500),
        manager.tools("fast", &config, &cancel),
    )
    .await
    .expect("different servers must not share a connection gate")
    .unwrap();
    assert_eq!(fast_tools[0].spec.name, "mcp_fast__echo");
    assert_eq!(
        slow_task.await.unwrap().unwrap()[0].spec.name,
        "mcp_slow__echo"
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn mcp_concurrent_tools_for_one_server_spawn_once_and_share_tools() {
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("marker");
    let mut config = config("http://localhost:12345", tmp.path());
    let (name, mut server) = gate_server_config("shared", &marker);
    if let McpTransport::Stdio { env, .. } = &mut server.transport {
        env.insert("MCP_GATE_INITIALIZE_DELAY".into(), "0.2".into());
    }
    config.mcp_servers.insert(name, server);
    let manager = Arc::new(McpManager::default());
    let cancel = CancellationToken::new();
    let first = {
        let manager = manager.clone();
        let config = config.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move { manager.tools("shared", &config, &cancel).await })
    };
    let second = {
        let manager = manager.clone();
        let config = config.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move { manager.tools("shared", &config, &cancel).await })
    };
    let (first, second) = tokio::time::timeout(Duration::from_secs(2), async {
        (
            first.await.unwrap().unwrap(),
            second.await.unwrap().unwrap(),
        )
    })
    .await
    .unwrap();
    assert_eq!(marker_count(&marker), 1);
    assert_eq!(first[0].spec.name, second[0].spec.name);
    assert_eq!(first[0].original_name, second[0].original_name);
    manager.shutdown().await;
}

#[tokio::test]
async fn mcp_stop_waits_for_connect_and_next_call_creates_fresh_client() {
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("marker");
    let mut config = config("http://localhost:12345", tmp.path());
    let (name, mut server) = gate_server_config("stoppable", &marker);
    if let McpTransport::Stdio { env, .. } = &mut server.transport {
        env.insert("MCP_GATE_INITIALIZE_DELAY".into(), "0.3".into());
    }
    config.mcp_servers.insert(name, server);
    let manager = Arc::new(McpManager::default());
    let cancel = CancellationToken::new();
    let connecting = tokio::spawn({
        let manager = manager.clone();
        let config = config.clone();
        let cancel = cancel.clone();
        async move { manager.tools("stoppable", &config, &cancel).await }
    });
    wait_for_marker(&marker, 1).await;
    tokio::time::timeout(Duration::from_secs(2), manager.stop("stoppable"))
        .await
        .expect("stop must wait for the in-flight connection gate");
    assert!(connecting.await.unwrap().is_ok());
    let tools = tokio::time::timeout(
        Duration::from_secs(2),
        manager.tools("stoppable", &config, &cancel),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(tools[0].spec.name, "mcp_stoppable__echo");
    assert_eq!(marker_count(&marker), 2);
    manager.shutdown().await;
}

#[tokio::test]
async fn mcp_failed_connect_is_not_cached_and_retries() {
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("marker");
    let mut config = config("http://localhost:12345", tmp.path());
    let (name, mut server) = gate_server_config("retry", &marker);
    if let McpTransport::Stdio { env, .. } = &mut server.transport {
        env.insert("MCP_GATE_FAIL_INITIALIZE_COUNT".into(), "1".into());
    }
    config.mcp_servers.insert(name, server);
    let manager = Arc::new(McpManager::default());
    let cancel = CancellationToken::new();
    assert!(tokio::time::timeout(
        Duration::from_secs(2),
        manager.tools("retry", &config, &cancel),
    )
    .await
    .unwrap()
    .is_err());
    manager.stop("retry").await;
    let tools = tokio::time::timeout(
        Duration::from_secs(2),
        manager.tools("retry", &config, &cancel),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(tools[0].spec.name, "mcp_retry__echo");
    assert_eq!(marker_count(&marker), 2);
    manager.shutdown().await;
}

#[tokio::test]
async fn mcp_cancelled_connect_is_not_cached_and_immediate_retry_connects() {
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("marker");
    let mut config = config("http://localhost:12345", tmp.path());
    let (name, mut server) = gate_server_config("cancel-connect", &marker);
    if let McpTransport::Stdio { env, .. } = &mut server.transport {
        env.insert("MCP_GATE_INITIALIZE_DELAY".into(), "0.7".into());
    }
    config.mcp_servers.insert(name, server);
    let manager = Arc::new(McpManager::default());
    let cancelled = CancellationToken::new();
    let connect = tokio::spawn({
        let manager = manager.clone();
        let config = config.clone();
        let cancelled = cancelled.clone();
        async move { manager.tools("cancel-connect", &config, &cancelled).await }
    });
    wait_for_marker(&marker, 1).await;
    cancelled.cancel();
    assert!(connect.await.unwrap().is_err());

    let tools = tokio::time::timeout(
        Duration::from_secs(2),
        manager.tools("cancel-connect", &config, &CancellationToken::new()),
    )
    .await
    .expect("uncancelled retry must be bounded")
    .unwrap();
    assert_eq!(tools[0].spec.name, "mcp_cancel-connect__echo");
    assert_eq!(marker_count(&marker), 2);
    shutdown_mcp(&manager).await;
}

#[tokio::test]
async fn mcp_shutdown_rejects_new_connects_and_manager_reuses_afterward() {
    let tmp = tempfile::tempdir().unwrap();
    let slow_marker = tmp.path().join("slow.marker");
    let new_marker = tmp.path().join("new.marker");
    let mut config = config("http://localhost:12345", tmp.path());
    let (slow_name, mut slow) = gate_server_config("shutdown-slow", &slow_marker);
    if let McpTransport::Stdio { env, .. } = &mut slow.transport {
        env.insert("MCP_GATE_INITIALIZE_DELAY".into(), "0.7".into());
    }
    let (new_name, new_server) = gate_server_config("shutdown-new", &new_marker);
    config.mcp_servers.insert(slow_name, slow);
    config.mcp_servers.insert(new_name, new_server);
    let manager = Arc::new(McpManager::default());
    let slow_connect = tokio::spawn({
        let manager = manager.clone();
        let config = config.clone();
        async move {
            manager
                .tools("shutdown-slow", &config, &CancellationToken::new())
                .await
        }
    });
    wait_for_marker(&slow_marker, 1).await;

    let shutdown = tokio::spawn({
        let manager = manager.clone();
        async move { manager.shutdown().await }
    });
    tokio::time::sleep(Duration::from_millis(25)).await;
    let rejected = tokio::time::timeout(
        Duration::from_secs(2),
        manager.tools("shutdown-new", &config, &CancellationToken::new()),
    )
    .await
    .expect("new connection rejection must be bounded");
    let rejected = match rejected {
        Ok(_) => panic!("new connection must be rejected during shutdown"),
        Err(error) => error,
    };
    assert!(rejected.to_string().contains("shutting down"));
    assert_eq!(marker_count(&new_marker), 0);
    shutdown.await.unwrap();
    assert!(slow_connect.await.unwrap().is_ok());

    let tools = tokio::time::timeout(
        Duration::from_secs(2),
        manager.tools("shutdown-new", &config, &CancellationToken::new()),
    )
    .await
    .expect("manager must be reusable after shutdown")
    .unwrap();
    assert_eq!(tools[0].spec.name, "mcp_shutdown-new__echo");
    assert_eq!(marker_count(&new_marker), 1);
    shutdown_mcp(&manager).await;
}

#[tokio::test]
async fn mcp_failed_connect_is_cached_for_tools_calls_within_ttl() {
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("marker");
    let mut config = config("http://localhost:12345", tmp.path());
    let (name, mut server) = gate_server_config("cached", &marker);
    if let McpTransport::Stdio { env, .. } = &mut server.transport {
        env.insert("MCP_GATE_FAIL_INITIALIZE_COUNT".into(), "10".into());
    }
    config.mcp_servers.insert(name, server);
    let manager = McpManager::default();
    let cancel = CancellationToken::new();

    let first = match manager.tools("cached", &config, &cancel).await {
        Ok(_) => panic!("the first connection attempt should fail"),
        Err(error) => error.to_string(),
    };
    let second = match manager.tools("cached", &config, &cancel).await {
        Ok(_) => panic!("the cached connection failure should be returned"),
        Err(error) => error.to_string(),
    };

    assert_eq!(marker_count(&marker), 1);
    assert_eq!(first.to_string(), second.to_string());
    shutdown_mcp(&manager).await;
}

#[tokio::test]
async fn mcp_failed_connect_cache_is_independent_per_server() {
    let tmp = tempfile::tempdir().unwrap();
    let failed_marker = tmp.path().join("failed.marker");
    let healthy_marker = tmp.path().join("healthy.marker");
    let mut config = config("http://localhost:12345", tmp.path());
    let (failed_name, mut failed) = gate_server_config("failed", &failed_marker);
    if let McpTransport::Stdio { env, .. } = &mut failed.transport {
        env.insert("MCP_GATE_FAIL_INITIALIZE_COUNT".into(), "10".into());
    }
    let (healthy_name, healthy) = gate_server_config("healthy", &healthy_marker);
    config.mcp_servers.insert(failed_name, failed);
    config.mcp_servers.insert(healthy_name, healthy);
    let manager = McpManager::default();
    let cancel = CancellationToken::new();

    assert!(manager.tools("failed", &config, &cancel).await.is_err());
    let healthy_tools = manager.tools("healthy", &config, &cancel).await.unwrap();

    assert_eq!(healthy_tools[0].spec.name, "mcp_healthy__echo");
    assert_eq!(marker_count(&failed_marker), 1);
    assert_eq!(marker_count(&healthy_marker), 1);
    shutdown_mcp(&manager).await;
}

#[tokio::test]
async fn mcp_stop_clears_failed_connect_cache_for_immediate_retry() {
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("marker");
    let mut config = config("http://localhost:12345", tmp.path());
    let (name, mut server) = gate_server_config("restart", &marker);
    if let McpTransport::Stdio { env, .. } = &mut server.transport {
        env.insert("MCP_GATE_FAIL_INITIALIZE_COUNT".into(), "1".into());
    }
    config.mcp_servers.insert(name, server);
    let manager = McpManager::default();
    let cancel = CancellationToken::new();

    assert!(manager.tools("restart", &config, &cancel).await.is_err());
    assert!(manager.tools("restart", &config, &cancel).await.is_err());
    assert_eq!(marker_count(&marker), 1);

    manager.stop("restart").await;
    let tools = manager.tools("restart", &config, &cancel).await.unwrap();

    assert_eq!(tools[0].spec.name, "mcp_restart__echo");
    assert_eq!(marker_count(&marker), 2);
    shutdown_mcp(&manager).await;
}

#[tokio::test]
async fn mcp_failed_connect_retries_after_negative_cache_ttl() {
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("marker");
    let mut config = config("http://localhost:12345", tmp.path());
    let (name, mut server) = gate_server_config("expired", &marker);
    if let McpTransport::Stdio { env, .. } = &mut server.transport {
        env.insert("MCP_GATE_FAIL_INITIALIZE_COUNT".into(), "1".into());
    }
    config.mcp_servers.insert(name, server);
    let manager = McpManager::default();
    let cancel = CancellationToken::new();

    assert!(manager.tools("expired", &config, &cancel).await.is_err());
    assert!(manager.tools("expired", &config, &cancel).await.is_err());
    assert_eq!(marker_count(&marker), 1);

    tokio::time::sleep(Duration::from_millis(5_250)).await;
    let tools = tokio::time::timeout(
        Duration::from_secs(2),
        manager.tools("expired", &config, &cancel),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(tools[0].spec.name, "mcp_expired__echo");
    assert_eq!(marker_count(&marker), 2);
    shutdown_mcp(&manager).await;
}

#[tokio::test]
async fn mcp_call_failure_does_not_enter_connect_failure_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("marker");
    let mut config = config("http://localhost:12345", tmp.path());
    let (name, mut server) = gate_server_config("call-failure", &marker);
    if let McpTransport::Stdio { env, .. } = &mut server.transport {
        env.insert("MCP_GATE_FAIL_CALL_COUNT".into(), "1".into());
    }
    config.mcp_servers.insert(name, server);
    let manager = McpManager::default();
    let cancel = CancellationToken::new();
    let tools = manager
        .tools("call-failure", &config, &cancel)
        .await
        .unwrap();

    assert!(manager
        .call(&tools[0], json!({"text":"first"}), &config, &cancel)
        .await
        .is_err());
    let result = manager
        .call(&tools[0], json!({"text":"second"}), &config, &cancel)
        .await
        .unwrap();

    assert_eq!(result["content"][0]["text"], "second");
    assert_eq!(marker_count(&marker), 2);
    shutdown_mcp(&manager).await;
}
#[tokio::test]
async fn mcp_http_propagates_session_headers_and_calls_tools() {
    let _env_lock = PROXY_ENV_LOCK.lock().await;
    let mut initialize = Reply::json(
        json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}}}}),
    );
    initialize
        .headers
        .push(("Mcp-Session-Id".into(), "fixture-session".into()));
    let mut server = server(vec![initialize,Reply::json(json!({})),Reply::json(json!({"id":2,"result":{"tools":[{"name":"echo","description":"fixture","inputSchema":{"type":"object"}}]}})),Reply::json(json!({"id":3,"result":{"content":[{"type":"text","text":"success"}]}})),Reply::json(json!({}))]).await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config("http://localhost:12345", tmp.path());
    config.mcp_servers.insert(
        "remote".into(),
        McpConfig {
            uuid: "remote-id".into(),
            enabled: true,
            hitl: false,
            timeout_seconds: 5,
            allow_private_networks: true,
            network_access: false,
            transport: McpTransport::Http {
                url: server.url.clone(),
                headers: BTreeMap::new(),
            },
        },
    );
    let manager = McpManager::default();
    let cancel = CancellationToken::new();
    let tools = manager.tools("remote", &config, &cancel).await.unwrap();
    assert_eq!(
        manager
            .call(&tools[0], json!({}), &config, &cancel)
            .await
            .unwrap()["content"][0]["text"],
        "success"
    );
    server.requests.recv().await.unwrap();
    let initialized = server.requests.recv().await.unwrap();
    assert!(initialized
        .headers
        .to_ascii_lowercase()
        .contains("mcp-session-id: fixture-session"));
    assert!(initialized.body.contains("notifications/initialized"));
    manager.shutdown().await;
}

#[tokio::test]
async fn mcp_shutdown_sends_http_deletes_concurrently_and_only_once() {
    let _env_lock = PROXY_ENV_LOCK.lock().await;
    let delete_delay = Duration::from_millis(300);
    let mut delayed_delete_reply = Reply::json(json!({}));
    delayed_delete_reply.header_delay = Some(delete_delay);
    let mut first_initialize = Reply::json(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "protocolVersion": "2025-03-26",
            "capabilities": {"tools": {}}
        }
    }));
    first_initialize
        .headers
        .push(("Mcp-Session-Id".into(), "first-session".into()));
    let mut second_initialize = Reply::json(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "protocolVersion": "2025-03-26",
            "capabilities": {"tools": {}}
        }
    }));
    second_initialize
        .headers
        .push(("Mcp-Session-Id".into(), "second-session".into()));
    let mut first = server(vec![
        first_initialize,
        Reply::json(json!({})),
        Reply::json(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "first fixture",
                    "inputSchema": {"type": "object"}
                }]
            }
        })),
        delayed_delete_reply,
    ])
    .await;
    let mut second = server(vec![
        second_initialize,
        Reply::json(json!({})),
        Reply::json(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "second fixture",
                    "inputSchema": {"type": "object"}
                }]
            }
        })),
        {
            let mut reply = Reply::json(json!({}));
            reply.header_delay = Some(delete_delay);
            reply
        },
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config("http://localhost:12345", tmp.path());
    for (name, url) in [("first", first.url.clone()), ("second", second.url.clone())] {
        config.mcp_servers.insert(
            name.into(),
            McpConfig {
                uuid: format!("{name}-id"),
                enabled: true,
                hitl: false,
                timeout_seconds: 5,
                allow_private_networks: true,
                network_access: false,
                transport: McpTransport::Http {
                    url,
                    headers: BTreeMap::new(),
                },
            },
        );
    }
    let manager = Arc::new(McpManager::default());
    let cancel = CancellationToken::new();
    manager.tools("first", &config, &cancel).await.unwrap();
    manager.tools("second", &config, &cancel).await.unwrap();

    let started = Instant::now();
    let shutdown = tokio::spawn({
        let manager = manager.clone();
        async move { manager.shutdown().await }
    });
    let first_delete = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let request = first.requests.recv().await.expect("first server request");
            if request.headers.starts_with("DELETE ") {
                break request;
            }
        }
    })
    .await
    .expect("first DELETE must start within the bounded wait");
    let second_delete = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let request = second.requests.recv().await.expect("second server request");
            if request.headers.starts_with("DELETE ") {
                break request;
            }
        }
    })
    .await
    .expect("second DELETE must start within the bounded wait");
    assert!(first_delete.headers.starts_with("DELETE "));
    assert!(second_delete.headers.starts_with("DELETE "));
    assert!(
        !shutdown.is_finished(),
        "both DELETEs must overlap their delayed responses"
    );

    tokio::time::timeout(Duration::from_secs(2), shutdown)
        .await
        .expect("concurrent MCP shutdown must be bounded")
        .expect("shutdown task must not panic");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(200),
        "shutdown completed before the delayed DELETE response: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(550),
        "shutdown took closer to serial time than one delay: {elapsed:?}"
    );

    manager.shutdown().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), first.requests.recv())
            .await
            .is_err(),
        "first server must receive exactly one DELETE"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), second.requests.recv())
            .await
            .is_err(),
        "second server must receive exactly one DELETE"
    );
}

#[tokio::test]
async fn custom_http_templates_encode_urls_preserve_body_types_and_extract_json() {
    let mut server = server(vec![Reply::json(json!({"result":{"id":42}}))]).await;
    let tmp = tempfile::tempdir().unwrap();
    let config = config(&server.url, tmp.path());
    let key = format!("DIET_TEST_{}", uuid::Uuid::new_v4().simple());
    std::env::set_var(&key, "test-secret");
    let tool: ToolConfig = serde_json::from_value(json!({"type":"http","method":"POST","url":format!("{}/items/{{{{name}}}}",server.url),"description":"test","headers":{"Authorization":format!("Bearer ${{{key}}}")},"query":{"q":"{{name}}"},"body_template":{"payload":"{{payload}}"},"response_pointer":"/result/id","allow_private_networks":true,"timeout_seconds":5,"max_output_bytes":4096,"hitl":false,"destructive":false})).unwrap();
    let result = tools::custom(
        &tool,
        &json!({"name":"a/b c","payload":{"count":2}}),
        &config,
        false,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    std::env::remove_var(key);
    assert_eq!(result, 42);
    let request = server.requests.recv().await.unwrap();
    assert!(request
        .headers
        .starts_with("POST /items/a%2Fb%20c?q=a%2Fb+c"));
    assert!(request.headers.contains("Bearer test-secret"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&request.body).unwrap(),
        json!({"payload":{"count":2}})
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test]
async fn pinned_clients_ignore_environment_proxies_for_local_web_and_custom_http() {
    let _env_lock = PROXY_ENV_LOCK.lock().await;
    let mut server = server(vec![
        Reply {
            status: 200,
            content_type: "text/plain".into(),
            body: "direct web fetch".into(),
            headers: vec![],
            header_delay: None,
            chunk_delay: None,
            stall: None,
        },
        Reply::json(json!({"direct": true})),
    ])
    .await;
    let _proxy_env = ProxyEnvGuard::install("http://127.0.0.1:9");
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.web_fetch.allow_private_networks = true;

    let fetched =
        tools::web_fetch_with_config(&server.url, &CancellationToken::new(), Some(&config))
            .await
            .unwrap();
    assert_eq!(fetched["text"], "direct web fetch");

    let custom_tool: ToolConfig = serde_json::from_value(json!({
        "type": "http",
        "method": "GET",
        "url": server.url,
        "description": "proxy bypass probe",
        "allow_private_networks": true,
        "timeout_seconds": 5
    }))
    .unwrap();
    let response = tools::custom(
        &custom_tool,
        &json!({}),
        &config,
        false,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(response["status"], 200);
    assert_eq!(response["body"], r#"{"direct":true}"#);

    assert!(server.requests.recv().await.is_some());
    assert!(server.requests.recv().await.is_some());
}

#[tokio::test]
async fn custom_http_loopback_requires_opt_in_for_static_and_templated_hosts() {
    let mut server = server(vec![
        Reply::json(json!({"ok": "static"})),
        Reply::json(json!({"ok": "templated"})),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let port = server.url.rsplit(':').next().unwrap();
    let static_url = format!("http://127.0.0.1:{port}/static");
    let static_tool: ToolConfig = serde_json::from_value(json!({
        "type": "http",
        "method": "GET",
        "url": static_url,
        "description": "static loopback probe"
    }))
    .unwrap();
    let config = config("http://localhost:12345", tmp.path());
    let error = tools::custom(
        &static_tool,
        &json!({}),
        &config,
        false,
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("non-public"),
        "static loopback must be denied: {error}"
    );

    let static_allowed: ToolConfig = serde_json::from_value(json!({
        "type": "http",
        "method": "GET",
        "url": format!("http://127.0.0.1:{port}/static"),
        "description": "static loopback probe",
        "allow_private_networks": true,
        "timeout_seconds": 5
    }))
    .unwrap();
    let result = tools::custom(
        &static_allowed,
        &json!({}),
        &config,
        false,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(result["status"], 200);
    assert_eq!(result["body"], r#"{"ok":"static"}"#);

    let templated_tool: ToolConfig = serde_json::from_value(json!({
        "type": "http",
        "method": "GET",
        "url": format!("http://{{{{host}}}}:{port}/templated"),
        "description": "templated loopback probe",
        "timeout_seconds": 5,
        "max_output_bytes": 128
    }))
    .unwrap();
    let error = tools::custom(
        &templated_tool,
        &json!({"host": "localhost"}),
        &config,
        false,
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("non-public"),
        "templated loopback must be denied without opt-in: {error}"
    );
    let templated_allowed: ToolConfig = serde_json::from_value(json!({
        "type": "http",
        "method": "GET",
        "url": format!("http://{{{{host}}}}:{port}/templated"),
        "description": "templated loopback probe",
        "allow_private_networks": true,
        "timeout_seconds": 5,
        "max_output_bytes": 128
    }))
    .unwrap();
    let result = tools::custom(
        &templated_allowed,
        &json!({"host": "localhost"}),
        &config,
        false,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(result["status"], 200);
    assert_eq!(result["body"], r#"{"ok":"templated"}"#);
    assert!(!result["truncated"].as_bool().unwrap());
    let static_request = server.requests.recv().await.unwrap();
    assert!(static_request.headers.starts_with("GET /static "));
    let templated_request = server.requests.recv().await.unwrap();
    assert!(templated_request.headers.starts_with("GET /templated "));
}

#[tokio::test]
async fn custom_http_redirects_are_not_followed() {
    let mut server = server(vec![Reply {
        status: 302,
        content_type: "text/plain".into(),
        body: "redirected".into(),
        headers: vec![("Location".into(), "http://127.0.0.1:9/secret".into())],
        header_delay: None,
        chunk_delay: None,
        stall: None,
    }])
    .await;
    let tool: ToolConfig = serde_json::from_value(json!({
        "type": "http",
        "method": "GET",
        "url": server.url,
        "description": "redirect probe",
        "allow_private_networks": true
    }))
    .unwrap();
    let error = tools::custom(
        &tool,
        &json!({}),
        &config(
            "http://localhost:12345",
            tempfile::tempdir().unwrap().path(),
        ),
        false,
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("302"),
        "redirect must remain a response error: {error}"
    );
    server.requests.recv().await.unwrap();
    assert_eq!(server.count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn custom_http_returns_status_error_for_non_success_response() {
    let mut server = server(vec![Reply {
        status: 503,
        content_type: "text/plain".into(),
        body: "temporarily unavailable".into(),
        headers: vec![],
        header_delay: None,
        chunk_delay: None,
        stall: None,
    }])
    .await;
    let tool: ToolConfig = serde_json::from_value(json!({
        "type": "http",
        "method": "GET",
        "url": server.url,
        "description": "status failure probe",
        "allow_private_networks": true
    }))
    .unwrap();

    let error = tools::custom(
        &tool,
        &json!({}),
        &config(
            "http://localhost:12345",
            tempfile::tempdir().unwrap().path(),
        ),
        false,
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();

    assert_eq!(
        error.to_string(),
        "HTTP tool returned 503 Service Unavailable"
    );
    server.requests.recv().await.unwrap();
}

#[tokio::test]
async fn custom_http_rejects_truncated_response_for_json_pointer_extraction() {
    let mut server = server(vec![Reply::json(json!({
        "result": {"id": "value larger than the limit"}
    }))])
    .await;
    let tool: ToolConfig = serde_json::from_value(json!({
        "type": "http",
        "method": "GET",
        "url": server.url,
        "description": "bounded JSON extraction probe",
        "response_pointer": "/result/id",
        "allow_private_networks": true,
        "max_output_bytes": 8
    }))
    .unwrap();

    let error = tools::custom(
        &tool,
        &json!({}),
        &config(
            "http://localhost:12345",
            tempfile::tempdir().unwrap().path(),
        ),
        false,
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();

    assert_eq!(error.to_string(), "Response too large for JSON extraction");
    server.requests.recv().await.unwrap();
}

#[tokio::test]
async fn custom_http_reports_missing_json_response_pointer() {
    let mut server = server(vec![Reply::json(json!({"result": {"id": 42}}))]).await;
    let tool: ToolConfig = serde_json::from_value(json!({
        "type": "http",
        "method": "GET",
        "url": server.url,
        "description": "missing JSON pointer probe",
        "response_pointer": "/result/missing",
        "allow_private_networks": true
    }))
    .unwrap();

    let error = tools::custom(
        &tool,
        &json!({}),
        &config(
            "http://localhost:12345",
            tempfile::tempdir().unwrap().path(),
        ),
        false,
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();

    assert_eq!(
        error.to_string(),
        "Response pointer not found: /result/missing"
    );
    server.requests.recv().await.unwrap();
}

#[tokio::test]
async fn custom_http_reports_malformed_json_for_json_pointer_extraction() {
    let mut server = server(vec![Reply {
        status: 200,
        content_type: "application/json".into(),
        body: "{not valid json".into(),
        headers: vec![],
        header_delay: None,
        chunk_delay: None,
        stall: None,
    }])
    .await;
    let tool: ToolConfig = serde_json::from_value(json!({
        "type": "http",
        "method": "GET",
        "url": server.url,
        "description": "malformed JSON probe",
        "response_pointer": "/result/id",
        "allow_private_networks": true
    }))
    .unwrap();

    let error = tools::custom(
        &tool,
        &json!({}),
        &config(
            "http://localhost:12345",
            tempfile::tempdir().unwrap().path(),
        ),
        false,
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();

    assert!(
        error.to_string().contains("key must be a string")
            && error.to_string().contains("line 1 column 2"),
        "malformed JSON error should identify the parse failure: {error}"
    );
    server.requests.recv().await.unwrap();
}

#[tokio::test]
async fn custom_http_opt_in_preserves_timeout_and_output_limits() {
    let mut server = server(vec![Reply {
        status: 200,
        content_type: "text/plain".into(),
        body: "0123456789".into(),
        headers: vec![],
        header_delay: None,
        chunk_delay: None,
        stall: None,
    }])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let tool: ToolConfig = serde_json::from_value(json!({
        "type": "http",
        "method": "POST",
        "url": server.url,
        "description": "bounded HTTP tool",
        "text_body": "{{body}}",
        "allow_private_networks": true,
        "timeout_seconds": 5,
        "max_output_bytes": 4
    }))
    .unwrap();
    let result = tools::custom(
        &tool,
        &json!({"body": "request payload"}),
        &config("http://localhost:12345", tmp.path()),
        false,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(result["status"], 200);
    assert_eq!(result["body"], "0123");
    assert_eq!(result["truncated"], true);
    let request = server.requests.recv().await.unwrap();
    assert!(request.headers.starts_with("POST / "));
    assert_eq!(request.body, "request payload");
}
#[tokio::test]
async fn website_fetch_extraction_and_content_type_limits_use_local_server() {
    let server = server(vec![Reply { status:200, content_type:"text/html".into(),body:"<title>Local</title><article><p>Hello there</p><script>not readable</script></article>".into(),headers:vec![],header_delay:None,chunk_delay:None,stall:None },Reply { status:200,content_type:"application/octet-stream".into(),body:"binary".into(),headers:vec![],header_delay:None,chunk_delay:None,stall:None }]).await;
    // Local server lives on 127.0.0.1; the SSRF guard rejects loopback
    // destinations unless `web_fetch.allow_private_networks = true`.
    let mut config = Config::default();
    config.web_fetch.allow_private_networks = true;
    let value = diet_soda::tools::web_fetch_with_config(
        &server.url,
        &CancellationToken::new(),
        Some(&config),
    )
    .await
    .unwrap();
    assert_eq!(value["title"], "Local");
    assert_eq!(value["text"], "Hello there");
    assert!(diet_soda::tools::web_fetch_with_config(
        &server.url,
        &CancellationToken::new(),
        Some(&config),
    )
    .await
    .is_err());
    // `file://` is rejected at the URL schema layer (`validate_url` only
    // accepts http/https).
    assert!(
        diet_soda::tools::web_fetch("file:///tmp/no", &CancellationToken::new())
            .await
            .is_err()
    );
}
#[tokio::test]
async fn command_cancellation_and_output_caps_are_enforced() {
    let tmp = tempfile::tempdir().unwrap();
    let env = BTreeMap::new();
    let args = vec!["-c".into(), "sleep 30".into()];
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        trigger.cancel();
    });
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        process::run(
            ProcessRequest {
                command: "/bin/sh",
                args: &args,
                cwd: tmp.path(),
                env: &env,
                input: None,
                timeout: 20,
                limit: 100,
                network_access: false,
            },
            &cancel,
        ),
    )
    .await
    .unwrap();
    assert!(result.is_err());
    let result = process::run(
        ProcessRequest {
            command: "/bin/echo",
            args: &["1234567890".into()],
            cwd: tmp.path(),
            env: &env,
            input: None,
            timeout: 2,
            limit: 4,
            network_access: false,
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(result.stdout, "1234");
    assert!(result.truncated);
}
#[tokio::test]
async fn plugin_receives_json_events_and_can_deny_a_tool() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config("http://localhost:12345", tmp.path());
    config.hooks.push(HookConfig { event:"before_tool".into(),command:python_command().into(),args:vec!["-c".into(),"import json,sys; e=json.load(sys.stdin); assert e['event']=='before_tool'; print(json.dumps({'deny':'policy fixture'}))".into()],env:BTreeMap::new(),enabled:true,timeout_seconds:5, network_access:false });
    let error = hooks::emit(
        &config,
        "before_tool",
        json!({"tool":"example"}),
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("policy fixture"));
    config.hooks[0].enabled = false;
    hooks::emit(&config, "before_tool", json!({}), &CancellationToken::new())
        .await
        .unwrap();
}

#[tokio::test]
async fn lazy_hook_payload_is_not_built_without_a_matching_enabled_hook() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config("http://localhost:12345", tmp.path());
    let built = Arc::new(AtomicUsize::new(0));

    hooks::emit_lazy(
        &config,
        "before_tool",
        {
            let built = built.clone();
            move || {
                built.fetch_add(1, Ordering::SeqCst);
                json!({"case":"no hooks"})
            }
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(built.load(Ordering::SeqCst), 0);

    config.hooks.push(HookConfig {
        event: "before_tool".into(),
        command: python_command().into(),
        args: vec!["-c".into(), "import sys; sys.exit(0)".into()],
        env: BTreeMap::new(),
        enabled: false,
        timeout_seconds: 5,
        network_access: false,
    });
    hooks::emit_lazy(
        &config,
        "before_tool",
        {
            let built = built.clone();
            move || {
                built.fetch_add(1, Ordering::SeqCst);
                json!({"case":"disabled hook"})
            }
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(built.load(Ordering::SeqCst), 0);

    config.hooks[0].enabled = true;
    hooks::emit_lazy(
        &config,
        "after_tool",
        {
            let built = built.clone();
            move || {
                built.fetch_add(1, Ordering::SeqCst);
                json!({"case":"wrong event"})
            }
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(built.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn lazy_hook_payload_is_built_once_and_matching_hooks_receive_the_same_envelope() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config("http://localhost:12345", tmp.path());
    let expected = r#"{'version': 1, 'event': 'before_tool', 'payload': {'tool': 'read_file', 'path': 'notes/today.md'}}"#;
    let command = format!(
        "import json,sys; assert json.load(sys.stdin) == {expected}",
        expected = expected
    );
    for _ in 0..2 {
        config.hooks.push(HookConfig {
            event: "before_tool".into(),
            command: python_command().into(),
            args: vec!["-c".into(), command.clone()],
            env: BTreeMap::new(),
            enabled: true,
            timeout_seconds: 5,
            network_access: false,
        });
    }
    config.hooks.push(HookConfig {
        event: "after_tool".into(),
        command: python_command().into(),
        args: vec!["-c".into(), "import sys; sys.exit(9)".into()],
        env: BTreeMap::new(),
        enabled: true,
        timeout_seconds: 5,
        network_access: false,
    });
    let built = Arc::new(AtomicUsize::new(0));

    hooks::emit_lazy(
        &config,
        "before_tool",
        {
            let built = built.clone();
            move || {
                built.fetch_add(1, Ordering::SeqCst);
                json!({"tool":"read_file","path":"notes/today.md"})
            }
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(built.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn lazy_hook_preserves_deny_nonzero_and_timeout_failures() {
    let cases = [
        (
            "import json; print(json.dumps({'deny':'policy fixture'}))",
            5,
            "denied before_tool: policy fixture",
        ),
        ("import sys; sys.exit(7)", 5, "failed (exit Some(7))"),
        ("import time; time.sleep(5)", 1, "Command timed out"),
    ];

    for (script, timeout_seconds, expected_error) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = config("http://localhost:12345", tmp.path());
        config.hooks.push(HookConfig {
            event: "before_tool".into(),
            command: python_command().into(),
            args: vec!["-c".into(), script.into()],
            env: BTreeMap::new(),
            enabled: true,
            timeout_seconds,
            network_access: false,
        });
        let built = Arc::new(AtomicUsize::new(0));
        let error = hooks::emit_lazy(
            &config,
            "before_tool",
            {
                let built = built.clone();
                move || {
                    built.fetch_add(1, Ordering::SeqCst);
                    json!({"tool":"example"})
                }
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();

        assert_eq!(built.load(Ordering::SeqCst), 1);
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains(expected_error),
            "expected {expected_error:?}, got {rendered:?}"
        );
    }
}
#[tokio::test]
async fn skill_installation_discovery_activation_and_duplicate_rejection() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(
        source.join("SKILL.md"),
        "---\nname: test-skill\ndescription: Test skill\n---\nUseful instructions.",
    )
    .unwrap();
    let config = config("http://localhost:12345", tmp.path());
    skills::install(source.to_str().unwrap(), &config.skills_dir)
        .await
        .unwrap();
    assert_eq!(skills::discover(&config).unwrap()[0].name, "test-skill");
    assert!(!skills::instructions(&config, &[])
        .unwrap()
        .contains("Useful instructions."));
    assert!(skills::instructions(&config, &["test-skill".into()])
        .unwrap()
        .contains("Useful instructions."));
    assert!(
        skills::install(source.to_str().unwrap(), &config.skills_dir)
            .await
            .is_err()
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/etc/passwd", source.join("link")).unwrap();
        assert!(
            skills::install(source.to_str().unwrap(), &tmp.path().join("other"))
                .await
                .is_err()
        );
    }
}
#[tokio::test]
async fn skill_archives_reject_links() {
    let tmp = tempfile::tempdir().unwrap();
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut archive = tar::Builder::new(encoder);
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o644);
    header.set_cksum();
    archive
        .append_link(&mut header, "escape", "/etc/passwd")
        .unwrap();
    let bytes = archive.into_inner().unwrap().finish().unwrap();
    let path = tmp.path().join("skill.tar.gz");
    std::fs::write(&path, bytes).unwrap();
    assert!(
        skills::install(path.to_str().unwrap(), &tmp.path().join("installed"))
            .await
            .is_err()
    );
}

#[tokio::test]
#[cfg(unix)]
async fn installed_skill_files_and_directories_are_owner_only() {
    let tmp = tempfile::tempdir().unwrap();
    let config = config("http://localhost:12345", tmp.path());

    let source = tmp.path().join("source");
    std::fs::create_dir_all(source.join("nested")).unwrap();
    std::fs::write(
        source.join("SKILL.md"),
        "---\nname: copied-skill\ndescription: Test\n---\nInstructions.",
    )
    .unwrap();
    std::fs::write(source.join("nested/data.txt"), "data").unwrap();
    skills::install(source.to_str().unwrap(), &config.skills_dir)
        .await
        .unwrap();

    let downloaded = tmp.path().join("downloaded.md");
    std::fs::write(
        &downloaded,
        "---\nname: downloaded-skill\ndescription: Test\n---\nInstructions.",
    )
    .unwrap();
    skills::install(downloaded.to_str().unwrap(), &config.skills_dir)
        .await
        .unwrap();

    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut archive = tar::Builder::new(encoder);
    let mut header = tar::Header::new_gnu();
    let body = b"---\nname: tar-skill\ndescription: Test\n---\nInstructions.";
    header.set_path("tar-skill/SKILL.md").unwrap();
    header.set_size(body.len() as u64);
    header.set_mode(0o777);
    header.set_cksum();
    archive.append(&header, &body[..]).unwrap();
    let bytes = archive.into_inner().unwrap().finish().unwrap();
    let tar_path = tmp.path().join("skill.tar.gz");
    std::fs::write(&tar_path, bytes).unwrap();
    skills::install(tar_path.to_str().unwrap(), &config.skills_dir)
        .await
        .unwrap();

    for skill in ["copied-skill", "downloaded-skill", "tar-skill"] {
        let root = config.skills_dir.join(skill);
        assert_eq!(root.metadata().unwrap().permissions().mode() & 0o077, 0);
        assert_eq!(
            root.join("SKILL.md")
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0
        );
    }
    assert_eq!(
        config
            .skills_dir
            .join("copied-skill/nested")
            .metadata()
            .unwrap()
            .permissions()
            .mode()
            & 0o077,
        0
    );
    assert_eq!(
        config
            .skills_dir
            .join("copied-skill/nested/data.txt")
            .metadata()
            .unwrap()
            .permissions()
            .mode()
            & 0o077,
        0
    );
}
