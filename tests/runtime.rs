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
    // write_file no longer prompts for can_edit agents by default; an
    // explicit approval_tools entry is required to force the prompt this
    // test exercises (disable-while-pending).
    test_config.approval_tools = vec!["write_file".into()];
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
    let outside = outside_tempdir();
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
    let outside = outside_tempdir();
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
    let outside = outside_tempdir();
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
        .contains("Command not found"));
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
async fn large_tool_result_reaches_model_untruncated() {
    let tmp = tempfile::tempdir().unwrap();
    let big = format!("{}TAILMARKER9", "0123456789".repeat(15_000));
    std::fs::write(tmp.path().join("big.txt"), &big).unwrap();
    let mut server = server(vec![
        tool_call("shell", json!({"command":"cat","args":["big.txt"]})),
        answer("done"),
    ])
    .await;
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
            assert_eq!(result.unwrap().unwrap(), "done");
        }
    }
    server.requests.recv().await.unwrap();
    let followup = server.requests.recv().await.unwrap();
    assert!(followup.body.contains("TAILMARKER9"));
    // 150 KB result is under the default cap: no tool re-read marker and no
    // engine head/tail wrap ("original_bytes" is the wrap's unique field).
    assert!(!followup.body.contains("[truncated: showing"));
    assert!(!followup.body.contains("original_bytes"));
}

#[tokio::test]
async fn custom_tool_without_max_output_bytes_uses_huge_default() {
    let tmp = tempfile::tempdir().unwrap();
    let big = format!("{}TAILMARKER9", "0123456789".repeat(15_000));
    std::fs::write(tmp.path().join("big.txt"), &big).unwrap();
    let mut server = server(vec![tool_call("bigcat", json!({})), answer("done")]).await;
    let mut test_config = config(&server.url, tmp.path());
    let tool: ToolConfig = serde_json::from_value(json!({
        "type":"command",
        "command":"cat",
        "args":["big.txt"],
        "description":"cat big",
        "hitl":false,
        "destructive":false,
        "input_schema":{"type":"object","properties":{}}
    }))
    .unwrap();
    assert_eq!(tool.max_output_bytes, 100_000_000);
    test_config.tools.insert("bigcat".into(), tool);
    let (engine, mut events) = engine(test_config);
    let runner = engine.clone();
    let mut task = tokio::spawn(async move {
        runner
            .turn(
                "run it".into(),
                Selection::default(),
                CancellationToken::new(),
            )
            .await
    });
    tokio::select! {
        Some(UiEvent::Approval { reply, .. }) = events.recv() => {
            reply.send(Decision::Reject).unwrap();
            panic!("tool unexpectedly requested approval");
        }
        result = &mut task => {
            assert_eq!(result.unwrap().unwrap(), "done");
        }
    }
    server.requests.recv().await.unwrap();
    let followup = server.requests.recv().await.unwrap();
    assert!(followup.body.contains("TAILMARKER9"));
    // 150 KB result under the 100 MB tool default and the 512 KB engine cap:
    // no tool re-read marker and no engine head/tail wrap.
    assert!(!followup.body.contains("[truncated: showing"));
    assert!(!followup.body.contains("original_bytes"));
}

#[cfg(unix)]
#[tokio::test]
async fn builtin_response_caps_use_shared_safety_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let big = format!("{}TAILMARKER9", "0123456789".repeat(15_000));
    std::fs::write(tmp.path().join("big.txt"), &big).unwrap();
    let big_read = format!("{}READTAIL9", "x".repeat(1_500_000));
    std::fs::write(tmp.path().join("bigread.txt"), &big_read).unwrap();
    let config = Config {
        workspace: tmp.path().into(),
        bash_permissions: "none".into(),
        max_tool_output_bytes: 100_000_000,
        ..Config::default()
    };

    let shell = tools::builtin(
        "shell",
        &json!({"command":"cat","args":["big.txt"]}),
        &config,
        &CancellationToken::new(),
        false,
    )
    .await
    .unwrap();
    assert_eq!(shell["truncated"], false);
    let stdout = shell["stdout"].as_str().unwrap();
    assert!(stdout.contains("TAILMARKER9"));
    assert!(stdout.len() >= 150_000);

    let read_file = tools::builtin(
        "read_file",
        &json!({"path":"bigread.txt"}),
        &config,
        &CancellationToken::new(),
        false,
    )
    .await
    .unwrap();
    assert_eq!(read_file["truncated"], false);
    let content = read_file["content"].as_str().unwrap();
    assert!(content.ends_with("READTAIL9"));
    assert_eq!(content.len(), big_read.len());
}

