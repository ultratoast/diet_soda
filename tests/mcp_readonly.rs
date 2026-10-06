//! MCP read-only classification tests. Deterministic, local, no network.
//!  - Group A: direct unit tests of `mcp::classify_read_only` pinning the
//!    fail-closed ladder: server override > mutating name veto >
//!    `destructiveHint` > `readOnlyHint` > name heuristic (leading read verb).
//!  - Group B: `McpTool.read_only` recorded at `tools/list` discovery time via
//!    the local stdio naming fixture, mixing annotation and name cases.
//!  - Group C: engine advertisement withholds edit-capable MCP tools from a
//!    `can_edit: false` agent and reports the withheld tools in a
//!    `UiEvent::Status`.
mod support;

use diet_soda::{
    config::{Config, McpConfig, McpTransport},
    engine::Selection,
    mcp::{classify_read_only, McpManager},
    model::UiEvent,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use support::*;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

/// Server config for direct classification tests. `transport` is never used
/// by [`classify_read_only`], so a stub stdio transport suffices.
fn classify_server(read_only: Option<bool>) -> McpConfig {
    McpConfig {
        uuid: "classify-uuid".into(),
        enabled: true,
        hitl: false,
        read_only,
        timeout_seconds: 5,
        allow_private_networks: true,
        network_access: false,
        transport: McpTransport::Stdio {
            command: "python3".into(),
            args: vec![],
            env: Default::default(),
        },
    }
}

/// Tool descriptor in the shape `tools/list` advertises: `name`,
/// `description`, and `inputSchema`, with optional server self-reported
/// `annotations`.
fn discovery_tool(name: &str, annotations: Option<Value>) -> Value {
    let mut tool = json!({
        "name": name,
        "description": "read-only classification fixture",
        "inputSchema": {"type": "object"},
    });
    if let Some(annotations) = annotations {
        tool["annotations"] = annotations;
    }
    tool
}

/// Stdio transport running the naming fixture with the given tool list and a
/// page size that returns everything in one `tools/list` page.
fn naming_fixture_transport(tools: &[Value]) -> McpTransport {
    McpTransport::Stdio {
        command: "python3".into(),
        args: vec![
            format!(
                "{}/tests/fixtures/mcp_naming_server.py",
                env!("CARGO_MANIFEST_DIR")
            ),
            serde_json::to_string(tools).expect("tool list serializes"),
            "50".into(),
        ],
        env: Default::default(),
    }
}

// ---------------------------------------------------------------------------
// Group A: direct unit tests of `classify_read_only`.
// ---------------------------------------------------------------------------

#[test]
fn server_read_only_override_beats_annotations() {
    let forced_read_only = classify_read_only(
        &classify_server(Some(true)),
        &json!({"annotations": {"destructiveHint": true}}),
        "create_issue",
    );
    assert!(
        forced_read_only,
        "read_only: Some(true) must beat destructiveHint: true; observed {forced_read_only}"
    );

    let forced_edit = classify_read_only(
        &classify_server(Some(false)),
        &json!({"annotations": {"readOnlyHint": true}}),
        "list_issues",
    );
    assert!(
        !forced_edit,
        "read_only: Some(false) must beat readOnlyHint: true; observed {forced_edit}"
    );
}

#[test]
fn server_read_only_override_is_total() {
    // The operator override short-circuits the whole ladder, so it is total:
    // every tool of a forced server takes `read_only` alone, whatever its name
    // or annotations claim.
    let forced_read_only = classify_read_only(
        &classify_server(Some(true)),
        &json!({"annotations": {"destructiveHint": true}}),
        "delete_repository",
    );
    assert!(
        forced_read_only,
        "read_only: Some(true) must classify even `delete_repository` (destructiveHint: true) read-only; observed {forced_read_only}"
    );

    let forced_edit = classify_read_only(
        &classify_server(Some(false)),
        &json!({"annotations": {"readOnlyHint": true}}),
        "list_items",
    );
    assert!(
        !forced_edit,
        "read_only: Some(false) must classify even `list_items` (readOnlyHint: true) edit-capable; observed {forced_edit}"
    );
}

#[test]
fn destructive_hint_beats_read_only_hint() {
    // A self-contradictory tool must fail closed, so `destructiveHint` is
    // checked before `readOnlyHint` even on a read-verb name.
    let classified = classify_read_only(
        &classify_server(None),
        &json!({"annotations": {"destructiveHint": true, "readOnlyHint": true}}),
        "list_issues",
    );
    assert!(
        !classified,
        "destructiveHint: true must beat readOnlyHint: true; observed {classified}"
    );
}

#[test]
fn mutating_name_vetoes_read_only_hint_annotation() {
    // A mutating verb in the name vetoes a server's `readOnlyHint: true`, so
    // a lying server cannot smuggle a mutating tool past the classifier.
    let classified = classify_read_only(
        &classify_server(None),
        &json!({"annotations": {"readOnlyHint": true}}),
        "create_issue",
    );
    assert!(
        !classified,
        "mutating name `create_issue` must veto readOnlyHint: true and be edit-capable; observed {classified}"
    );
}

#[test]
fn leading_read_verbs_without_annotations_are_read_only() {
    for name in ["get_file_contents", "list_issues", "search_repositories"] {
        let classified = classify_read_only(&classify_server(None), &json!({}), name);
        assert!(
            classified,
            "name `{name}` starts with a read verb and has no annotations, must be read-only; observed {classified}"
        );
    }
}

#[test]
fn mutating_verbs_anywhere_in_name_are_edit_capable() {
    // `list_delete_queue` proves a mutating word beats a leading read word:
    // classification scans every word for mutators before the leading-word
    // read check.
    for name in ["create_issue", "search_and_replace", "list_delete_queue"] {
        let classified = classify_read_only(&classify_server(None), &json!({}), name);
        assert!(
            !classified,
            "name `{name}` contains a mutating verb and must be edit-capable; observed {classified}"
        );
    }
}

#[test]
fn adversarial_names_fail_closed_as_edit_capable() {
    // Previously fail-open names: each must classify edit-capable even though
    // a lying annotation or a glued-up name once let it through.
    let lying_hint = json!({"annotations": {"readOnlyHint": true}});

    // `create_issue`: the mutating `create` vetoes the lying readOnlyHint.
    let create_issue = classify_read_only(&classify_server(None), &lying_hint, "create_issue");
    assert!(
        !create_issue,
        "`create_issue` with a lying readOnlyHint: true must be edit-capable; observed {create_issue}"
    );

    // `delete_repository`: the mutating `delete` vetoes the lying readOnlyHint.
    let delete_repository =
        classify_read_only(&classify_server(None), &lying_hint, "delete_repository");
    assert!(
        !delete_repository,
        "`delete_repository` with a lying readOnlyHint: true must be edit-capable; observed {delete_repository}"
    );

    for (name, reason) in [
        // `get_2Delete`: digits end the current word, so `delete` stands alone.
        (
            "get_2Delete",
            "digits end the current word, exposing the mutating `delete`",
        ),
        // `getHTTPPost`: the acronym split exposes `post` as its own word.
        (
            "getHTTPPost",
            "the acronym split exposes the mutating `post`",
        ),
        // `resolve_issue`: `resolve` is not a read verb, so it fails closed.
        ("resolve_issue", "`resolve` is not a leading read verb"),
        // `report_bug`: `report` is not a read verb, so it fails closed.
        ("report_bug", "`report` is not a leading read verb"),
        // `check_out`: `check` is not a read verb, so it fails closed.
        ("check_out", "`check` is not a leading read verb"),
        // `search_and_clone`: `clone` vetoes the leading read verb `search`.
        (
            "search_and_clone",
            "mutating `clone` appears after the leading read verb",
        ),
        // `fetch_and_rebase`: `rebase` vetoes the leading read verb `fetch`.
        (
            "fetch_and_rebase",
            "mutating `rebase` appears after the leading read verb",
        ),
        // `get_and_destroy`: `destroy` vetoes the leading read verb `get`.
        (
            "get_and_destroy",
            "mutating `destroy` appears after the leading read verb",
        ),
    ] {
        let classified = classify_read_only(&classify_server(None), &json!({}), name);
        assert!(
            !classified,
            "name `{name}` must fail closed as edit-capable ({reason}); observed {classified}"
        );
    }
}

#[test]
fn splitter_does_not_over_split_read_names() {
    // The splitter must not carve a read name into fragments where the leading
    // read verb or a would-be mutating word could be misread.
    for name in ["getHTTPResponse", "listIssues", "get_thing"] {
        let classified = classify_read_only(&classify_server(None), &json!({}), name);
        assert!(
            classified,
            "read name `{name}` must stay read-only after splitting; observed {classified}"
        );
    }
}

#[test]
fn camel_case_tool_names_split_into_words_for_classification() {
    for name in ["listIssues", "getFileContents"] {
        let classified = classify_read_only(&classify_server(None), &json!({}), name);
        assert!(
            classified,
            "camelCase name `{name}` must split into words with a leading read verb; observed {classified}"
        );
    }
}

#[test]
fn unrecognized_tool_names_fail_closed_as_edit_capable() {
    for name in ["echo", "ping"] {
        let classified = classify_read_only(&classify_server(None), &json!({}), name);
        assert!(
            !classified,
            "unrecognized name `{name}` must fail closed as edit-capable; observed {classified}"
        );
    }
}

#[test]
fn only_the_leading_word_must_be_a_read_verb() {
    // `list` is a read verb but appears second; only the leading word counts.
    let classified = classify_read_only(&classify_server(None), &json!({}), "issues_list");
    assert!(
        !classified,
        "trailing read verb in `issues_list` must not mark the tool read-only; observed {classified}"
    );
}

#[test]
fn explicit_false_read_only_hint_falls_back_to_name_heuristic() {
    // `readOnlyHint: false` is the protocol default and indistinguishable
    // from absent, so the name heuristic still applies.
    let classified = classify_read_only(
        &classify_server(None),
        &json!({"annotations": {"readOnlyHint": false}}),
        "list_issues",
    );
    assert!(
        classified,
        "readOnlyHint: false with a leading read-verb name must fall back to the name heuristic; observed {classified}"
    );
}

// ---------------------------------------------------------------------------
// Group B: classification through real MCP discovery.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn discovery_records_read_only_flag_per_tool() {
    let tmp = tempdir().unwrap();
    let mut config = Config {
        workspace: tmp.path().into(),
        ..Config::default()
    };
    let advertised = vec![
        discovery_tool("list_items", None),
        discovery_tool("delete_item", None),
        discovery_tool("get_thing", Some(json!({"readOnlyHint": true}))),
        discovery_tool("read_report", Some(json!({"destructiveHint": true}))),
    ];
    config.mcp_servers.insert(
        "fixture".into(),
        McpConfig {
            uuid: "naming-fixture-uuid".into(),
            enabled: true,
            hitl: false,
            read_only: None,
            timeout_seconds: 5,
            allow_private_networks: true,
            network_access: false,
            transport: naming_fixture_transport(&advertised),
        },
    );
    let tools = McpManager::default()
        .tools("fixture", &config, &CancellationToken::new())
        .await
        .expect("stdio naming fixture should discover tools");
    let observed: BTreeMap<String, bool> = tools
        .iter()
        .map(|tool| (tool.original_name.clone(), tool.read_only))
        .collect();
    assert_eq!(
        observed.len(),
        4,
        "all four advertised tools should be discovered; observed: {observed:?}"
    );
    assert_eq!(
        observed.get("list_items"),
        Some(&true),
        "`list_items` (no annotations, leading read verb) must be read-only; observed: {observed:?}"
    );
    assert_eq!(
        observed.get("delete_item"),
        Some(&false),
        "`delete_item` (no annotations, mutating verb) must be edit-capable; observed: {observed:?}"
    );
    assert_eq!(
        observed.get("get_thing"),
        Some(&true),
        "`get_thing` with readOnlyHint: true must be read-only; observed: {observed:?}"
    );
    assert_eq!(
        observed.get("read_report"),
        Some(&false),
        "`read_report` with destructiveHint: true must be edit-capable; observed: {observed:?}"
    );
}

