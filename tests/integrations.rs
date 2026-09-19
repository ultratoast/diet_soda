mod support;
use diet_soda::{
    config::{HookConfig, McpConfig, McpTransport, ToolConfig},
    hooks,
    mcp::McpManager,
    process::{self, ProcessRequest},
    skills, tools,
};
use serde_json::json;
use std::{collections::BTreeMap, time::Duration};
use support::*;
use tokio_util::sync::CancellationToken;

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
            transport: McpTransport::Stdio {
                command: "python3".into(),
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
async fn mcp_http_propagates_session_headers_and_calls_tools() {
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
async fn custom_http_templates_encode_urls_preserve_body_types_and_extract_json() {
    let mut server = server(vec![Reply::json(json!({"result":{"id":42}}))]).await;
    let tmp = tempfile::tempdir().unwrap();
    let config = config(&server.url, tmp.path());
    let key = format!("DIET_TEST_{}", uuid::Uuid::new_v4().simple());
    std::env::set_var(&key, "test-secret");
    let tool: ToolConfig = serde_json::from_value(json!({"type":"http","method":"POST","url":format!("{}/items/{{{{name}}}}",server.url),"description":"test","headers":{"Authorization":format!("Bearer ${{{key}}}")},"query":{"q":"{{name}}"},"body_template":{"payload":"{{payload}}"},"response_pointer":"/result/id","hitl":false,"destructive":false})).unwrap();
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
#[tokio::test]
async fn website_fetch_extraction_and_content_type_limits_use_local_server() {
    let server = server(vec![Reply { status:200, content_type:"text/html".into(),body:"<title>Local</title><article><p>Hello there</p><script>not readable</script></article>".into(),headers:vec![] },Reply { status:200,content_type:"application/octet-stream".into(),body:"binary".into(),headers:vec![] }]).await;
    let value = tools::web_fetch(&server.url, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(value["title"], "Local");
    assert_eq!(value["text"], "Hello there");
    assert!(tools::web_fetch(&server.url, &CancellationToken::new())
        .await
        .is_err());
    assert!(
        tools::web_fetch("file:///tmp/no", &CancellationToken::new())
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
    config.hooks.push(HookConfig { event:"before_tool".into(),command:"python3".into(),args:vec!["-c".into(),"import json,sys; e=json.load(sys.stdin); assert e['event']=='before_tool'; print(json.dumps({'deny':'policy fixture'}))".into()],env:BTreeMap::new(),enabled:true,timeout_seconds:5 });
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
