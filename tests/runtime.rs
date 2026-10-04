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
    let outside = tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/target")).unwrap();
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
    let outside = tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/target")).unwrap();
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
    let outside = tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/target")).unwrap();
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
    assert!(!followup.body.contains("[output truncated]"));
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
    assert!(!followup.body.contains("[output truncated]"));
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
    let outside = tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/target")).unwrap();
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
    let outside = tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/target")).unwrap();
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
    let outside = tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/target")).unwrap();
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
        discovered: None,
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
                discovered: None,
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
async fn openai_context_window_caps_output_tokens() {
    let mut server = server(vec![answer("context-capped response")]).await;
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
        "context-capped response"
    );
    let request = server.requests.recv().await.unwrap();
    let body: Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["max_completion_tokens"], 50_000);
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
        discovered: None,
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
            .contains("Re-issue the affected tool call in smaller pieces"));
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
                    .contains("Re-issue the affected tool call in smaller pieces"),
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
                .contains("Re-issue the affected tool call in smaller pieces"),
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
        .contains("Re-issue the affected tool call in smaller pieces"));
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
    let outside = tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/target")).unwrap();
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