#[tokio::test]
async fn discovery_classifies_unannotated_names_by_name_only() {
    // With no annotations at all, discovery must classify purely by name:
    // leading read verbs read-only, a mutating verb edit-capable.
    let tmp = tempdir().unwrap();
    let mut config = Config {
        workspace: tmp.path().into(),
        ..Config::default()
    };
    let advertised = vec![
        discovery_tool("list_items", None),
        discovery_tool("get_thing", None),
        discovery_tool("delete_item", None),
    ];
    config.mcp_servers.insert(
        "plain".into(),
        McpConfig {
            uuid: "plain-fixture-uuid".into(),
            enabled: true,
            hitl: false,
            read_only: None,
            timeout_seconds: 5,
            allow_private_networks: true,
            network_access: false,
            transport: naming_fixture_transport(&advertised),
        },
    );
    let tools = McpManager::default()
        .tools("plain", &config, &CancellationToken::new())
        .await
        .expect("stdio naming fixture should discover tools");
    let observed: BTreeMap<String, bool> = tools
        .iter()
        .map(|tool| (tool.original_name.clone(), tool.read_only))
        .collect();
    assert_eq!(
        observed.len(),
        3,
        "all three advertised tools should be discovered; observed: {observed:?}"
    );
    assert_eq!(
        observed.get("list_items"),
        Some(&true),
        "`list_items` (no annotations, leading read verb) must be read-only; observed: {observed:?}"
    );
    assert_eq!(
        observed.get("get_thing"),
        Some(&true),
        "`get_thing` (no annotations, leading read verb) must be read-only; observed: {observed:?}"
    );
    assert_eq!(
        observed.get("delete_item"),
        Some(&false),
        "`delete_item` (no annotations, mutating verb) must be edit-capable; observed: {observed:?}"
    );
}

