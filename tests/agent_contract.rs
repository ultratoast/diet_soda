mod support;

use diet_soda::{
    config::{AgentConfig, Config},
    engine::Selection,
};
use serde_json::{json, Value};
use std::time::Duration;
use support::*;
use tokio_util::sync::CancellationToken;

fn parent_scope_config(url: &str, directory: &std::path::Path) -> Config {
    let mut config = config(url, directory);
    config.agents.insert(
        "parent".into(),
        AgentConfig {
            tools: Some(vec![
                "delegate".into(),
                "web_fetch".into(),
                "read_file".into(),
                "load_skill".into(),
                "shell".into(),
            ]),
            mcp_servers: Some(vec!["parent-mcp".into()]),
            ..AgentConfig::default()
        },
    );
    config
}

#[tokio::test]
async fn child_turn_budget_defaults_to_twenty_five_and_honors_lower_override() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = parent_scope_config("http://127.0.0.1:1", tmp.path());
    config
        .agents
        .insert("default-child".into(), AgentConfig::default());
    config.agents.insert(
        "short-child".into(),
        AgentConfig {
            max_turns: Some(7),
            ..AgentConfig::default()
        },
    );
    let (engine, _) = engine(config);
    let parent = engine
        .scope(
            &Selection {
                agent: Some("parent".into()),
                ..Selection::default()
            },
            "parent",
            None,
        )
        .await
        .unwrap();

    let default_child = engine
        .scope(
            &Selection {
                agent: Some("default-child".into()),
                ..Selection::default()
            },
            "default child",
            Some(&parent),
        )
        .await
        .unwrap();
    let short_child = engine
        .scope(
            &Selection {
                agent: Some("short-child".into()),
                ..Selection::default()
            },
            "short child",
            Some(&parent),
        )
        .await
        .unwrap();

    assert_eq!(default_child.max_turns, Some(25));
    assert_eq!(short_child.max_turns, Some(7));
}

#[tokio::test]
async fn main_agent_is_not_limited_by_legacy_global_max_turns() {
    let mut server = server(vec![
        tool_call("read_file", json!({"path": "input.txt"})),
        answer("completed after the tool turn"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("input.txt"), "local fixture").unwrap();
    let mut config = config(&server.url, tmp.path());
    config.max_turns = 1;
    let (engine, _) = engine(config);

    let result = tokio::time::timeout(
        Duration::from_secs(3),
        engine.turn(
            "continue after reading the file".into(),
            Selection::default(),
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(result, "completed after the tool turn");
    let _ = server.requests.recv().await.unwrap();
    let _ = server.requests.recv().await.unwrap();
}

#[tokio::test]
async fn child_depth_is_rejected_at_configured_boundary() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = parent_scope_config("http://127.0.0.1:1", tmp.path());
    config.max_subagent_depth = 1;
    config.agents.insert("child".into(), AgentConfig::default());
    let (engine, _) = engine(config);
    let root = engine
        .scope(
            &Selection {
                agent: Some("parent".into()),
                ..Selection::default()
            },
            "root",
            None,
        )
        .await
        .unwrap();
    let child = engine
        .scope(
            &Selection {
                agent: Some("child".into()),
                ..Selection::default()
            },
            "child",
            Some(&root),
        )
        .await
        .unwrap();
    assert_eq!(child.depth, 1);

    let error = engine
        .scope(
            &Selection {
                agent: Some("child".into()),
                ..Selection::default()
            },
            "grandchild",
            Some(&child),
        )
        .await
        .err()
        .expect("grandchild at the configured boundary must be rejected");
    assert!(error.to_string().contains("Subagent depth limit reached"));
}

#[tokio::test]
async fn omitted_child_tools_are_safe_defaults_intersected_with_parent_and_have_no_mcps() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = parent_scope_config("http://127.0.0.1:1", tmp.path());
    config.agents.insert("child".into(), AgentConfig::default());
    let (engine, _) = engine(config);
    let parent = engine
        .scope(
            &Selection {
                agent: Some("parent".into()),
                ..Selection::default()
            },
            "parent",
            None,
        )
        .await
        .unwrap();
    let child = engine
        .scope(
            &Selection {
                agent: Some("child".into()),
                ..Selection::default()
            },
            "child",
            Some(&parent),
        )
        .await
        .unwrap();

    assert_eq!(
        child.tools,
        Some(vec![
            "web_fetch".into(),
            "read_file".into(),
            "load_skill".into()
        ])
    );
    assert_eq!(child.mcps, Some(Vec::new()));
}

#[tokio::test]
async fn nested_delegation_with_one_parallel_slot_remains_deadlock_free() {
    let mut server = server(vec![
        tool_call("delegate", json!({"agent": "worker", "prompt": "child"})),
        tool_call(
            "delegate",
            json!({"agent": "worker", "prompt": "grandchild"}),
        ),
        answer("leaf"),
        answer("child"),
        answer("parent"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.max_parallel_subagents = 1;
    config.agents.insert(
        "worker".into(),
        AgentConfig {
            tools: Some(vec!["delegate".into()]),
            ..AgentConfig::default()
        },
    );
    let (engine, _) = engine(config);

    let result = tokio::time::timeout(
        Duration::from_secs(3),
        engine.turn(
            "start".into(),
            Selection::default(),
            CancellationToken::new(),
        ),
    )
    .await
    .expect("nested delegation exceeded bounded wait")
    .unwrap();

    assert_eq!(result, "parent");
    let _requests: Vec<Value> = {
        let mut requests = Vec::new();
        for _ in 0..5 {
            requests
                .push(serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap());
        }
        requests
    };
}
