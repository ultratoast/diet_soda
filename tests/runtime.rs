mod support;
#[cfg(unix)]
use diet_soda::config::Config;
use diet_soda::{
    config::{AgentConfig, ProviderConfig, ProviderKind, ToolConfig},
    engine::Selection,
    model::{Decision, Message, ToolCall, UiEvent},
    provider::{
        phases, IncompleteStreamError, ModelProvider, ModelRequest, RemoteProvider, SseDecoder,
    },
    session::{DisplayEvent, TranscriptEntry},
    tools,
    workflow::{self, Step, Workflow},
};
use serde_json::{json, Value};
use std::{sync::atomic::Ordering, time::Duration};
use support::*;
use tokio_util::sync::CancellationToken;

fn display_messages(events: &[DisplayEvent]) -> Vec<&TranscriptEntry> {
    events
        .iter()
        .filter_map(|event| match event {
            DisplayEvent::Message(entry) => Some(entry),
            DisplayEvent::Activity(_) => None,
        })
        .collect()
}

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

#[cfg(unix)]
#[tokio::test]
async fn shell_builtin_uses_configured_timeout_and_reports_termination() {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        workspace: tmp.path().into(),
        builtin_timeouts: diet_soda::config::BuiltinTimeoutsConfig {
            shell_timeout_seconds: 1,
            gh_timeout_seconds: 120,
        },
        bash_permissions: "none".into(),
        ..Config::default()
    };

    let error = tools::builtin(
        "shell",
        &json!({"command":"/bin/sh","args":["-c","sleep 30"]}),
        &config,
        &CancellationToken::new(),
        false,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("Command timed out"));
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
async fn direct_read_builtin_requires_outside_grant_and_accepts_standing_grant() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let path = outside.path().join("secret.txt");
    std::fs::write(&path, "outside-data").unwrap();
    let config = config("http://127.0.0.1:1", workspace.path());
    let args = json!({"path": path});

    let error = tools::builtin(
        "read_file",
        &args,
        &config,
        &CancellationToken::new(),
        false,
    )
    .await
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("outside the configured workspace"));
    assert_eq!(
        tools::builtin("read_file", &args, &config, &CancellationToken::new(), true)
            .await
            .unwrap()["content"],
        "outside-data"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn direct_read_builtin_rejects_in_workspace_symlink_to_outside_without_grant() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("secret.txt");
    std::fs::write(&target, "outside-data").unwrap();
    std::os::unix::fs::symlink(&target, workspace.path().join("link.txt")).unwrap();
    let config = config("http://127.0.0.1:1", workspace.path());

    let error = tools::builtin(
        "read_file",
        &json!({"path":"link.txt"}),
        &config,
        &CancellationToken::new(),
        false,
    )
    .await
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("outside the configured workspace"));
}