// ---------------------------------------------------------------------------
// Group C: engine-level gating of an advertisement.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_only_agent_advertisement_withholds_edit_capable_mcp_tools() {
    let tmp = tempdir().unwrap();
    let mut config = config("http://127.0.0.1:1", tmp.path());
    let advertised = vec![
        discovery_tool("list_items", None),
        discovery_tool("delete_item", None),
    ];
    config.mcp_servers.insert(
        "naming".into(),
        McpConfig {
            uuid: "naming-uuid".into(),
            enabled: true,
            hitl: false,
            read_only: None,
            timeout_seconds: 5,
            allow_private_networks: true,
            network_access: false,
            transport: naming_fixture_transport(&advertised),
        },
    );
    // `can_edit` is omitted so it defaults to false: the agent is read-only.
    config.agents.insert(
        "reader".into(),
        serde_json::from_value(json!({
            "tools": ["shell", "read_file"],
            "mcp_servers": ["naming-uuid"]
        }))
        .expect("agent config should parse"),
    );
    let (engine, mut events) = engine(config);
    let tools = engine
        .list_tools(
            &Selection {
                agent: Some("reader".into()),
                ..Selection::default()
            },
            &CancellationToken::new(),
        )
        .await
        .expect("listing tools for the reader agent should succeed");
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert!(
        names.contains(&"mcp_naming__list_items"),
        "read-only tool mcp_naming__list_items must be advertised to the read-only agent; got: {names:?}"
    );
    assert!(
        !names.contains(&"mcp_naming__delete_item"),
        "edit-capable tool mcp_naming__delete_item must be withheld from the read-only agent; got: {names:?}"
    );

    // The advertisement path reports withheld tools synchronously before
    // `list_tools` returns, so draining with `try_recv` cannot race the send.
    let mut statuses = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let UiEvent::Status { text, .. } = event {
            statuses.push(text);
        }
    }
    assert!(
        statuses
            .iter()
            .any(|text| text.contains("mcp_naming__delete_item") && text.contains("hidden")),
        "expected a UiEvent::Status reporting mcp_naming__delete_item as hidden; observed statuses: {statuses:?}"
    );
}

