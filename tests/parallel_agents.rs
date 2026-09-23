mod support;
use diet_soda::{
    config::AgentConfig,
    engine::Selection,
    model::{Decision, UiEvent},
};
use serde_json::{json, Value};
use std::time::Duration;
use support::*;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn agent_can_dispatch_independent_children_in_parallel_and_account_once() {
    let tasks = json!({"tasks":[{"agent":"researcher","prompt":"task A"},{"agent":"researcher","prompt":"task B"}]});
    let mut server = parallel_server(vec![
        tool_call("delegate_parallel", tasks),
        answer("child result"),
        answer("child result"),
        answer("combined"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.max_parallel_subagents = 2;
    config.agents.insert(
        "researcher".into(),
        AgentConfig {
            tools: Some(vec![]),
            ..AgentConfig::default()
        },
    );
    let (engine, _) = engine(config);
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        engine.turn(
            "private parent".into(),
            Selection::default(),
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result, "combined");
    server.requests.recv().await.unwrap();
    for _ in 0..2 {
        let child = server.requests.recv().await.unwrap();
        assert!(!child.body.contains("private parent"));
        let body: Value = serde_json::from_str(&child.body).unwrap();
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    }
    let last: Value = serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let tool: Value = serde_json::from_str(
        last["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(tool["results"].as_array().unwrap().len(), 2);
    let session = engine.session.lock().await;
    assert_eq!(session.spend.microusd, 492);
    assert_eq!(session.messages.len(), 4);
}

#[tokio::test]
async fn multiple_delegate_calls_are_concurrent_and_tool_results_keep_call_order() {
    let calls: Vec<_> = ["first","second"].iter().enumerate().map(|(index,id)| json!({"index":index,"id":id,"function":{"name":"delegate","arguments":json!({"agent":"worker","prompt":id}).to_string()}})).collect();
    let first = Reply::sse(
        vec![json!({"choices":[{"delta":{"tool_calls":calls}}]})],
        true,
    );
    let mut server = parallel_server(vec![
        first,
        answer("child"),
        answer("child"),
        answer("done"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config
        .agents
        .insert("worker".into(), AgentConfig::default());
    let (engine, _) = engine(config);
    tokio::time::timeout(
        Duration::from_secs(3),
        engine.turn(
            "dispatch".into(),
            Selection::default(),
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    for _ in 0..3 {
        server.requests.recv().await.unwrap();
    }
    let final_request: Value =
        serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let messages = final_request["messages"].as_array().unwrap();
    assert_eq!(messages[3]["tool_call_id"], "first");
    assert_eq!(messages[4]["tool_call_id"], "second");
}

#[tokio::test]
async fn nested_delegation_with_one_slot_does_not_deadlock() {
    let server = server(vec![
        tool_call("delegate", json!({"agent":"worker","prompt":"child"})),
        tool_call("delegate", json!({"agent":"worker","prompt":"grandchild"})),
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
    .unwrap()
    .unwrap();
    assert_eq!(result, "parent");
}

#[tokio::test]
async fn approvals_are_serialized_and_waiting_children_can_be_cancelled() {
    let tmp = tempfile::tempdir().unwrap();
    let (engine, mut events) = engine(config("http://localhost:1", tmp.path()));
    let first = engine.clone();
    let first_task = tokio::spawn(async move {
        first
            .approve(
                "a",
                "first".into(),
                "".into(),
                false,
                &CancellationToken::new(),
            )
            .await
    });
    let first_reply = match events.recv().await.unwrap() {
        UiEvent::Approval { reply, .. } => reply,
        _ => panic!("expected approval"),
    };
    let second = engine.clone();
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let second_task = tokio::spawn(async move {
        second
            .approve("b", "second".into(), "".into(), false, &token)
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(30), events.recv())
            .await
            .is_err()
    );
    cancel.cancel();
    assert_eq!(second_task.await.unwrap().unwrap(), Decision::Abort);
    first_reply.send(Decision::Approve).unwrap();
    assert_eq!(first_task.await.unwrap().unwrap(), Decision::Approve);
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn parallel_children_finish_after_serialized_approvals_pause_parent_budget() {
    let tasks = json!({
        "tasks": [
            {"agent": "worker", "prompt": "inspect first"},
            {"agent": "worker", "prompt": "inspect second"}
        ]
    });
    let mut server = parallel_server(vec![
        tool_call("delegate_parallel", tasks),
        tool_call("read_file", json!({"path": "first.txt"})),
        tool_call("read_file", json!({"path": "second.txt"})),
        answer("first child"),
        answer("second child"),
        answer("parent complete"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("first.txt"), "first").unwrap();
    std::fs::write(tmp.path().join("second.txt"), "second").unwrap();
    let mut config = config(&server.url, tmp.path());
    config.approval_tools.push("read_file".into());
    config.max_parallel_subagents = 2;
    config.agents.insert(
        "parent".into(),
        serde_json::from_value(json!({
            "timeout_seconds": 2,
            "tools": ["delegate_parallel", "read_file"]
        }))
        .unwrap(),
    );
    config.agents.insert(
        "worker".into(),
        AgentConfig {
            timeout_seconds: Some(10),
            tools: Some(vec!["read_file".into()]),
            ..AgentConfig::default()
        },
    );
    let (engine, mut events) = engine(config);
    let runner = engine.clone();
    let task = tokio::spawn(async move {
        runner
            .turn(
                "coordinate both inspections".into(),
                Selection {
                    agent: Some("parent".into()),
                    ..Selection::default()
                },
                CancellationToken::new(),
            )
            .await
    });

    let first_reply = loop {
        if let UiEvent::Approval { reply, .. } =
            tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .unwrap()
                .unwrap()
        {
            break reply;
        }
    };
    // The first approval outlasts the parent's two-second running budget.
    // While it is held, the child and its parent pause guards must keep the
    // parent budget from expiring.
    tokio::time::sleep(Duration::from_millis(2_100)).await;
    first_reply.send(Decision::Approve).unwrap();

    let second_reply = loop {
        if let UiEvent::Approval { reply, .. } =
            tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .unwrap()
                .unwrap()
        {
            break reply;
        }
    };
    // Approval delivery is serialized, so the total human wait exceeds the
    // parent deadline even though the second approval starts afterward.
    tokio::time::sleep(Duration::from_millis(2_100)).await;
    second_reply.send(Decision::Approve).unwrap();

    let result = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("parallel children must not deadlock after the parent deadline")
        .unwrap()
        .unwrap();
    assert_eq!(result, "parent complete");
    let mut requests = Vec::new();
    for _ in 0..6 {
        requests.push(server.requests.recv().await.unwrap());
    }
    let final_request: Value = serde_json::from_str(&requests[5].body).unwrap();
    let tool: Value = serde_json::from_str(
        final_request["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["content"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let results = tool["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    let mut child_results: Vec<_> = results
        .iter()
        .map(|result| result["result"].as_str().unwrap())
        .collect();
    child_results.sort_unstable();
    assert_eq!(child_results, ["first child", "second child"]);
}