#[tokio::test]
async fn write_builtin_missing_parent_returns_error_without_creating_anything() {
    let workspace = tempfile::tempdir().unwrap();
    let config = config("http://127.0.0.1:1", workspace.path());
    let destination = workspace.path().join("missing").join("file.txt");

    let error = tools::builtin(
        "write_file",
        &json!({"path":"missing/file.txt","content":"never written"}),
        &config,
        &CancellationToken::new(),
        false,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("No such file") || error.to_string().contains("not found"));
    assert!(!destination.exists());
    assert!(!workspace.path().join("missing").exists());
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
    let (engine, mut events) = engine(test_config);
    let runner = engine.clone();
    let mut task = tokio::spawn(async move {
        runner
            .turn(
                "run".into(),
                Selection {
                    agent: Some("runner".into()),
                    ..Selection::default()
                },
                CancellationToken::new(),
            )
            .await
    });
    let mut approved = false;
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            tokio::select! {
                event = events.recv() => match event {
                    Some(UiEvent::Approval { reply, .. }) => {
                        reply.send(Decision::Approve).unwrap();
                        approved = true;
                    }
                    Some(_) => {}
                    None => break,
                },
                outcome = &mut task => {
                    return outcome.unwrap().unwrap();
                }
            }
        }
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    })
    .await;
    assert!(approved, "expected shell approval was never delivered");
    result.unwrap();
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
async fn read_only_agents_can_run_safe_shell_commands() {
    let mut server = server(vec![
        tool_call("shell", json!({"command":"printf","args":["agent-shell"]})),
        answer("handled"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut test_config = config(&server.url, tmp.path());
    test_config.agents.insert(
        "runner".into(),
        serde_json::from_value(json!({"tools":["shell"]})).unwrap(),
    );
    let (engine, mut events) = engine(test_config);
    let runner = engine.clone();
    let mut task = tokio::spawn(async move {
        runner
            .turn(
                "run it".into(),
                Selection {
                    agent: Some("runner".into()),
                    ..Selection::default()
                },
                CancellationToken::new(),
            )
            .await
    });
    tokio::select! {
        Some(UiEvent::Approval { reply, .. }) = events.recv() => {
            reply.send(Decision::Reject).unwrap();
            panic!("safe shell command unexpectedly requested approval");
        }
        result = &mut task => {
            result.unwrap().unwrap();
        }
    }
    server.requests.recv().await.unwrap();
    let followup = server.requests.recv().await.unwrap();
    assert!(followup.body.contains("agent-shell"));
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
async fn approved_inline_outside_shell_path_runs_with_a_per_call_grant() {
    let outside = tempfile::tempdir().unwrap();
    let output_path = outside.path().join("result.txt");
    let inline_path = format!("--output={}", output_path.display());
    let mut server = server(vec![
        tool_call(
            "shell",
            json!({"command":"printf","args":["%s",inline_path]}),
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
                "run it".into(),
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
    let followup: Value =
        serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let content = followup["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap();
    let result: Value = serde_json::from_str(content).unwrap();
    assert!(
        result["stdout"].as_str().unwrap().contains(&inline_path),
        "stdout {:?} did not contain {:?}",
        result["stdout"],
        inline_path
    );
    assert!(!output_path.exists());
}

#[tokio::test]
async fn rejected_inline_outside_shell_path_never_executes() {
    let outside = tempfile::tempdir().unwrap();
    let output_path = outside.path().join("result.txt");
    let inline_path = format!("--output={}", output_path.display());
    let mut server = server(vec![
        tool_call(
            "shell",
            json!({"command":"printf","args":[inline_path.clone()]}),
        ),
        answer("rejected"),
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
                "reject it".into(),
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
            reply.send(Decision::Reject).unwrap();
            break;
        }
    }
    task.await.unwrap().unwrap();
    server.requests.recv().await.unwrap();
    let followup: Value =
        serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let content = followup["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap();
    let result: Value = serde_json::from_str(content).unwrap();
    assert_eq!(result["error"], "Tool rejected by user");
    assert!(result.get("stdout").is_none());
    assert!(!output_path.exists());
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
            persist_allowed,
            reply,
        } = events.recv().await.unwrap()
        {
            assert!(workflow);
            assert!(!persist_allowed);
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
        headers: std::collections::BTreeMap::new(),
        timeout_seconds: 5,
        allow_private_networks: true,
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

// -------------------------------------------------------------------------
// Wave 2 streaming timeout, partial output, and status behavior tests.
// Each test sets `timeout_seconds` to a small value and uses the new
// `header_delay` / `chunk_delay` / `stall` fields on [`Reply`] to drive a
// single edge of the timeout/EOF contract. The mock server returns
// `application/json` shaped responses for failure cases so the assertions
// focus on the timeout path rather than the SSE parser. The engine-level
// test drives the public `Engine::turn` path so the transcript and
// model-request-history split is verified end to end.
// -------------------------------------------------------------------------

fn request(model: diet_soda::config::ModelConfig) -> ModelRequest {
    ModelRequest {
        model,
        system: "system".into(),
        messages: vec![],
        tools: vec![],
        context: "test".into(),
    }
}

#[tokio::test]
async fn header_timeout_raises_when_provider_does_not_respond() {
    // Server waits longer than `timeout_seconds` before sending headers.
    // The provider must surface a header-timeout error carrying the
    // configured deadline, not a generic network error or a hang.
    let mut server = server(vec![Reply {
        status: 200,
        content_type: "text/event-stream".into(),
        body: String::new(),
        headers: vec![],
        header_delay: Some(Duration::from_secs(3)),
        chunk_delay: None,
        stall: None,
    }])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config
        .providers
        .get_mut("openrouter")
        .unwrap()
        .timeout_seconds = 1;
    let provider = RemoteProvider::new(config.providers["openrouter"].clone()).unwrap();
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let error = match provider
        .stream(
            request(config.model.clone()),
            &events,
            &CancellationToken::new(),
        )
        .await
    {
        Ok(_) => panic!("header timeout must error"),
        Err(error) => error,
    };
    let message = format!("{error:#}");
    assert!(
        message.to_lowercase().contains("header timeout"),
        "expected header-timeout error; got: {message}"
    );
    server.requests.recv().await.unwrap();
}

#[tokio::test]
async fn idle_timeout_after_partial_text_records_incomplete_reason() {
    // Provider sends one chunk of text, then stalls past the idle deadline.
    // The stream must surface an incomplete error carrying the partial text
    // and an idle-timeout reason. The returned Message must not include any
    // tool-call fragments so the engine cannot re-execute them later.
    let body = format!(
        "data: {}\r\n\r\n",
        json!({"choices": [{"delta": {"content": "begin "}}]})
    );
    let mut server = server(vec![Reply {
        status: 200,
        content_type: "text/event-stream".into(),
        body,
        headers: vec![],
        header_delay: None,
        chunk_delay: None,
        stall: Some(Duration::from_secs(3)),
    }])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config
        .providers
        .get_mut("openrouter")
        .unwrap()
        .timeout_seconds = 1;
    let provider = RemoteProvider::new(config.providers["openrouter"].clone()).unwrap();
    let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let error = match provider
        .stream(
            request(config.model.clone()),
            &events,
            &CancellationToken::new(),
        )
        .await
    {
        Ok(_) => panic!("stalled stream must error"),
        Err(error) => error,
    };
    let incomplete = error
        .downcast_ref::<IncompleteStreamError>()
        .expect("idle timeout must surface IncompleteStreamError");
    assert_eq!(incomplete.message.content, "begin ");
    assert_eq!(incomplete.reason, "idle timeout: no chunk within 1s");
    // The provider emitted a single Delta for the partial text; the
    // incomplete path itself does not emit any further UI events.
    let mut saw_delta = false;
    while let Ok(event) = rx.try_recv() {
        if let UiEvent::Delta { text, .. } = event {
            assert_eq!(text, "begin ");
            saw_delta = true;
        }
    }
    assert!(saw_delta, "partial text must still surface as a Delta");
    server.requests.recv().await.unwrap();
}

#[tokio::test]
async fn slow_drip_within_idle_budget_succeeds_even_if_total_exceeds_timeout() {
    // Each chunk arrives well under the idle deadline, so the per-chunk
    // timer never fires. The total wall-clock duration is several times the
    // configured `timeout_seconds`, which would have tripped the old
    // total-body deadline. The provider must treat this as a clean
    // streaming completion once the `[DONE]` sentinel arrives.
    let mut events_body = String::new();
    for _ in 0..3 {
        events_body.push_str(&format!(
            "data: {}\r\n\r\n",
            json!({"choices": [{"delta": {"content": "x"}}]})
        ));
    }
    events_body.push_str("data: [DONE]\r\n\r\n");
    // The body is roughly 140 bytes; with the 7-byte chunk the server
    // uses, that's ~20 chunks. With `chunk_delay` of 150ms, the total
    // wall-clock duration is ~3 seconds, well past the 1s `timeout_seconds`
    // the test configures — exactly the case the old total-body deadline
    // would have broken.
    let server = server(vec![Reply {
        status: 200,
        content_type: "text/event-stream".into(),
        body: events_body,
        headers: vec![],
        header_delay: None,
        chunk_delay: Some(Duration::from_millis(150)),
        stall: None,
    }])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config
        .providers
        .get_mut("openrouter")
        .unwrap()
        .timeout_seconds = 1;
    let provider = RemoteProvider::new(config.providers["openrouter"].clone()).unwrap();
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let response = tokio::time::timeout(
        Duration::from_secs(8),
        provider.stream(
            request(config.model.clone()),
            &events,
            &CancellationToken::new(),
        ),
    )
    .await
    .expect("slow drip must not hang the test fixture")
    .expect("slow drip must succeed");
    assert_eq!(response.message.content, "xxx");
    assert!(response.message.tool_calls.is_empty());
    assert!(response.message.incomplete.is_none());
}

#[tokio::test]
async fn clean_eof_without_done_with_finish_reason_succeeds() {
    // OpenAI-compatible streams occasionally close without sending
    // `[DONE]`. The provider must accept this when every choice reported a
    // valid non-null `finish_reason` and no tool call is incomplete. The
    // `complete` here means "no fragment is missing id/name/arguments".
    let mut body = String::new();
    body.push_str(&format!(
        "data: {}\r\n\r\n",
        json!({"choices": [{"delta": {"tool_calls": [
            {"index": 0, "id": "c1", "function": {"name": "noop", "arguments": "{}"}}
        ]}}]})
    ));
    body.push_str(&format!(
        "data: {}\r\n\r\n",
        json!({"choices": [{"finish_reason": "tool_calls", "delta": {}}]})
    ));
    let server = server(vec![Reply {
        status: 200,
        content_type: "text/event-stream".into(),
        body,
        headers: vec![],
        header_delay: None,
        chunk_delay: None,
        stall: None,
    }])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let config = config(&server.url, tmp.path());
    let provider = RemoteProvider::new(config.providers["openrouter"].clone()).unwrap();
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let response = provider
        .stream(
            request(config.model.clone()),
            &events,
            &CancellationToken::new(),
        )
        .await
        .expect("clean EOF with finish_reason must succeed");
    assert_eq!(response.message.tool_calls.len(), 1);
    assert_eq!(response.message.tool_calls[0].name, "noop");
    assert!(response.message.incomplete.is_none());
}

#[tokio::test]
async fn incomplete_eof_without_finish_reason_is_an_error() {
    // The same close-without-`[DONE]` shape as above, but the provider
    // never reported a `finish_reason`. The stream must be treated as
    // incomplete rather than silently accepted; the error must carry the
    // safe partial text so the transcript still shows what was streamed.
    let body = format!(
        "data: {}\r\n\r\n",
        json!({"choices": [{"delta": {"content": "partial"}}]})
    );
    let server = server(vec![Reply {
        status: 200,
        content_type: "text/event-stream".into(),
        body,
        headers: vec![],
        header_delay: None,
        chunk_delay: None,
        stall: None,
    }])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let config = config(&server.url, tmp.path());
    let provider = RemoteProvider::new(config.providers["openrouter"].clone()).unwrap();
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let error = match provider
        .stream(
            request(config.model.clone()),
            &events,
            &CancellationToken::new(),
        )
        .await
    {
        Ok(_) => panic!("EOF without finish_reason must error"),
        Err(error) => error,
    };
    let incomplete = error
        .downcast_ref::<IncompleteStreamError>()
        .expect("incomplete EOF must surface IncompleteStreamError");
    assert_eq!(incomplete.message.content, "partial");
    assert!(
        incomplete.reason.contains("completion event"),
        "reason must mention the missing completion event; got: {}",
        incomplete.reason
    );
}

#[tokio::test]
async fn cancellation_during_stream_returns_incomplete_error_with_partial_text() {
    // Cancel after the first event has streamed but before the second:
    // the partial text must survive in the returned incomplete message
    // so the engine can record it as a transcript marker.
    //
    // The first SSE event is ~50 bytes; with the server's 7-byte chunks
    // and a 30ms per-chunk delay, it lands in roughly 7 * 30 = 210ms.
    // Cancelling at 350ms gives the SSE decoder time to emit the event
    // and the provider time to record the partial delta.
    let mut body = String::new();
    body.push_str(&format!(
        "data: {}\r\n\r\n",
        json!({"choices": [{"delta": {"content": "start "}}]})
    ));
    body.push_str(&format!(
        "data: {}\r\n\r\n",
        json!({"choices": [{"delta": {"content": "more"}}]})
    ));
    let mut server = server(vec![Reply {
        status: 200,
        content_type: "text/event-stream".into(),
        body,
        headers: vec![],
        header_delay: None,
        chunk_delay: Some(Duration::from_millis(30)),
        stall: None,
    }])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let config = config(&server.url, tmp.path());
    let provider = RemoteProvider::new(config.providers["openrouter"].clone()).unwrap();
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let cancel_inside = cancel.clone();
    let task = tokio::spawn(async move {
        let provider_stream = provider.stream(request(config.model.clone()), &events_tx, &cancel);
        // The stream future drives the provider; the cancellation fires
        // when the caller (this test) decides. We await the stream and
        // cancel mid-flight by polling the events channel from another
        // task: once we see the first Delta we know the partial text
        // has reached the message, so we cancel and wait for the
        // provider future to error out.
        tokio::pin!(provider_stream);
        let cancel_task = async {
            loop {
                match events_rx.recv().await {
                    Some(UiEvent::Delta { .. }) => {
                        cancel_inside.cancel();
                        return;
                    }
                    Some(_) => continue,
                    None => return,
                }
            }
        };
        tokio::select!(
            result = &mut provider_stream => result,
            _ = cancel_task => provider_stream.await,
        )
    });
    let error = match task.await.unwrap() {
        Ok(_) => panic!("cancelled stream must error"),
        Err(error) => error,
    };
    let incomplete = error
        .downcast_ref::<IncompleteStreamError>()
        .unwrap_or_else(|| {
            panic!("cancellation must surface IncompleteStreamError; got: {error:#}")
        });
    // Scheduling-safe: the second SSE event may or may not have reached the
    // decoder before the cancellation fires. The first delta ("start ") is
    // the contract — it must be preserved as safe partial text. We assert
    // `starts_with` so a scheduler that lets the second delta slip through
    // does not flake the test, while still guaranteeing the partial text
    // survives.
    assert!(
        incomplete.message.content.starts_with("start "),
        "partial text must begin with the first delta; got: {:?}",
        incomplete.message.content
    );
    assert!(
        incomplete.message.content.len() <= "start more".len(),
        "cancellation must not consume the whole stream into the partial; got: {:?}",
        incomplete.message.content
    );
    assert_eq!(incomplete.reason, "cooperative cancellation");
    assert!(incomplete.message.tool_calls.is_empty());
    let _ = server.requests.recv().await;
}

#[tokio::test]
async fn engine_records_incomplete_message_and_excludes_it_from_history() {
    // Drive a real engine turn against a stream that stalls after one
    // delta. The engine must:
    //   1. Persist the partial assistant message with `incomplete` set,
    //   2. Push it onto the display timeline,
    //   3. NOT push it onto the model request history,
    //   4. Bubble up the typed error so the caller can render it.
    let mut server = server(vec![Reply {
        status: 200,
        content_type: "text/event-stream".into(),
        body: format!(
            "data: {}\r\n\r\n",
            json!({"choices": [{"delta": {"content": "begin"}}]})
        ),
        headers: vec![],
        header_delay: None,
        chunk_delay: None,
        stall: Some(Duration::from_secs(3)),
    }])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config
        .providers
        .get_mut("openrouter")
        .unwrap()
        .timeout_seconds = 1;
    let (engine, _events) = engine(config);
    let err = engine
        .turn(
            "first".into(),
            Selection::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("stalled stream must surface as a turn error");
    let message = format!("{err:#}");
    assert!(
        message.contains("idle timeout") || message.contains("Provider stream ended"),
        "expected incomplete-stream message; got: {message}"
    );
    let sessions_dir = engine.config.read().await.sessions_dir.clone();
    let session_path = {
        let session = engine.session.lock().await;
        let messages = display_messages(&session.display_events);
        assert_eq!(messages.len(), 2);
        let partial = messages
            .iter()
            .find(|row| row.context == "main" && row.message.role == "assistant")
            .expect("partial assistant transcript entry");
        assert_eq!(partial.message.content, "begin");
        let incomplete = partial
            .message
            .incomplete
            .as_ref()
            .expect("partial assistant must carry an incomplete marker");
        assert!(
            incomplete.reason.contains("idle timeout"),
            "incomplete reason should mention idle timeout; got: {}",
            incomplete.reason
        );
        assert!(
            session.messages.len() == 1 && session.messages[0].role == "user",
            "incomplete assistant must not be on the main-context history; got: {:?}",
            session.messages
        );
        let raw = std::fs::read_to_string(&session.path).unwrap();
        assert!(
            raw.contains("\"incomplete\"") && raw.contains("idle timeout"),
            "raw session must persist the incomplete marker: {raw}"
        );
        session.path.clone()
    };
    drop(engine);
    // Reopen the existing session by id, not a fresh one. The engine
    // owned the lock until the drop above; the second `Session::open`
    // claim the exclusive advisory lock the production code path takes.
    let session_id = session_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(|stem| stem.to_owned())
        .expect("session path should carry the id as its stem");
    let reopened = diet_soda::session::Session::open(&sessions_dir, Some(&session_id)).unwrap();
    assert!(
        reopened.messages.len() == 1 && reopened.messages[0].role == "user",
        "reopen must not push the incomplete assistant into history; got: {:?}",
        reopened.messages
    );
    let messages = display_messages(&reopened.display_events);
    let partial = messages
        .iter()
        .find(|row| row.message.incomplete.is_some())
        .expect("display timeline must still show the partial after reopen");
    assert_eq!(partial.message.content, "begin");
    assert!(session_path.exists());
    let _ = server.requests.recv().await;
}

#[tokio::test]
async fn approval_wait_is_outside_agent_budget() {
    let mut server = server(vec![
        tool_call("read_file", json!({"path":"approved.txt"})),
        answer("completed after approval"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("approved.txt"), "approved").unwrap();
    let mut config = config(&server.url, tmp.path());
    config.approval_tools.push("read_file".into());
    config.agents.insert(
        "short-lived".into(),
        serde_json::from_value(json!({
            "timeout_seconds": 1,
            "tools": ["read_file"]
        }))
        .unwrap(),
    );
    let (engine, mut events) = engine(config);
    let runner = engine.clone();
    let task = tokio::spawn(async move {
        runner
            .turn(
                "wait for approval".into(),
                Selection {
                    agent: Some("short-lived".into()),
                    ..Selection::default()
                },
                CancellationToken::new(),
            )
            .await
    });

    let reply = loop {
        if let UiEvent::Approval { reply, .. } =
            tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap()
        {
            break reply;
        }
    };
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    reply.send(Decision::Approve).unwrap();

    let result = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result, "completed after approval");
    server.requests.recv().await.unwrap();
    server.requests.recv().await.unwrap();
}

#[tokio::test]
async fn execution_after_approval_still_consumes_budget_and_records_one_interrupted_result() {
    let server = server(vec![tool_call(
        "shell",
        json!({"command":"sleep", "args":["3"]}),
    )])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.approval_tools.push("shell".into());
    config.agents.insert(
        "short-lived".into(),
        serde_json::from_value(json!({
            "timeout_seconds": 1,
            "can_edit": true,
            "tools": ["shell"]
        }))
        .unwrap(),
    );
    let (engine, mut events) = engine(config);
    let runner = engine.clone();
    let task = tokio::spawn(async move {
        runner
            .turn(
                "run after approval".into(),
                Selection {
                    agent: Some("short-lived".into()),
                    ..Selection::default()
                },
                CancellationToken::new(),
            )
            .await
    });
    let reply = loop {
        if let UiEvent::Approval { reply, .. } =
            tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap()
        {
            break reply;
        }
    };
    reply.send(Decision::Approve).unwrap();

    let error = tokio::time::timeout(Duration::from_secs(4), task)
        .await
        .unwrap()
        .unwrap()
        .expect_err("execution must exceed the post-approval budget");
    assert!(format!("{error:#}").contains("timed out"));
    let session = engine.session.lock().await;
    let interrupted = session
        .display_events
        .iter()
        .filter(|event| {
            matches!(
                event,
                DisplayEvent::Message(message)
                    if message.message.role == "tool"
            )
        })
        .count();
    assert_eq!(interrupted, 1);
}

#[tokio::test]
async fn workflow_hitl_wait_does_not_consume_next_step_conversation_budget() {
    let server = server(vec![answer("first"), answer("second")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.agents.insert(
        "short-lived".into(),
        serde_json::from_value(json!({"timeout_seconds": 1})).unwrap(),
    );
    let (engine, mut events) = engine(config);
    let workflow = Workflow {
        title: "budget workflow".into(),
        author: "test".into(),
        steps: vec![
            Step {
                agent: Some("short-lived".into()),
                model: "openai/gpt-4.1-mini".into(),
                prompt: "first".into(),
                mcps: vec![],
                hitl: true,
            },
            Step {
                agent: Some("short-lived".into()),
                model: "openai/gpt-4.1-mini".into(),
                prompt: "second".into(),
                mcps: vec![],
                hitl: false,
            },
        ],
    };
    let runner = engine.clone();
    let task = tokio::spawn(async move {
        workflow::run(
            &runner,
            workflow,
            "input".into(),
            Selection::default(),
            CancellationToken::new(),
        )
        .await
    });
    let reply = loop {
        if let UiEvent::Approval {
            workflow: true,
            reply,
            ..
        } = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap()
        {
            break reply;
        }
    };
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    reply.send(Decision::Approve).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        "second"
    );
}

#[tokio::test]
async fn child_tool_approval_pauses_child_and_parent_budgets() {
    let server = server(vec![
        tool_call("delegate", json!({"agent":"worker","prompt":"inspect"})),
        tool_call("read_file", json!({"path":"child.txt"})),
        answer("child finished"),
        answer("parent finished"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("child.txt"), "child data").unwrap();
    let mut config = config(&server.url, tmp.path());
    config.approval_tools.push("read_file".into());
    config.agents.insert(
        "parent".into(),
        serde_json::from_value(json!({
            "timeout_seconds": 1,
            "tools": ["delegate", "read_file"]
        }))
        .unwrap(),
    );
    config.agents.insert(
        "worker".into(),
        serde_json::from_value(json!({
            "timeout_seconds": 1,
            "tools": ["read_file"]
        }))
        .unwrap(),
    );
    let (engine, mut events) = engine(config);
    let runner = engine.clone();
    let task = tokio::spawn(async move {
        runner
            .turn(
                "delegate work".into(),
                Selection {
                    agent: Some("parent".into()),
                    ..Selection::default()
                },
                CancellationToken::new(),
            )
            .await
    });
    let reply = loop {
        if let UiEvent::Approval { reply, .. } =
            tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap()
        {
            break reply;
        }
    };
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    reply.send(Decision::Approve).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(4), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        "parent finished"
    );
}

#[tokio::test]
async fn engine_error_repairs_all_missing_calls_and_keeps_original_error() {
    let upstream_error = Reply {
        status: 500,
        content_type: "application/json".into(),
        body: r#"{"error":{"message":"upstream exploded"}}"#.into(),
        headers: vec![],
        header_delay: None,
        chunk_delay: None,
        stall: None,
    };
    let mut server = server(vec![upstream_error]).await;
    let tmp = tempfile::tempdir().unwrap();
    let config = config(&server.url, tmp.path());
    let (engine, _events) = engine(config);
    let mut assistant = Message::new("assistant", "");
    for id in ["error-a", "error-b", "error-c"] {
        assistant.tool_calls.push(ToolCall {
            id: id.into(),
            name: "lookup".into(),
            arguments: "{}".into(),
        });
    }
    engine
        .session
        .lock()
        .await
        .record_message("main", assistant)
        .unwrap();

    let error = engine
        .turn(
            "continue".into(),
            Selection::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("the upstream failure must remain an error");
    assert!(
        format!("{error:#}").contains("500"),
        "the provider error must remain intact: {error:#}"
    );
    let session = engine.session.lock().await;
    let repaired: Vec<&str> = session
        .messages
        .iter()
        .filter(|message| message.role == "tool")
        .map(|message| message.tool_call_id.as_deref().unwrap())
        .collect();
    assert_eq!(repaired, vec!["error-a", "error-b", "error-c"]);
    let raw = std::fs::read_to_string(&session.path).unwrap();
    assert!(
        raw.ends_with('\n'),
        "error repair must reach the checkpointed log"
    );
    drop(session);
    assert!(server.requests.recv().await.is_some());
}

#[tokio::test]
async fn engine_cancel_repairs_all_missing_calls_and_keeps_cancel_error() {
    let server = server(vec![]).await;
    let tmp = tempfile::tempdir().unwrap();
    let config = config(&server.url, tmp.path());
    let (engine, _events) = engine(config);
    let mut assistant = Message::new("assistant", "");
    for id in ["cancel-a", "cancel-b", "cancel-c"] {
        assistant.tool_calls.push(ToolCall {
            id: id.into(),
            name: "lookup".into(),
            arguments: "{}".into(),
        });
    }
    engine
        .session
        .lock()
        .await
        .record_message("main", assistant)
        .unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();

    let error = engine
        .turn("cancel".into(), Selection::default(), cancel)
        .await
        .expect_err("cancelled turn must remain an error");
    assert!(format!("{error:#}").contains("Cancelled"));
    let session = engine.session.lock().await;
    let repaired: Vec<&str> = session
        .messages
        .iter()
        .filter(|message| message.role == "tool")
        .map(|message| message.tool_call_id.as_deref().unwrap())
        .collect();
    assert_eq!(repaired, vec!["cancel-a", "cancel-b", "cancel-c"]);
    assert!(std::fs::read_to_string(&session.path)
        .unwrap()
        .ends_with('\n'));
}

#[tokio::test]
async fn export_marks_incomplete_assistant_message_with_reason() {
    // Persist an incomplete assistant message and verify the human-readable
    // export shows the marker and reason so a user reading the transcript
    // understands why the run stopped.
    let tmp = tempfile::tempdir().unwrap();
    let mut session =
        diet_soda::session::Session::open(tmp.path(), Some("export-incomplete")).unwrap();
    session
        .record_message("main", Message::new("user", "ask"))
        .unwrap();
    session
        .record_message(
            "main",
            Message::incomplete_assistant("partial answer", "idle timeout: no chunk within 1s"),
        )
        .unwrap();
    let exports = tmp.path().join("exports");
    let path = session.export(&exports).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("partial answer"));
    assert!(text.contains("[incomplete response: idle timeout: no chunk within 1s]"));
    // Tool calls / native fragments were never written, and the marker
    // line lives next to the partial text without showing JSON scaffolds.
    assert!(!text.contains("tool_calls"));
}

#[tokio::test]
async fn status_events_follow_connecting_waiting_streaming_with_first_data() {
    // Capture the ordered `UiEvent::Status` stream and verify the
    // documented sequence: Connecting → Waiting → Streaming (with first data).
    // Each phase must appear exactly once for the request, even though the
    // body delivers several text deltas. The streaming status must report
    // first data rather than assuming the first data is a text token.
    let mut body = String::new();
    for text in ["first ", "second ", "third "] {
        body.push_str(&format!(
            "data: {}\r\n\r\n",
            json!({"choices": [{"delta": {"content": text}}]})
        ));
    }
    body.push_str(&format!(
        "data: {}\r\n\r\n",
        json!({"choices": [], "usage": {"prompt_tokens": 1, "completion_tokens": 1}})
    ));
    body.push_str("data: [DONE]\r\n\r\n");
    let server = server(vec![Reply {
        status: 200,
        content_type: "text/event-stream".into(),
        body,
        headers: vec![],
        header_delay: None,
        chunk_delay: None,
        stall: None,
    }])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let config = config(&server.url, tmp.path());
    let provider = RemoteProvider::new(config.providers["openrouter"].clone()).unwrap();
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    provider
        .stream(
            request(config.model.clone()),
            &events_tx,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    drop(events_tx);
    let mut statuses: Vec<String> = vec![];
    let mut events = vec![];
    while let Ok(event) = events_rx.try_recv() {
        if let UiEvent::Status { context, text } = &event {
            assert_eq!(context, "test");
            statuses.push(text.clone());
        }
        events.push(event);
    }
    assert_eq!(
        statuses.first().map(String::as_str),
        Some(phases::CONNECTING),
        "first status must be Connecting; got: {statuses:?}"
    );
    assert_eq!(
        statuses.get(1).map(String::as_str),
        Some(phases::WAITING),
        "second status must be Waiting before first data; got: {statuses:?}"
    );
    let streaming = statuses
        .get(2)
        .expect("streaming status must be present; got: {statuses:?}");
    assert!(
        streaming.starts_with(phases::STREAMING_PREFIX),
        "third status must be the streaming phase with first data; got: {streaming}"
    );
    assert!(
        streaming.contains("first data "),
        "streaming status must include first data; got: {streaming}"
    );
    let phase_counts = statuses
        .iter()
        .filter(|s| **s == phases::CONNECTING || **s == phases::WAITING)
        .count();
    assert_eq!(
        phase_counts, 2,
        "Connecting and Waiting must each appear exactly once; got: {statuses:?}"
    );
    assert!(
        statuses
            .iter()
            .filter(|s| s.starts_with(phases::STREAMING_PREFIX))
            .count()
            == 1,
        "Streaming must appear exactly once; got: {statuses:?}"
    );
    let streaming_index = events
        .iter()
        .position(|event| {
            matches!(event, UiEvent::Status { context, text } if context == "test" && text.starts_with(phases::STREAMING_PREFIX))
        })
        .expect("streaming status must be present in the full event stream");
    let first_delta_index = events
        .iter()
        .position(|event| matches!(event, UiEvent::Delta { .. }))
        .expect("normal text stream must emit a Delta");
    assert!(
        streaming_index < first_delta_index,
        "Streaming must precede the first Delta; got events: {events:?}"
    );
}

#[tokio::test]
async fn provider_wire_serialization_omits_incomplete_field() {
    // Wire payloads must never carry the `incomplete` marker; partial
    // assistant messages must not appear in subsequent provider requests.
    // The openai and anthropic helpers both build the wire payload from
    // explicit fields, so this assertion guards any future regression that
    // switches to a serde derive that serializes every struct field.
    let mut message = Message::incomplete_assistant("partial", "idle timeout");
    message.tool_calls.push(diet_soda::model::ToolCall {
        id: "tool-1".into(),
        name: "noop".into(),
        arguments: "{}".into(),
    });
    let openai = diet_soda::provider::openai_messages("system", &[message.clone()]);
    let anthropic = diet_soda::provider::anthropic_messages(&[message]);
    let openai_str = serde_json::to_string(&openai).unwrap();
    let anthropic_str = serde_json::to_string(&anthropic).unwrap();
    assert!(
        !openai_str.contains("incomplete"),
        "openai wire payload must not include incomplete; got: {openai_str}"
    );
    assert!(
        !anthropic_str.contains("incomplete"),
        "anthropic wire payload must not include incomplete; got: {anthropic_str}"
    );
}
