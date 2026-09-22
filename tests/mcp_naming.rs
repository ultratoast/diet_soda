//! MCP exposed-name fallback tests. These use the local stdio fixture so name
//! generation is tested through the public discovery API.
use diet_soda::{
    config::{Config, McpConfig, McpTransport},
    mcp::McpManager,
};
use serde_json::json;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

fn tool(name: &str) -> serde_json::Value {
    json!({
        "name": name,
        "description": "naming fixture",
        "inputSchema": {"type": "object"}
    })
}

async fn discover(server: &str, names: &[&str], page_size: Option<usize>) -> Vec<String> {
    let tmp = tempdir().unwrap();
    let mut config = Config {
        workspace: tmp.path().into(),
        ..Config::default()
    };
    config.mcp_servers.insert(
        server.into(),
        McpConfig {
            uuid: "naming-fixture-uuid".into(),
            enabled: true,
            hitl: false,
            timeout_seconds: 5,
            transport: McpTransport::Stdio {
                command: "python3".into(),
                args: vec![
                    format!(
                        "{}/tests/fixtures/mcp_naming_server.py",
                        env!("CARGO_MANIFEST_DIR")
                    ),
                    serde_json::to_string(&names.iter().map(|name| tool(name)).collect::<Vec<_>>())
                        .unwrap(),
                    page_size.unwrap_or(0).to_string(),
                ],
                env: Default::default(),
            },
        },
    );
    let tools = McpManager::default()
        .tools(server, &config, &CancellationToken::new())
        .await
        .unwrap();
    tools.into_iter().map(|tool| tool.spec.name).collect()
}

#[tokio::test]
async fn readable_mcp_name_is_unchanged() {
    let names = discover("fixture", &["echo"], None).await;
    assert_eq!(names, ["mcp_fixture__echo"]);
}

#[tokio::test]
async fn long_and_invalid_original_names_get_valid_bounded_deterministic_names() {
    let original = "tool name/with invalid punctuation and a name that is deliberately very long";
    let first = discover("fixture", &[original], None).await;
    let second = discover("fixture", &[original], None).await;
    assert_eq!(first, second);
    let exposed = &first[0];
    assert!(exposed.len() <= 64);
    assert!(exposed
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'));
}

#[tokio::test]
async fn invalid_server_names_use_ascii_bounded_deterministic_fallbacks() {
    let servers = ["日本語サーバー", "server name with spaces", "🔥💥☠️"];

    for server in servers {
        let first = discover(server, &["tool name/with invalid punctuation"], None).await;
        let second = discover(server, &["tool name/with invalid punctuation"], None).await;
        assert_eq!(first, second);
        let exposed = &first[0];
        assert!(exposed.len() <= 64);
        assert!(exposed
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'));
    }
}

#[tokio::test]
async fn fallback_hash_remains_tied_to_original_tool_name() {
    let names = discover("server name", &["tool name", "tool/name"], None).await;
    assert_ne!(names[0], names[1]);
    assert!(names.iter().all(|name| name.len() <= 64));
    assert!(names.iter().all(|name| {
        name.bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    }));
}

#[tokio::test]
async fn fallback_name_is_independent_of_list_order_and_pagination() {
    let names = [
        "tool name/with invalid punctuation and a name that is deliberately very long",
        "another tool name that is also too long to remain readable",
    ];
    let ordered = discover("fixture", &names, Some(1)).await;
    let reordered = discover("fixture", &[names[1], names[0]], None).await;
    assert_eq!(ordered[0], reordered[1]);
    assert_eq!(ordered[1], reordered[0]);
}

#[tokio::test]
async fn distinct_original_names_remain_distinct() {
    let names = discover(
        "fixture",
        &[
            "first invalid tool name that is long enough for fallback",
            "second invalid tool name that is long enough for fallback",
        ],
        None,
    )
    .await;
    assert_ne!(names[0], names[1]);
}

#[tokio::test]
async fn fallback_truncates_long_server_component_to_fit_limit() {
    let server = "server-name-that-is-long-enough-to-require-component-truncation";
    let names = discover(server, &["tool name/that is invalid"], None).await;
    let exposed = &names[0];
    assert!(exposed.len() <= 64);
    assert!(exposed.starts_with("mcp_server-name-that-is-long-enough-to-require-"));
}

#[tokio::test]
#[should_panic(expected = "MCP tool name collision")]
async fn duplicate_exposed_names_fail_closed() {
    let _ = discover("fixture", &["echo", "echo"], None).await;
}
