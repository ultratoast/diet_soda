mod support;
use diet_soda::{
    config::{AgentConfig, ProviderConfig, ProviderKind, ToolConfig},
    engine::Selection,
    model::{Decision, UiEvent},
    provider::{ModelProvider, ModelRequest, RemoteProvider, SseDecoder},
    workflow::{self, Step, Workflow},
};
use serde_json::{json, Value};
use std::{sync::atomic::Ordering, time::Duration};
use support::*;
use tokio_util::sync::CancellationToken;

#[test]
fn sse_parser_handles_utf8_boundaries_comments_crlf_and_multiple_data_lines() {
    let bytes =
        ": heartbeat\r\ndata: {\"text\":\"🍞\"}\r\n\r\ndata: first\ndata: second\n\n".as_bytes();
    let mut parser = SseDecoder::default();
    let mut events = vec![];
    for byte in bytes {
        events.extend(parser.push(&[*byte]).unwrap());
    }
    assert_eq!(events, vec!["{\"text\":\"🍞\"}", "first\nsecond"]);
}
#[tokio::test]
async fn openrouter_tool_loop_persists_history_and_cost_and_continues_multi_turn() {
    let mut server = server(vec![
        tool_call("echo", json!({"value":"literal $HOME; nope"})),
        answer("tool done"),
        answer("second turn"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    let tool: ToolConfig = serde_json::from_value(json!({"type":"command","command":"/bin/echo","args":["{{value}}"],"description":"echo","hitl":false,"destructive":false,"input_schema":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}})).unwrap();
    config.tools.insert("echo".into(), tool);
    let (engine, _events) = engine(config);
    assert_eq!(
        engine
            .turn(
                "first".into(),
                Selection::default(),
                CancellationToken::new()
            )
            .await
            .unwrap(),
        "tool done"
    );
    assert_eq!(
        engine
            .turn(
                "next".into(),
                Selection::default(),
                CancellationToken::new()
            )
            .await
            .unwrap(),
        "second turn"
    );
    server.requests.recv().await.unwrap();
    let followup: Value =
        serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let messages = followup["messages"].as_array().unwrap();
    assert_eq!(messages.last().unwrap()["role"], "tool");
    assert!(messages.last().unwrap()["content"]
        .as_str()
        .unwrap()
        .contains("literal $HOME; nope"));
    let third: Value = serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    assert_eq!(third["messages"].as_array().unwrap().len(), 6);
    let session = engine.session.lock().await;
    assert_eq!(session.spend.microusd, 369);
    assert_eq!(session.messages.len(), 6);
    assert!(std::fs::read_to_string(&session.path)
        .unwrap()
        .contains("model_request"));
}
#[tokio::test]
async fn disabling_a_tool_while_approval_is_pending_prevents_execution() {
    let mut server = server(vec![
        tool_call("write_file", json!({"path":"forbidden.txt","content":"no"})),
        answer("rejected"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut test_config = config(&server.url, tmp.path());
    test_config.agents.insert(
        "writer".into(),
        serde_json::from_value(json!({"can_edit":true,"tools":["write_file"]})).unwrap(),
    );
    let (engine, mut events) = engine(test_config);
    let runner = engine.clone();
    let task = tokio::spawn(async move {
        runner
            .turn(
                "write".into(),
                Selection {
                    agent: Some("writer".into()),
                    ..Selection::default()
                },
                CancellationToken::new(),
            )
            .await
    });
    loop {
        if let UiEvent::Approval { reply, .. } = events.recv().await.unwrap() {
            engine
                .switches
                .write()
                .await
                .tools
                .insert("write_file".into(), false);
            reply.send(Decision::Approve).unwrap();
            break;
        }
    }
    task.await.unwrap().unwrap();
    assert!(!tmp.path().join("forbidden.txt").exists());
    server.requests.recv().await.unwrap();
    let followup = server.requests.recv().await.unwrap();
    assert!(followup.body.contains("Tool is disabled"));
}
#[tokio::test]
async fn outside_reads_are_approved_once_per_directory() {
    let outside = tempfile::tempdir().unwrap();
    let first = outside.path().join("first.txt");
    let second = outside.path().join("second.txt");
    std::fs::write(&first, "first-data").unwrap();
    std::fs::write(&second, "second-data").unwrap();
    let mut server = server(vec![
        tool_call("read_file", json!({"path": first.to_string_lossy()})),
        tool_call("read_file", json!({"path": second.to_string_lossy()})),
        answer("done"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut test_config = config(&server.url, tmp.path());
    test_config.agents.insert(
        "reader".into(),
        serde_json::from_value(json!({"tools":["read_file"]})).unwrap(),
    );
    let (engine, mut events) = engine(test_config);
    let runner = engine.clone();
    let mut task = tokio::spawn(async move {
        runner
            .turn(
                "read both".into(),
                Selection {
                    agent: Some("reader".into()),
                    ..Selection::default()
                },
                CancellationToken::new(),
            )
            .await
    });
    let mut approvals = 0;
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(UiEvent::Approval { reply, detail, .. }) => {
                    approvals += 1;
                    assert!(detail.contains("approved by directory"));
                    reply.send(Decision::Approve).unwrap();
                }
                Some(_) => {}
                None => break,
            },
            result = &mut task => {
                result.unwrap().unwrap();
                break;
            }
        }
    }
    assert_eq!(approvals, 1);
    server.requests.recv().await.unwrap();
    server.requests.recv().await.unwrap();
    let final_request = server.requests.recv().await.unwrap();
    assert!(final_request.body.contains("first-data"));
    assert!(final_request.body.contains("second-data"));
}
#[tokio::test]
async fn tool_errors_keep_the_original_call() {
    let mut server = server(vec![
        tool_call(
            "shell",
            json!({"command":"definitely-not-a-real-binary-xyz","args":[]}),
        ),
        answer("handled"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut test_config = config(&server.url, tmp.path());
    test_config.agents.insert(
        "runner".into(),
        serde_json::from_value(json!({"can_edit":true,"tools":["shell"]})).unwrap(),
    );
    let (engine, _events) = engine(test_config);
    engine
        .turn(
            "run".into(),
            Selection {
                agent: Some("runner".into()),
                ..Selection::default()
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    server.requests.recv().await.unwrap();
    let followup: Value =
        serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let last = followup["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(last["role"], "tool");
    let result: Value = serde_json::from_str(last["content"].as_str().unwrap()).unwrap();
    assert!(result["error"]
        .as_str()
        .unwrap()
        .contains("No such file or directory"));
    assert!(result["call"]
        .as_str()
        .unwrap()
        .contains("definitely-not-a-real-binary-xyz"));
}
#[tokio::test]
async fn approved_outside_shell_call_runs_without_a_standing_grant() {
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, "outside-data").unwrap();
    let mut server = server(vec![
        tool_call(
            "shell",
            json!({"command":"cat","args":[secret.to_string_lossy()]}),
        ),
        answer("done"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut test_config = config(&server.url, tmp.path());
    test_config.agents.insert(
        "runner".into(),
        serde_json::from_value(json!({"can_edit":true,"tools":["shell"]})).unwrap(),
    );
    let (engine, mut events) = engine(test_config);
    let runner = engine.clone();
    let task = tokio::spawn(async move {
        runner
            .turn(
                "read it".into(),
                Selection {
                    agent: Some("runner".into()),
                    ..Selection::default()
                },
                CancellationToken::new(),
            )
            .await
    });
    loop {
        if let UiEvent::Approval { reply, detail, .. } = events.recv().await.unwrap() {
            assert!(detail.contains("outside the configured workspace"));
            reply.send(Decision::Approve).unwrap();
            break;
        }
    }
    task.await.unwrap().unwrap();
    server.requests.recv().await.unwrap();
    let followup = server.requests.recv().await.unwrap();
    assert!(followup.body.contains("outside-data"));
}
#[tokio::test]
async fn workflow_hitl_is_after_step_and_never_after_final_step() {
    let mut server = server(vec![answer("first result"), answer("final result")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (engine, mut events) = engine(config(&server.url, tmp.path()));
    let workflow = Workflow {
        title: "test".into(),
        author: "test".into(),
        steps: vec![
            Step {
                agent: None,
                model: "openai/gpt-4.1-mini".into(),
                prompt: "first {{input}}".into(),
                mcps: vec![],
                hitl: true,
            },
            Step {
                agent: None,
                model: "openai/gpt-4.1-mini".into(),
                prompt: "second {{previous_result}}".into(),
                mcps: vec![],
                hitl: true,
            },
        ],
    };
    let runner = engine.clone();
    let task = tokio::spawn(async move {
        workflow::run(
            &runner,
            workflow,
            "topic".into(),
            Selection::default(),
            CancellationToken::new(),
        )
        .await
    });
    loop {
        if let UiEvent::Approval {
            title,
            detail,
            workflow,
            reply,
        } = events.recv().await.unwrap()
        {
            assert!(workflow);
            assert!(title.contains("Step 1 complete"));
            assert_eq!(detail, "first result");
            assert_eq!(server.count.load(Ordering::SeqCst), 1);
            assert!(engine.session.lock().await.spend.microusd > 0);
            reply.send(Decision::Approve).unwrap();
            break;
        }
    }
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        "final result"
    );
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, UiEvent::Approval { .. }));
    }
    server.requests.recv().await.unwrap();
    assert!(server
        .requests
        .recv()
        .await
        .unwrap()
        .body
        .contains("first result"));
}
#[tokio::test]
async fn workflow_retry_and_skip_do_not_propagate_discarded_results() {
    let mut server = server(vec![
        answer("discarded first"),
        answer("discarded retry"),
        answer("last"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let (engine, mut events) = engine(config(&server.url, tmp.path()));
    let workflow = Workflow {
        title: "test".into(),
        author: "test".into(),
        steps: vec![
            Step {
                agent: None,
                model: "x".into(),
                prompt: "first".into(),
                mcps: vec![],
                hitl: true,
            },
            Step {
                agent: None,
                model: "x".into(),
                prompt: "previous={{previous_result}}".into(),
                mcps: vec![],
                hitl: false,
            },
        ],
    };
    let task = tokio::spawn(async move {
        workflow::run(
            &engine,
            workflow,
            "".into(),
            Selection::default(),
            CancellationToken::new(),
        )
        .await
    });
    let mut decisions = vec![Decision::Skip, Decision::Retry];
    while !decisions.is_empty() {
        if let UiEvent::Approval { reply, .. } = events.recv().await.unwrap() {
            reply.send(decisions.pop().unwrap()).unwrap();
        }
    }
    assert_eq!(task.await.unwrap().unwrap(), "last");
    server.requests.recv().await.unwrap();
    server.requests.recv().await.unwrap();
    let third = server.requests.recv().await.unwrap();
    assert!(!third.body.contains("discarded"));
}
#[tokio::test]
async fn subagent_has_isolated_messages_and_parent_permissions_are_intersected() {
    let mut server = server(vec![
        tool_call(
            "delegate",
            json!({"agent":"researcher","prompt":"child only"}),
        ),
        answer("child result"),
        answer("parent result"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.agents.insert(
        "parent".into(),
        AgentConfig {
            tools: Some(vec!["delegate".into(), "web_fetch".into()]),
            ..AgentConfig::default()
        },
    );
    config.agents.insert(
        "researcher".into(),
        AgentConfig {
            tools: Some(vec!["web_fetch".into(), "write_file".into()]),
            prompt: Some("child system".into()),
            ..AgentConfig::default()
        },
    );
    let (engine, _events) = engine(config);
    let result = engine
        .turn(
            "private parent context".into(),
            Selection {
                agent: Some("parent".into()),
                ..Selection::default()
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result, "parent result");
    server.requests.recv().await.unwrap();
    let child: Value = serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    assert!(!child.to_string().contains("private parent context"));
    assert_eq!(child["tools"].as_array().unwrap().len(), 1);
    assert_eq!(child["tools"][0]["function"]["name"], "web_fetch");
    let session = engine.session.lock().await;
    assert_eq!(session.messages.len(), 4);
    assert_eq!(session.spend.microusd, 369);
}
#[tokio::test]
async fn provider_rejects_truncated_stream_and_handles_anthropic_tool_blocks() {
    let server = server(vec![Reply::sse(vec![json!({"choices":[{"delta":{"content":"partial"}}]})],false),Reply::sse(vec![
        json!({"type":"message_start","message":{"usage":{"input_tokens":20}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tool1","name":"web_fetch","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"url\":\"https://example.test\"}"}}),
        json!({"type":"message_delta","usage":{"output_tokens":3}}),json!({"type":"message_stop"})],false)]).await;
    let tmp = tempfile::tempdir().unwrap();
    let config = config(&server.url, tmp.path());
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let request = || ModelRequest {
        model: config.model.clone(),
        system: "system".into(),
        messages: vec![],
        tools: vec![],
        context: "test".into(),
    };
    assert!(RemoteProvider::new(config.providers["openrouter"].clone())
        .unwrap()
        .stream(request(), &tx, &CancellationToken::new())
        .await
        .is_err());
    let provider = RemoteProvider::new(ProviderConfig {
        kind: ProviderKind::Anthropic,
        base_url: server.url.clone(),
        api_key_env: None,
        timeout_seconds: 5,
    })
    .unwrap();
    let result = provider
        .stream(request(), &tx, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.usage.input_tokens, 20);
    assert_eq!(result.usage.output_tokens, 3);
    assert_eq!(result.message.tool_calls[0].name, "web_fetch");
    assert!(result.usage.cost_microusd.is_none());
}

#[tokio::test]
async fn fragmented_parallel_tool_calls_are_reassembled_by_index() {
    let server = server(vec![Reply::sse(vec![
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"first","function":{"name":"read_file","arguments":"{\"path\":"}},{"index":1,"id":"second","function":{"name":"web_fetch","arguments":"{\"url\":"}}]}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"\"https://example.test\"}"}},{"index":0,"function":{"arguments":"\"file.txt\"}"}}]}}]})
    ],true)]).await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.model.input_usd_per_million = Some(1.0);
    config.model.output_usd_per_million = Some(2.0);
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let response = RemoteProvider::new(config.providers["openrouter"].clone())
        .unwrap()
        .stream(
            ModelRequest {
                model: config.model,
                system: "system".into(),
                messages: vec![],
                tools: vec![],
                context: "test".into(),
            },
            &events,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.message.tool_calls[0].id, "first");
    assert_eq!(
        serde_json::from_str::<Value>(&response.message.tool_calls[1].arguments).unwrap(),
        json!({"url":"https://example.test"})
    );
    // Configured prices must not turn missing usage into a reported zero cost.
    assert!(response.usage.cost_microusd.is_none());
}

#[tokio::test]
async fn openai_and_litellm_use_their_configured_endpoints_and_token_fields() {
    for kind in [ProviderKind::Openai, ProviderKind::Litellm] {
        let mut server = server(vec![answer("compatible response")]).await;
        let tmp = tempfile::tempdir().unwrap();
        let mut config = config(&server.url, tmp.path());
        config.providers.get_mut("openrouter").unwrap().kind = kind.clone();
        let (engine, _) = engine(config);
        assert_eq!(
            engine
                .turn(
                    "test".into(),
                    Selection::default(),
                    CancellationToken::new()
                )
                .await
                .unwrap(),
            "compatible response"
        );
        let request = server.requests.recv().await.unwrap();
        assert!(request.headers.starts_with("POST /chat/completions "));
        let body: Value = serde_json::from_str(&request.body).unwrap();
        let field = if kind == ProviderKind::Openai {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        assert_eq!(body[field], 4096);
    }
}