#[tokio::test]
async fn approved_outside_shell_call_runs_without_a_standing_grant() {
    let outside = outside_tempdir();
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
    let outside = outside_tempdir();
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
    let outside = outside_tempdir();
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
async fn subagent_has_isolated_messages_and_keeps_its_own_tool_scope() {
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
            can_edit: true,
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
    let mut child_tools: Vec<String> = child["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap().to_string())
        .collect();
    child_tools.sort();
    assert_eq!(
        child_tools,
        vec!["web_fetch".to_string(), "write_file".to_string()]
    );
    let session = engine.session.lock().await;
    assert_eq!(session.messages.len(), 4);
    assert_eq!(session.spend.microusd, 369);
}
#[tokio::test]
async fn empty_final_assistant_turn_with_no_content_is_an_error() {
    let server = server(vec![answer("")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (engine, _events) = engine(config(&server.url, tmp.path()));
    let error = engine
        .turn(
            "x".into(),
            Selection::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("empty response"),
        "blank final turn must name the empty response; got: {rendered}"
    );
}
#[tokio::test]
async fn empty_final_whitespace_only_assistant_turn_is_an_error() {
    let server = server(vec![answer("  ")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (engine, _events) = engine(config(&server.url, tmp.path()));
    let error = engine
        .turn(
            "x".into(),
            Selection::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("empty response"),
        "whitespace-only final turn counts as empty; got: {rendered}"
    );
}
#[tokio::test]
async fn empty_final_nonempty_assistant_turn_still_succeeds() {
    let server = server(vec![answer("text")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (engine, _events) = engine(config(&server.url, tmp.path()));
    let result = engine
        .turn(
            "x".into(),
            Selection::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result, "text");
}
#[tokio::test]
async fn empty_final_blank_turn_is_not_pushed_to_history() {
    let mut server = server(vec![answer(""), answer("ok")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (engine, _events) = engine(config(&server.url, tmp.path()));
    assert!(
        engine
            .turn(
                "first".into(),
                Selection::default(),
                CancellationToken::new(),
            )
            .await
            .is_err(),
        "the blank first turn must error"
    );
    assert_eq!(
        engine
            .turn(
                "second".into(),
                Selection::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap(),
        "ok"
    );
    // Drop the first request; the second request's body is the retry.
    server.requests.recv().await.unwrap();
    let retry: Value = serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let no_empty_assistant = retry["messages"]
        .as_array()
        .unwrap()
        .iter()
        .all(|m| m["role"] != "assistant" || !m["content"].as_str().unwrap_or("").trim().is_empty());
    assert!(
        no_empty_assistant,
        "a discarded blank turn must not replay as an empty assistant message: {retry}"
    );
}
#[tokio::test]
async fn empty_final_child_delegation_surfaces_tool_error_in_parent() {
    let mut server = server(vec![
        tool_call("delegate", json!({"agent":"child","prompt":"go"})),
        answer(""),
        answer(""),
        answer("parent done"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.agents.insert(
        "parent".into(),
        AgentConfig {
            tools: Some(vec!["delegate".into()]),
            ..AgentConfig::default()
        },
    );
    config.agents.insert(
        "child".into(),
        AgentConfig {
            tools: Some(vec!["web_fetch".into()]),
            ..AgentConfig::default()
        },
    );
    let (engine, _events) = engine(config);
    let result = engine
        .turn(
            "start".into(),
            Selection {
                agent: Some("parent".into()),
                ..Selection::default()
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result, "parent done");
    // Request 1 is the parent's delegate call, request 2 the child's blank
    // final turn, request 3 the child's rescue prompt (also blank), and
    // request 4 the parent resuming with the tool error surfaced from the
    // second empty turn.
    server.requests.recv().await.unwrap();
    server.requests.recv().await.unwrap();
    server.requests.recv().await.unwrap();
    let final_request: Value =
        serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let tool = final_request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("parent must receive a tool result for the failed child");
    let content = tool["content"].as_str().unwrap();
    assert!(
        content.contains("empty response"),
        "the child's blank final turn must surface as a delegate tool error; got: {content}"
    );
    assert!(
        !content.contains("\"result\":\"\""),
        "a blank child must not report success as an empty result; got: {content}"
    );
}
#[tokio::test]
async fn empty_final_child_is_rescued_with_summary_prompt() {
    // When the child returns a blank final turn exactly once, the engine
    // must issue a single rescue call carrying the documented follow-up
    // prompt so the parent can recover a summary instead of seeing a tool
    // error. The third scripted reply ("child summary") must drive the
    // rescue call to a real answer.
    let mut server = server(vec![
        tool_call("delegate", json!({"agent":"child","prompt":"go"})),
        answer(""),
        answer("child summary"),
        answer("parent done"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.agents.insert(
        "parent".into(),
        AgentConfig {
            tools: Some(vec!["delegate".into()]),
            ..AgentConfig::default()
        },
    );
    config.agents.insert(
        "child".into(),
        AgentConfig {
            tools: Some(vec!["web_fetch".into()]),
            ..AgentConfig::default()
        },
    );
    let (engine, _events) = engine(config);
    let result = engine
        .turn(
            "start".into(),
            Selection {
                agent: Some("parent".into()),
                ..Selection::default()
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result, "parent done");
    // Request 1 is the parent's delegate call, request 2 the child's blank
    // final turn, request 3 the child's rescue request, and request 4 the
    // parent resuming with the rescued summary as a tool result.
    server.requests.recv().await.unwrap();
    server.requests.recv().await.unwrap();
    let rescue: Value =
        serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let messages = rescue["messages"].as_array().unwrap();
    let rescue_prompt = "Your previous reply reached the parent as an empty response. Reply now with a concise summary of the task: what you did, what you found, files/commands touched.";
    assert!(
        messages
            .iter()
            .any(|m| m["role"] == "user" && m["content"] == rescue_prompt),
        "rescue request must carry the documented follow-up prompt verbatim: {rescue_prompt}"
    );
    let final_request: Value =
        serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let tool = final_request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("parent must receive a tool result for the rescued child");
    let content = tool["content"].as_str().unwrap();
    assert!(
        content.contains("child summary"),
        "rescued child summary must surface as the delegate tool result; got: {content}"
    );
    assert!(
        !content.contains("empty response"),
        "a rescued child must not surface the empty-response error; got: {content}"
    );
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
        output_cap: 128_000,
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
                output_cap: 128_000,
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
        assert_eq!(body[field], 128_000);
    }
}

#[tokio::test]
async fn explicit_max_tokens_overrides_output_cap() {
    let mut server = server(vec![answer("context-capped response")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.providers.get_mut("openrouter").unwrap().kind = ProviderKind::Openai;
    config.model.max_tokens = Some(40_000);
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
        "context-capped response"
    );
    let request = server.requests.recv().await.unwrap();
    let body: Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["max_completion_tokens"], 40_000);
}

#[tokio::test]
async fn context_window_sets_input_budget_not_output_cap() {
    let mut server = server(vec![answer("windowed response")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.providers.get_mut("openrouter").unwrap().kind = ProviderKind::Openai;
    config.model.context_window = Some(200_000);
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
        "windowed response"
    );
    let request = server.requests.recv().await.unwrap();
    let body: Value = serde_json::from_str(&request.body).unwrap();
    // context_window no longer derives the output cap; the global 128k applies.
    assert_eq!(body["max_completion_tokens"], 128_000);
}

#[tokio::test]
async fn list_models_discovery_caps_output_tokens() {
    let mut server = server(vec![
        Reply::json(json!({
            "data": [{
                "id": "openai/gpt-4.1-mini",
                "context_length": 128000,
                "top_provider": {"max_completion_tokens": 4000}
            }],
            "has_more": false
        })),
        answer("discovered-capped response"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.model.model = "openai/gpt-4.1-mini".into();
    let model_id = config.model.model.clone();
    config.providers.get_mut("openrouter").unwrap().kind = ProviderKind::Openai;
    let (engine, _) = engine(config);
    let provider = engine.config.read().await.providers["openrouter"].clone();
    let models = engine.list_models(provider).await.unwrap();
    let model = models
        .iter()
        .find(|model| model.id == model_id)
        .unwrap();
    assert_eq!(model.context_window, Some(128000));
    assert_eq!(model.max_output, Some(4000));

    assert_eq!(
        engine
            .turn(
                "test".into(),
                Selection::default(),
                CancellationToken::new()
            )
            .await
            .unwrap(),
        "discovered-capped response"
    );
    let first = server.requests.recv().await.unwrap();
    assert!(first.headers.starts_with("GET /models"));
    let second = server.requests.recv().await.unwrap();
    assert!(second.headers.starts_with("POST /chat/completions"));
    assert_eq!(server.count.load(Ordering::SeqCst), 2);
    let body: Value = serde_json::from_str(&second.body).unwrap();
    assert_eq!(body["max_completion_tokens"], 4000);
}

#[tokio::test]
async fn prefetch_limits_fills_cache_before_turn() {
    let mut server = server(vec![
        Reply::json(json!({
            "data": [{
                "id": "openai/gpt-4.1-mini",
                "context_length": 128000,
                "top_provider": {"max_completion_tokens": 4000}
            }],
            "has_more": false
        })),
        answer("ok"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.model.model = "openai/gpt-4.1-mini".into();
    config.providers.get_mut("openrouter").unwrap().kind = ProviderKind::Openai;
    let (engine, _) = engine(config);
    engine.prefetch_limits().await;
    assert_eq!(server.count.load(Ordering::SeqCst), 1);
    assert_eq!(
        engine
            .turn(
                "test".into(),
                Selection::default(),
                CancellationToken::new()
            )
            .await
            .unwrap(),
        "ok"
    );
    let first = server.requests.recv().await.unwrap();
    assert!(first.headers.starts_with("GET /models"));
    let second = server.requests.recv().await.unwrap();
    assert!(second.headers.starts_with("POST /chat/completions"));
    let body: Value = serde_json::from_str(&second.body).unwrap();
    assert_eq!(body["max_completion_tokens"], 4000);
}

#[tokio::test]
async fn prefetch_limits_is_a_noop_when_disabled() {
    let server = server(vec![]).await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.discover_model_limits = false;
    let (engine, _) = engine(config);
    engine.prefetch_limits().await;
    assert_eq!(server.count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn prefetch_limits_ignores_catalog_errors() {
    let mut server = server(vec![
        Reply {
            status: 500,
            content_type: "text/plain".into(),
            body: "boom".into(),
            headers: vec![],
            header_delay: None,
            chunk_delay: None,
            stall: None,
        },
        answer("ok"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.providers.get_mut("openrouter").unwrap().kind = ProviderKind::Openai;
    let (engine, _) = engine(config);
    engine.prefetch_limits().await;
    assert_eq!(
        engine
            .turn(
                "test".into(),
                Selection::default(),
                CancellationToken::new()
            )
            .await
            .unwrap(),
        "ok"
    );
    let first = server.requests.recv().await.unwrap();
    assert!(first.headers.starts_with("GET /models"));
    let second = server.requests.recv().await.unwrap();
    assert!(second.headers.starts_with("POST /chat/completions"));
    let body: Value = serde_json::from_str(&second.body).unwrap();
    assert_eq!(body["max_completion_tokens"], 128_000);
}

#[tokio::test]
async fn prefetch_limits_ignores_missing_api_key() {
    let server = server(vec![answer("ok")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.providers.get_mut("openrouter").unwrap().api_key_env =
        Some("DIET_SODA_TEST_DEFINITELY_UNSET_KEY".into());
    let (engine, _) = engine(config);
    engine.prefetch_limits().await;
    assert_eq!(server.count.load(Ordering::SeqCst), 0);
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
        output_cap: 128_000,
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
    //   2. Push the marker and user-role rescue note onto the display timeline,
    //   3. Keep the marker out of history but record the rescue note there,
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
        message.contains("idle timeout") || message.contains("Provider response incomplete"),
        "expected incomplete-stream message; got: {message}"
    );
    let sessions_dir = engine.config.read().await.sessions_dir.clone();
    let session_path = {
        let session = engine.session.lock().await;
        let messages = display_messages(&session.display_events);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1].message.role, "assistant");
        assert!(messages[1].message.incomplete.is_some());
        assert_eq!(messages[2].message.role, "user");
        assert!(messages[2]
            .message
            .content
            .contains("Continue the task and re-issue any interrupted tool call"));
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
            session.messages.len() == 2
                && session.messages[0].role == "user"
                && session.messages[1].role == "user"
                && session.messages[1]
                    .content
                    .contains("Continue the task and re-issue any interrupted tool call"),
            "incomplete assistant must stay out of history and the rescue note must be recorded; got: {:?}",
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
        reopened.messages.len() == 2
            && reopened.messages[0].role == "user"
            && reopened.messages[1].role == "user"
            && reopened.messages[1]
                .content
                .contains("Continue the task and re-issue any interrupted tool call"),
        "reopen must keep the incomplete assistant out of history and retain the rescue note; got: {:?}",
        reopened.messages
    );
    let messages = display_messages(&reopened.display_events);
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1].message.role, "assistant");
    assert!(messages[1].message.incomplete.is_some());
    assert_eq!(messages[2].message.role, "user");
    assert!(messages[2]
        .message
        .content
        .contains("Continue the task and re-issue any interrupted tool call"));
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
    let openai = diet_soda::provider::openai_messages(
        "system",
        &[message.clone()],
        &diet_soda::config::ProviderKind::Openrouter,
    );
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

#[cfg(unix)]
#[tokio::test]
async fn shell_builtin_blocks_deny_inside_wrapped_script() {
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let marker = workspace.path().join("keep.txt");
    std::fs::write(&marker, "keep").unwrap();
    let config = Config {
        workspace: workspace.path().into(),
        config_dir: config_dir.path().into(),
        ..Config::default()
    };

    let error = tools::builtin(
        "shell",
        &json!({"command":"bash","args":["-c","cargo --version && rm -rf keep.txt"]}),
        &config,
        &CancellationToken::new(),
        false,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("Blocked"), "{error}");
    assert!(marker.exists(), "denied wrapped script must not spawn");
}

#[cfg(unix)]
#[tokio::test]
async fn shell_builtin_rejects_outside_read_inside_wrapped_script() {
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let config = Config {
        workspace: workspace.path().into(),
        config_dir: config_dir.path().into(),
        ..Config::default()
    };

    let error = tools::builtin(
        "shell",
        &json!({"command":"bash","args":["-c","cat /etc/hosts"]}),
        &config,
        &CancellationToken::new(),
        false,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("outside"), "{error}");
}

#[cfg(unix)]
#[tokio::test]
async fn shell_builtin_rejects_sed_write_outside_via_script_filename() {
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let outside = outside_tempdir();
    let source = workspace.path().join("f.txt");
    std::fs::write(&source, "a\n").unwrap();
    let output = outside.path().join("pwned.txt");
    let config = Config {
        workspace: workspace.path().into(),
        config_dir: config_dir.path().into(),
        ..Config::default()
    };

    let error = tools::builtin(
        "shell",
        &json!({"command":"sed","args":[format!("s/a/b/w {}", output.display()),"f.txt"]}),
        &config,
        &CancellationToken::new(),
        false,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("outside"), "{error}");
    assert!(!output.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn shell_builtin_runs_allowed_wrapped_script() {
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let marker = workspace.path().join("wrapped-shell-marker.txt");
    std::fs::write(&marker, "visible").unwrap();
    let config = Config {
        workspace: workspace.path().into(),
        config_dir: config_dir.path().into(),
        ..Config::default()
    };

    let result = tools::builtin(
        "shell",
        &json!({"command":"bash","args":["-c","/bin/ls -la"]}),
        &config,
        &CancellationToken::new(),
        false,
    )
    .await
    .unwrap();
    assert_eq!(result["exit_code"], 0);
    assert!(result["stdout"]
        .as_str()
        .unwrap()
        .contains("wrapped-shell-marker.txt"));
}

// -------------------------------------------------------------------------
// Max-output-token truncation. A stream that stops at the output cap is not
// a complete answer: reasoning models can spend the whole cap and emit
// nothing. All of these names contain `output_limit` for focused runs.
// -------------------------------------------------------------------------

#[tokio::test]
async fn provider_output_limit_finish_reason_length_is_error() {
    let server = server(vec![Reply::sse(
        vec![json!({"choices":[{"delta":{"content":""},"finish_reason":"length"}]})],
        true,
    )])
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
        Ok(_) => panic!("a length-truncated response must error"),
        Err(error) => error,
    };
    assert!(
        error.downcast_ref::<IncompleteStreamError>().is_some(),
        "truncation must surface IncompleteStreamError"
    );
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("max output token limit"),
        "error must name the output token limit; got: {rendered}"
    );
}

#[tokio::test]
async fn provider_output_limit_keeps_partial_text() {
    let server = server(vec![Reply::sse(
        vec![
            json!({"choices":[{"delta":{"content":"partial"}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"length"}]}),
        ],
        true,
    )])
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
        Ok(_) => panic!("a length-truncated response must error"),
        Err(error) => error,
    };
    let incomplete = error
        .downcast_ref::<IncompleteStreamError>()
        .expect("truncation must surface IncompleteStreamError");
    assert!(
        incomplete.message.content.contains("partial"),
        "partial visible text must survive; got: {:?}",
        incomplete.message.content
    );
    assert!(
        incomplete.reason.contains("max output token limit"),
        "reason must name the output token limit; got: {}",
        incomplete.reason
    );
}

#[tokio::test]
async fn provider_output_limit_truncated_tool_call_is_error() {
    for done in [true, false] {
        let server = server(vec![Reply::sse(
            vec![json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":"{\"path\":\"x"}}]},"finish_reason":"length"}]})],
            done,
        )])
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
            Ok(_) => panic!("a truncated tool call must error (done={done})"),
            Err(error) => error,
        };
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("max output token limit"),
            "done={done}; truncation must be reported; got: {rendered}"
        );
        assert!(
            !rendered.contains("stream ended before completion event"),
            "done={done}; truncation must win over EOF handling; got: {rendered}"
        );
    }

    // An output-capped stream with no complete calls still errors at the
    // engine boundary and must not execute even a partially streamed write.
    let partial_call = Reply::sse(
        vec![json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"cut-only","function":{"name":"write_file","arguments":"{\"path\":\"should-not-exist.txt\",\"content\":\"partial"}}]},"finish_reason":"length"}]})],
        true,
    );
    let server = server(vec![partial_call]).await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.agents.insert(
        "writer".into(),
        serde_json::from_value(json!({"can_edit":true,"tools":["write_file"]})).unwrap(),
    );
    let (engine, _) = engine(config);
    let error = engine
        .turn(
            "write".into(),
            Selection { agent: Some("writer".into()), ..Selection::default() },
            CancellationToken::new(),
        )
        .await
        .expect_err("a stream with no complete call must remain incomplete");
    assert!(error.downcast_ref::<IncompleteStreamError>().is_some());
    assert!(!tmp.path().join("should-not-exist.txt").exists());
    let session = engine.session.lock().await;
    assert!(session.messages.iter().all(|message| message.role != "tool"));
}

#[tokio::test]
async fn provider_degenerate_tool_call_is_incomplete_with_recorded_usage() {
    // Protocol completes ([DONE]) and tokens are reported, but the streamed
    // tool call has an id with no name: the sanity check must still reject it
    // while recording the provider's billed usage, matching the truncation
    // policy.
    let server = server(vec![Reply::sse(
        vec![
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_degen","function":{"arguments":"{}"}}]},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}),
        ],
        true,
    )])
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
        Ok(_) => panic!("a degenerate tool call must error"),
        Err(error) => error,
    };
    let incomplete = error
        .downcast_ref::<IncompleteStreamError>()
        .expect("degenerate tool call must surface IncompleteStreamError");
    assert_eq!(
        incomplete.reason, "incomplete tool call from provider",
        "reason must name the degenerate tool call; got: {}",
        incomplete.reason
    );
    let usage = incomplete
        .usage
        .as_ref()
        .expect("a protocol-complete stream with reported tokens must carry usage");
    assert_eq!(
        usage.output_tokens, 5,
        "recorded usage must hold the provider-reported completion tokens"
    );
}

#[tokio::test]
async fn transient_stream_failure_is_retried_once_and_recovers() {
    // The first attempt streams a degenerate tool call (id without name),
    // which the provider rejects as "incomplete tool call from provider" —
    // a retryable stream reason. The engine must retry exactly once, recover
    // with the second fixture, and record no rescue note in history.
    let server = server(vec![
        Reply::sse(
            vec![
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_degen","function":{"arguments":"{}"}}]},"finish_reason":"tool_calls"}]}),
                json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}),
            ],
            true,
        ),
        answer("recovered"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let config = config(&server.url, tmp.path());
    let (engine, _events) = engine(config);
    let result = engine
        .turn("test".into(), Selection::default(), CancellationToken::new())
        .await
        .expect("a single transient stream failure must be retried and recover");
    assert_eq!(result, "recovered");
    assert_eq!(
        server.count.load(Ordering::SeqCst),
        2,
        "the retry must hit the server exactly once more"
    );
    let session = engine.session.lock().await;
    let notes: Vec<&String> = session
        .messages
        .iter()
        .filter(|message| {
            message.role == "user"
                && (message.content.starts_with("Your previous response was truncated")
                    || message.content.starts_with("The provider stream failed"))
        })
        .map(|message| &message.content)
        .collect();
    assert!(
        notes.is_empty(),
        "a recovered turn must not record a rescue note; got: {notes:?}"
    );
    assert_eq!(
        session.spend.output_tokens, 10,
        "billed output tokens must include the failed attempt (5) plus the \
         recovered answer (5); got spend: {:?}",
        session.spend
    );
}

#[tokio::test]
async fn transient_stream_failure_retry_exhausted_records_stream_failure_note() {
    // Both attempts stream the same degenerate tool call. The one-shot
    // retry exhausts, the turn must error, and the recorded rescue note must
    // be reason-accurate: a stream failure, NOT an output-limit truncation.
    let degenerate = || {
        Reply::sse(
            vec![
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_degen","function":{"arguments":"{}"}}]},"finish_reason":"tool_calls"}]}),
                json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}),
            ],
            true,
        )
    };
    let server = server(vec![degenerate(), degenerate()]).await;
    let tmp = tempfile::tempdir().unwrap();
    let config = config(&server.url, tmp.path());
    let (engine, _events) = engine(config);
    engine
        .turn("test".into(), Selection::default(), CancellationToken::new())
        .await
        .expect_err("two consecutive transient stream failures must surface as a turn error");
    assert_eq!(
        server.count.load(Ordering::SeqCst),
        2,
        "exactly the initial attempt and its one retry may reach the server"
    );
    let session = engine.session.lock().await;
    let stream_notes: Vec<&String> = session
        .messages
        .iter()
        .filter(|message| {
            message.role == "user"
                && message
                    .content
                    .starts_with("The provider stream failed before your previous response completed")
        })
        .map(|message| &message.content)
        .collect();
    assert_eq!(
        stream_notes.len(),
        1,
        "an exhausted stream-failure retry must record one stream-failure note; got: {:?}",
        session.messages
    );
    let truncation_notes: Vec<&String> = session
        .messages
        .iter()
        .filter(|message| {
            message.role == "user"
                && message
                    .content
                    .starts_with("Your previous response was truncated")
        })
        .map(|message| &message.content)
        .collect();
    assert!(
        truncation_notes.is_empty(),
        "a stream failure must not be described as an output-limit truncation; got: {truncation_notes:?}"
    );
}

#[tokio::test]
async fn output_limit_truncation_is_not_retried() {
    // finish_reason "length" is NOT in the retryable set: a truncated turn
    // errors after a single request, records the output-limit rescue note,
    // and never re-hits the server.
    let server = server(vec![Reply::sse(
        vec![json!({"choices":[{"delta":{"content":""},"finish_reason":"length"}]})],
        true,
    )])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let config = config(&server.url, tmp.path());
    let (engine, _events) = engine(config);
    engine
        .turn("test".into(), Selection::default(), CancellationToken::new())
        .await
        .expect_err("a length-truncated turn must error");
    assert_eq!(
        server.count.load(Ordering::SeqCst),
        1,
        "output-limit truncation must not trigger a retry"
    );
    let session = engine.session.lock().await;
    let notes: Vec<&String> = session
        .messages
        .iter()
        .filter(|message| {
            message.role == "user"
                && message
                    .content
                    .starts_with("Your previous response was truncated at the output token limit")
        })
        .map(|message| &message.content)
        .collect();
    assert_eq!(
        notes.len(),
        1,
        "a truncated turn must record the output-limit rescue note exactly once; got: {:?}",
        session.messages
    );
}

#[tokio::test]
async fn provider_output_limit_stop_and_tool_calls_still_succeed() {
    // finish_reason "stop" with visible text remains a normal answer.
    let server = server(vec![Reply::sse(
        vec![json!({"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]})],
        true,
    )])
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
        .expect("finish_reason stop must succeed");
    assert_eq!(response.message.content, "done");

    // finish_reason "tool_calls" with a complete call remains a success.
    let server2 = support::server(vec![Reply::sse(
        vec![json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":"{\"path\":\"x\"}"}}]},"finish_reason":"tool_calls"}]})],
        true,
    )])
    .await;
    let tmp2 = tempfile::tempdir().unwrap();
    let config2 = support::config(&server2.url, tmp2.path());
    let provider2 = RemoteProvider::new(config2.providers["openrouter"].clone()).unwrap();
    let response = provider2
        .stream(
            request(config2.model.clone()),
            &events,
            &CancellationToken::new(),
        )
        .await
        .expect("finish_reason tool_calls with a complete call must succeed");
    assert_eq!(response.message.tool_calls[0].name, "read_file");
}

fn anthropic_config(url: &str) -> ProviderConfig {
    ProviderConfig {
        kind: ProviderKind::Anthropic,
        base_url: url.into(),
        api_key_env: None,
        headers: std::collections::BTreeMap::new(),
        timeout_seconds: 5,
        allow_private_networks: true,
    }
}

#[tokio::test]
async fn anthropic_output_limit_stop_reason_max_tokens_is_error() {
    let server = server(vec![Reply::sse(
        vec![
            json!({"type":"message_start","message":{"usage":{"input_tokens":10}}}),
            json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":4096}}),
            json!({"type":"message_stop"}),
        ],
        false,
    )])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let config = config(&server.url, tmp.path());
    let provider = RemoteProvider::new(anthropic_config(&server.url)).unwrap();
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let error = match provider
        .stream(
            request(config.model.clone()),
            &events,
            &CancellationToken::new(),
        )
        .await
    {
        Ok(_) => panic!("stop_reason max_tokens must error"),
        Err(error) => error,
    };
    assert!(error.downcast_ref::<IncompleteStreamError>().is_some());
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("max output token limit"),
        "error must name the output token limit; got: {rendered}"
    );

    // end_turn with text is a normal answer.
    let server_b = support::server(vec![Reply::sse(
        vec![
            json!({"type":"message_start","message":{"usage":{"input_tokens":5}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
            json!({"type":"message_stop"}),
        ],
        false,
    )])
    .await;
    let tmp_b = tempfile::tempdir().unwrap();
    let config_b = support::config(&server_b.url, tmp_b.path());
    let provider_b = RemoteProvider::new(anthropic_config(&server_b.url)).unwrap();
    let (events_b, _) = tokio::sync::mpsc::unbounded_channel();
    let response = provider_b
        .stream(
            request(config_b.model.clone()),
            &events_b,
            &CancellationToken::new(),
        )
        .await
        .expect("stop_reason end_turn must succeed");
    assert_eq!(response.message.content, "hi");

    // tool_use with a complete call is a normal success.
    let server_c = support::server(vec![Reply::sse(
        vec![
            json!({"type":"message_start","message":{"usage":{"input_tokens":5}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tool1","name":"read_file","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"x\"}"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":2}}),
            json!({"type":"message_stop"}),
        ],
        false,
    )])
    .await;
    let tmp_c = tempfile::tempdir().unwrap();
    let config_c = support::config(&server_c.url, tmp_c.path());
    let provider_c = RemoteProvider::new(anthropic_config(&server_c.url)).unwrap();
    let (events_c, _) = tokio::sync::mpsc::unbounded_channel();
    let response = provider_c
        .stream(
            request(config_c.model.clone()),
            &events_c,
            &CancellationToken::new(),
        )
        .await
        .expect("stop_reason tool_use must succeed");
    assert_eq!(response.message.tool_calls[0].name, "read_file");
}

#[tokio::test]
async fn engine_output_limit_truncated_turn_is_error() {
    let server = server(vec![Reply::sse(
        vec![json!({"choices":[{"delta":{"content":""},"finish_reason":"length"}]})],
        true,
    )])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let config = config(&server.url, tmp.path());
    let (engine, _events) = engine(config);
    let error = match engine
        .turn(
            "x".into(),
            Selection::default(),
            CancellationToken::new(),
        )
        .await
    {
        Ok(_) => panic!("a length-truncated turn must error"),
        Err(error) => error,
    };
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("truncated"),
        "engine error must mention truncation; got: {rendered}"
    );
}

#[tokio::test]
async fn engine_output_limit_usage_records_final_spend_for_completed_stream() {
    // The provider completed the protocol ([DONE]) and reported final billed
    // usage, then the response was rejected as truncated. That usage must
    // still land in spend accounting instead of vanishing.
    let server = server(vec![Reply::sse(
        vec![
            json!({"choices":[{"delta":{"content":"partial answer"}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"length"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"cost":0.000123}}),
        ],
        true,
    )])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let config = config(&server.url, tmp.path());
    let (engine, _events) = engine(config);
    let error = engine
        .turn(
            "x".into(),
            Selection::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("a length-truncated turn must error");
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("truncated"),
        "engine error must mention truncation; got: {rendered}"
    );
    let session = engine.session.lock().await;
    assert_eq!(
        session.spend.microusd, 123,
        "final billed usage from a protocol-complete truncated stream must be recorded"
    );
}

#[tokio::test]
async fn engine_output_limit_cost_estimate_matches_success_path() {
    // Real providers omit the `cost` field, so a protocol-complete truncated
    // response would otherwise record tokens at $0 and count as unpriced. The
    // configured per-million prices must backfill the same estimate the
    // success path produces for an identical usage chunk.
    let truncated = server(vec![Reply::sse(
        vec![
            json!({"choices":[{"delta":{"content":"partial answer"}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"length"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}),
        ],
        true,
    )])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&truncated.url, tmp.path());
    config.model.input_usd_per_million = Some(1.0);
    config.model.output_usd_per_million = Some(2.0);
    let (engine, _events) = engine(config);
    let error = engine
        .turn(
            "x".into(),
            Selection::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("a length-truncated turn must error");
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("truncated"),
        "engine error must mention truncation; got: {rendered}"
    );
    let session = engine.session.lock().await;
    assert_eq!(
        session.spend.microusd, 20,
        "truncated turn must get the same price estimate as a successful one"
    );
    assert_eq!(session.spend.unpriced_requests, 0);
    assert!(session.spend.estimated);
    assert_eq!(session.spend.input_tokens, 10);
    assert_eq!(session.spend.output_tokens, 5);

    // The success path over the same usage chunk (finish_reason "stop") must
    // produce identical spend bookkeeping for the estimate to be equivalent.
    let success = server(vec![Reply::sse(
        vec![
            json!({"choices":[{"delta":{"content":"complete answer"},"finish_reason":"stop"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}),
        ],
        true,
    )])
    .await;
    let tmp_b = tempfile::tempdir().unwrap();
    let mut config_b = support::config(&success.url, tmp_b.path());
    config_b.model.input_usd_per_million = Some(1.0);
    config_b.model.output_usd_per_million = Some(2.0);
    let (engine_b, _events_b) = support::engine(config_b);
    engine_b
        .turn(
            "x".into(),
            Selection::default(),
            CancellationToken::new(),
        )
        .await
        .expect("finish_reason stop must succeed");
    let session_b = engine_b.session.lock().await;
    assert_eq!(session_b.spend.microusd, 20);
    assert_eq!(session_b.spend.unpriced_requests, 0);
    assert!(session_b.spend.estimated);
    assert_eq!(session_b.spend.input_tokens, 10);
    assert_eq!(session_b.spend.output_tokens, 5);
}

#[tokio::test]
async fn provider_output_limit_usage_is_some_with_done_and_none_without() {
    // Protocol-complete: `[DONE]` was seen after `finish_reason` length and a
    // usage chunk, so the error must carry the final billed usage.
    let server = server(vec![Reply::sse(
        vec![
            json!({"choices":[{"delta":{"content":"partial"}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"length"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"cost":0.000123}}),
        ],
        true,
    )])
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
        Ok(_) => panic!("a length-truncated response must error"),
        Err(error) => error,
    };
    let incomplete = error
        .downcast_ref::<IncompleteStreamError>()
        .expect("truncation must surface IncompleteStreamError");
    assert_eq!(
        incomplete.usage.as_ref().map(|usage| usage.output_tokens),
        Some(5),
        "a protocol-complete truncated stream must carry the final billed usage"
    );

    // No `[DONE]`: the protocol never completed, so any usage seen mid-stream
    // may be partial and must not be presented as final.
    let server_b = support::server(vec![Reply::sse(
        vec![
            json!({"choices":[{"delta":{"content":"partial"}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"length"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"cost":0.000123}}),
        ],
        false,
    )])
    .await;
    let tmp_b = tempfile::tempdir().unwrap();
    let config_b = support::config(&server_b.url, tmp_b.path());
    let provider_b = RemoteProvider::new(config_b.providers["openrouter"].clone()).unwrap();
    let (events_b, _) = tokio::sync::mpsc::unbounded_channel();
    let error_b = match provider_b
        .stream(
            request(config_b.model.clone()),
            &events_b,
            &CancellationToken::new(),
        )
        .await
    {
        Ok(_) => panic!("a length-truncated response must error"),
        Err(error) => error,
    };
    let incomplete_b = error_b
        .downcast_ref::<IncompleteStreamError>()
        .expect("truncation must surface IncompleteStreamError");
    assert!(
        incomplete_b.usage.is_none(),
        "a stream without the completion event must not carry final usage"
    );
}

#[tokio::test]
async fn engine_output_limit_executes_only_complete_tool_calls_and_records_retry_note() {
    let truncated = Reply::sse(
        vec![json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"write-complete","function":{"name":"write_file","arguments":"{\"path\":\"complete.txt\",\"content\":\"saved\"}"}},
            {"index":1,"id":"write-cut","function":{"name":"write_file","arguments":"{\"path\":\"cut.txt\",\"content\":\"not finished"}}
        ]},"finish_reason":"length"}]})],
        true,
    );
    let mut server = server(vec![truncated, answer("finished")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.agents.insert(
        "writer".into(),
        serde_json::from_value(json!({"can_edit":true,"tools":["write_file"]})).unwrap(),
    );
    let (engine, _) = engine(config);
    assert_eq!(
        engine.turn(
            "write both".into(),
            Selection { agent: Some("writer".into()), ..Selection::default() },
            CancellationToken::new(),
        ).await.unwrap(),
        "finished"
    );

    assert_eq!(std::fs::read_to_string(tmp.path().join("complete.txt")).unwrap(), "saved");
    assert!(!tmp.path().join("cut.txt").exists());
    let _first = server.requests.recv().await.unwrap();
    let retry_request: Value = serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let messages = retry_request["messages"].as_array().unwrap();
    let tool_index = messages.iter().position(|message| message["role"] == "tool")
        .expect("complete write call result must be sent to the provider");
    assert_eq!(messages[tool_index]["tool_call_id"], "write-complete");
    let tool_result: Value = serde_json::from_str(messages[tool_index]["content"].as_str().unwrap()).unwrap();
    assert!(tool_result["written"].as_str().unwrap().contains("complete.txt"));
    let note_index = tool_index + 1;
    assert_eq!(messages[note_index]["role"], "user");
    let note = messages[note_index]["content"].as_str().unwrap();
    assert!(note.starts_with("Your previous response was truncated at the output token limit"));
    assert!(note.contains("response truncated: the model stopped at its max output token limit"));
    assert!(note.contains("truncated tool call(s): write_file"));
    assert_eq!(messages.iter().filter(|message| message["role"] == "user"
        && message["content"].as_str().unwrap_or("").starts_with("Your previous response was truncated at the output token limit")).count(), 1);
    assert!(!retry_request.to_string().contains("write-cut"));

    let session = engine.session.lock().await;
    let recorded_tools: Vec<_> = session.messages.iter().filter(|message| message.role == "tool").collect();
    assert_eq!(recorded_tools.len(), 1);
    assert_eq!(recorded_tools[0].tool_call_id.as_deref(), Some("write-complete"));
    let notes: Vec<_> = session.messages.iter().filter(|message| message.role == "user"
        && message.content.starts_with("Your previous response was truncated at the output token limit")).collect();
    assert_eq!(notes.len(), 1);
}

#[tokio::test]
async fn engine_output_limit_drops_cut_delegate_and_names_it_in_retry_note() {
    let truncated = Reply::sse(
        vec![json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"write-before-delegate","function":{"name":"write_file","arguments":"{\"path\":\"parent.txt\",\"content\":\"saved\"}"}},
            {"index":1,"id":"delegate-cut","function":{"name":"delegate","arguments":"{\"agent\":\"child\",\"prompt\":\"unfinished"}}
        ]},"finish_reason":"length"}]})],
        true,
    );
    let mut server = server(vec![truncated, answer("finished")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.agents.insert(
        "writer".into(),
        serde_json::from_value(json!({"can_edit":true,"tools":["write_file","delegate"]})).unwrap(),
    );
    config.agents.insert("child".into(), serde_json::from_value(json!({"tools":["web_fetch"]})).unwrap());
    let (engine, _) = engine(config);
    assert_eq!(engine.turn(
        "write and delegate".into(),
        Selection { agent: Some("writer".into()), ..Selection::default() },
        CancellationToken::new(),
    ).await.unwrap(), "finished");
    assert_eq!(std::fs::read_to_string(tmp.path().join("parent.txt")).unwrap(), "saved");

    let _first = server.requests.recv().await.unwrap();
    let followup: Value = serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let messages = followup["messages"].as_array().unwrap();
    let note = messages.iter().find(|message| message["role"] == "user"
        && message["content"].as_str().unwrap_or("").starts_with("Your previous response was truncated at the output token limit"))
        .expect("retry note must follow successful work")["content"].as_str().unwrap();
    assert!(note.contains("truncated tool call(s): delegate"), "cut delegate should be named: {note}");
    assert!(!followup.to_string().contains("delegate-cut"));

    // The only provider follow-up is the parent's retry. No child request or
    // subagent lifecycle may be started for the cut delegate.
    assert_eq!(server.count.load(Ordering::SeqCst), 2);
    assert!(server.requests.try_recv().is_err());
    let session = engine.session.lock().await;
    assert!(session.display_events.iter().all(|event| match event {
        DisplayEvent::Activity(activity) => activity.kind != diet_soda::model::ActivityKind::Subagent,
        DisplayEvent::Message(_) => true,
    }));
    let tool_results: Vec<_> = session.messages.iter().filter(|message| message.role == "tool").collect();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(tool_results[0].tool_call_id.as_deref(), Some("write-before-delegate"));
}

#[tokio::test]
async fn consecutive_output_limited_model_turns_record_only_one_retry_note() {
    let truncated_turn = |id: &str, path: &str| Reply::sse(
        vec![json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":id,"function":{"name":"write_file","arguments":format!("{{\"path\":\"{path}\",\"content\":\"written\"}}")}},
            {"index":1,"id":format!("{id}-cut"),"function":{"name":"write_file","arguments":"{\"path\":\"cut.txt\",\"content\":\"partial"}}
        ]},"finish_reason":"length"}]})],
        true,
    );
    let mut server = server(vec![
        truncated_turn("first-write", "first.txt"),
        truncated_turn("second-write", "second.txt"),
        answer("finished"),
    ]).await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.agents.insert(
        "writer".into(),
        serde_json::from_value(json!({"can_edit":true,"tools":["write_file"]})).unwrap(),
    );
    let (engine, _) = engine(config);
    assert_eq!(engine.turn(
        "write twice".into(),
        Selection { agent: Some("writer".into()), ..Selection::default() },
        CancellationToken::new(),
    ).await.unwrap(), "finished");
    assert_eq!(std::fs::read_to_string(tmp.path().join("first.txt")).unwrap(), "written");
    assert_eq!(std::fs::read_to_string(tmp.path().join("second.txt")).unwrap(), "written");

    let _first = server.requests.recv().await.unwrap();
    let after_first: Value = serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let after_second: Value = serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let retry_prefix = "Your previous response was truncated at the output token limit";
    for request in [&after_first, &after_second] {
        assert_eq!(request["messages"].as_array().unwrap().iter().filter(|message|
            message["role"] == "user" && message["content"].as_str().unwrap_or("").starts_with(retry_prefix)).count(),
            1, "retry note must not duplicate across consecutive truncated model turns");
    }
    let session = engine.session.lock().await;
    let notes: Vec<_> = session.messages.iter().filter(|message| message.role == "user"
        && message.content.starts_with(retry_prefix)).collect();
    assert_eq!(notes.len(), 1, "one engine turn records at most one retry note");
    let tool_results: Vec<_> = session.messages.iter().filter(|message| message.role == "tool").collect();
    assert_eq!(tool_results.len(), 2);
    assert_eq!(tool_results[0].tool_call_id.as_deref(), Some("first-write"));
    assert_eq!(tool_results[1].tool_call_id.as_deref(), Some("second-write"));
}

