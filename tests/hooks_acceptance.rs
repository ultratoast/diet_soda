mod support;

use diet_soda::{
    config::{Config, HookConfig},
    engine::Selection,
    hooks,
    workflow::{self, McpReference, Step, Workflow},
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, path::Path};
use support::*;
use tempfile::TempDir;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

fn python() -> &'static str {
    if cfg!(windows) {
        "python"
    } else {
        "python3"
    }
}

fn recorder(tmp: &TempDir, behavior: &str) -> HookConfig {
    let mut env = BTreeMap::new();
    env.insert(
        "HOOK_RECORDER".into(),
        tmp.path().join("events.jsonl").display().to_string(),
    );
    env.insert("HOOK_BEHAVIOR".into(), behavior.into());
    HookConfig {
        event: "session_start".into(),
        command: python().into(),
        args: vec![format!(
            "{}/tests/fixtures/hook_recorder.py",
            env!("CARGO_MANIFEST_DIR")
        )],
        env,
        enabled: true,
        timeout_seconds: 1,
        network_access: false,
    }
}

fn hook_for(tmp: &TempDir, event: &str) -> HookConfig {
    let mut hook = recorder(tmp, "ok");
    hook.event = event.into();
    hook
}

fn recorded(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn all_documented_events_use_ordered_versioned_envelopes() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config("http://127.0.0.1:1", tmp.path());
    let events = [
        "session_start",
        "before_model",
        "after_model",
        "before_tool",
        "after_tool",
        "workflow_step",
        "shutdown",
    ];
    config.hooks = events.iter().map(|event| hook_for(&tmp, event)).collect();
    let cancel = CancellationToken::new();

    for event in events {
        hooks::emit(&config, event, json!({"marker": event}), &cancel)
            .await
            .unwrap();
    }

    let entries = recorded(&tmp.path().join("events.jsonl"));
    assert_eq!(entries.len(), events.len());
    for (entry, expected) in entries.iter().zip(events) {
        assert_eq!(entry["version"], 1);
        assert_eq!(entry["event"], expected);
        assert_eq!(entry["payload"]["marker"], expected);
    }
}

#[tokio::test]
async fn provider_tool_and_workflow_emit_their_hook_events_in_execution_order() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("input.txt"), "local fixture").unwrap();
    let server = server(vec![
        tool_call("read_file", json!({"path":"input.txt"})),
        answer("done"),
        answer("workflow done"),
    ])
    .await;
    let mut config = config(&server.url, tmp.path());
    config.hooks = [
        "before_model",
        "after_model",
        "before_tool",
        "after_tool",
        "workflow_step",
    ]
    .iter()
    .map(|event| hook_for(&tmp, event))
    .collect();
    let (engine, _events) = engine(config.clone());
    engine
        .turn(
            "inspect the fixture".into(),
            Selection::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let workflow = Workflow {
        title: "Acceptance workflow".into(),
        author: "test".into(),
        steps: vec![Step {
            agent: None,
            model: "openrouter:openrouter/test".into(),
            prompt: "Return the fixture result".into(),
            mcps: Vec::<McpReference>::new(),
            hitl: false,
        }],
    };
    workflow::run(
        &engine,
        workflow,
        "fixture".into(),
        Selection::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let names: Vec<String> = recorded(&tmp.path().join("events.jsonl"))
        .into_iter()
        .map(|entry| entry["event"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        names,
        [
            "before_model",
            "after_model",
            "before_tool",
            "after_tool",
            "before_model",
            "after_model",
            "before_model",
            "after_model",
            "workflow_step",
        ]
    );
}

#[tokio::test]
async fn hook_output_cap_timeout_nonzero_and_deny_are_failures() {
    for (behavior, expected) in [
        ("large", "64"),
        ("timeout", "timed out"),
        ("nonzero", "exit"),
        ("deny", "denied"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = config("http://127.0.0.1:1", tmp.path());
        let mut hook = recorder(&tmp, behavior);
        hook.event = "before_model".into();
        config.hooks.push(hook);
        let error = hooks::emit(
            &config,
            "before_model",
            json!({}),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err()
        .to_string();
        if behavior == "large" {
            assert!(
                error.contains("invalid JSON"),
                "output must be capped before parsing: {error}"
            );
        } else if behavior == "timeout" {
            assert!(
                error.contains("Plugin hook before_model"),
                "timeout: {error}"
            );
        } else {
            assert!(
                error.to_ascii_lowercase().contains(expected),
                "{behavior}: {error}"
            );
        }
    }
}

#[tokio::test]
async fn headless_normal_completion_runs_session_start_and_shutdown_hooks() {
    let tmp = tempfile::tempdir().unwrap();
    let server = server(vec![answer("hello")]).await;
    let config = headless_config(&tmp, &server.url, "ok");
    let path = tmp.path().join("config.json");
    fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_diet_soda"))
        .args(["--config", path.to_str().unwrap(), "--prompt", "hello"])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let names: Vec<String> = recorded(&tmp.path().join("events.jsonl"))
        .into_iter()
        .map(|entry| entry["event"].as_str().unwrap().into())
        .collect();
    assert_eq!(names, ["session_start", "shutdown"]);
}

#[cfg(unix)]
#[tokio::test]
async fn headless_cancellation_still_runs_session_start_and_shutdown_hooks() {
    let tmp = tempfile::tempdir().unwrap();
    let mut reply = answer("never completes");
    reply.stall = Some(std::time::Duration::from_secs(30));
    let server = server(vec![reply]).await;
    let config = headless_config(&tmp, &server.url, "ok");
    let path = tmp.path().join("config.json");
    fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_diet_soda"))
        .args(["--config", path.to_str().unwrap(), "--prompt", "cancel me"])
        .spawn()
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child.id().unwrap() as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    let output = child.wait_with_output().await.unwrap();
    let _ = output;
    let names: Vec<String> = recorded(&tmp.path().join("events.jsonl"))
        .into_iter()
        .map(|entry| entry["event"].as_str().unwrap().into())
        .collect();
    assert_eq!(names, ["session_start", "shutdown"]);
}

fn headless_config(tmp: &TempDir, provider_url: &str, behavior: &str) -> Config {
    let mut config = config(provider_url, tmp.path());
    let mut start = recorder(tmp, behavior);
    start.event = "session_start".into();
    let mut shutdown = start.clone();
    shutdown.event = "shutdown".into();
    config.hooks = vec![start, shutdown];
    config
}