#[tokio::test]
async fn forced_editable_server_withholds_read_named_tools_from_read_only_agent() {
    // `read_only: Some(false)` is an operator assertion that every tool of the
    // server is edit-capable, so even read-verb names must be withheld from a
    // read-only agent. The `locked` server is separate from `naming` above so
    // the two cases cannot interfere.
    let tmp = tempdir().unwrap();
    let mut config = config("http://127.0.0.1:1", tmp.path());
    let advertised = vec![
        discovery_tool("list_items", None),
        discovery_tool("get_thing", None),
    ];
    config.mcp_servers.insert(
        "locked".into(),
        McpConfig {
            uuid: "locked-uuid".into(),
            enabled: true,
            hitl: false,
            read_only: Some(false),
            timeout_seconds: 5,
            allow_private_networks: true,
            network_access: false,
            transport: naming_fixture_transport(&advertised),
        },
    );
    // `can_edit` is omitted so it defaults to false: the agent is read-only.
    config.agents.insert(
        "reader".into(),
        serde_json::from_value(json!({
            "tools": ["shell", "read_file"],
            "mcp_servers": ["locked-uuid"]
        }))
        .expect("agent config should parse"),
    );
    let (engine, mut events) = engine(config);
    let tools = engine
        .list_tools(
            &Selection {
                agent: Some("reader".into()),
                ..Selection::default()
            },
            &CancellationToken::new(),
        )
        .await
        .expect("listing tools for the reader agent should succeed");
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert!(
        !names.contains(&"mcp_locked__list_items"),
        "mcp_locked__list_items must be withheld despite its read name because the server forces read_only: false; got: {names:?}"
    );
    assert!(
        !names.contains(&"mcp_locked__get_thing"),
        "mcp_locked__get_thing must be withheld despite its read name because the server forces read_only: false; got: {names:?}"
    );

    // The advertisement path reports withheld tools synchronously before
    // `list_tools` returns, so draining with `try_recv` cannot race the send.
    let mut statuses = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let UiEvent::Status { text, .. } = event {
            statuses.push(text);
        }
    }
    assert!(
        statuses.iter().any(|text| {
            text.contains("hidden")
                && (text.contains("mcp_locked__list_items")
                    || text.contains("mcp_locked__get_thing"))
        }),
        "expected a UiEvent::Status reporting at least one withheld mcp_locked__ tool as hidden; observed statuses: {statuses:?}"
    );
}