#[tokio::test]
async fn provider_context_overflow_trims_and_retries_once() {
    // The first provider call is rejected as context overflow (HTTP 400 whose
    // body names the context limit). The engine must re-trim the request to a
    // tighter budget, record a `context_trim` event marked retry, and recover
    // on the second call instead of failing the turn. The history is seeded
    // with a large tool result (~211 KB of request bytes once JSON-escaped)
    // and the limits are pinned to a 131,072-token window with a 32,768-token
    // output cap: the history is over the 0.6x retry budget, under the full
    // budget, so the retry trim actually has something to collapse and the
    // retried request provably shrinks — an already-minimal request must not
    // be resent unchanged.
    let mut server = server(vec![
        tool_call("seq_numbers", json!({})),
        answer("seeded large history"),
        Reply {
            status: 400,
            content_type: "application/json".into(),
            body: json!({"error":{"message":"This model's maximum context length is 8192 tokens and your prompt has 20000 tokens"}}).to_string(),
            headers: vec![],
            header_delay: None,
            chunk_delay: None,
            stall: None,
        },
        answer("recovered after trim"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    // Pin the limits to reproduce the pre-rework budget: a 131,072-token
    // window with a 32,768-token output cap leaves ~295 KB of input budget,
    // while the 0.6x retry budget is ~177 KB — the seeded ~211 KB history
    // sits between them, so the retry trim has something to collapse.
    config.model.context_window = Some(131_072);
    config.model.max_tokens = Some(32_768);
    let tool: ToolConfig = serde_json::from_value(json!({"type":"command","command":"/usr/bin/seq","args":["1","27000"],"description":"count","hitl":false,"destructive":false,"input_schema":{"type":"object","properties":{}}})).unwrap();
    config.tools.insert("seq_numbers".into(), tool);
    let (engine, _events) = engine(config);
    assert_eq!(
        engine
            .turn(
                "first".into(),
                Selection::default(),
                CancellationToken::new(),
            )
            .await
            .expect("the seeding turn must complete"),
        "seeded large history"
    );
    let result = engine
        .turn(
            "start".into(),
            Selection::default(),
            CancellationToken::new(),
        )
        .await
        .expect("the single retry must recover the turn");
    assert_eq!(result, "recovered after trim");
    // Request 1: pre-tool turn 1. Request 2: post-tool turn 1 (large).
    let _pre_tool = server.requests.recv().await.unwrap();
    let _seeded = server.requests.recv().await.unwrap();
    let oversized = server.requests.recv().await.unwrap();
    let retried = server.requests.recv().await.unwrap();
    assert!(
        retried.body.len() < oversized.body.len(),
        "the retry must send a smaller request: {} >= {}",
        retried.body.len(),
        oversized.body.len()
    );
    let session_path = engine.session.lock().await.path.clone();
    let raw = std::fs::read_to_string(session_path).unwrap();
    assert!(
        raw.lines()
            .any(|line| line.contains("\"type\":\"context_trim\"")
                && line.contains("\"retry\":true")
                && line.contains("\"estimated_before\"")),
        "a context_trim event with retry=true and estimated_before must be recorded; session:\n{raw}"
    );
}
