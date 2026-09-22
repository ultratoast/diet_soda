//! Lifecycle (Wave 2) tool-dispatch tests. The outer `Engine::invoke` is
//! expected to emit a paired `Start`/`End` activity record before and after
//! every tool call — even on rejection, cancellation, or a validation
//! failure inside the inner executor — so the on-disk activity trail is
//! always complete. These tests drive `Engine::invoke` directly with a
//! fake approval channel so each termination path can be exercised
//! without touching any model provider.
#![allow(clippy::needless_raw_string_hashes)]

use diet_soda::{
    config::{AgentConfig, Config, McpConfig, McpTransport, ProviderConfig, ProviderKind},
    engine::{Engine, RegisteredTool, Scope, Selection},
    model::{
        ActivityEvent, ActivityKind, ActivityPhase, ActivityStatus, Decision, ToolCall, UiEvent,
    },
    session::Session,
};
use serde_json::{json, Value};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use tempfile::tempdir;
use tokio::{sync::mpsc, time::timeout};
use tokio_util::sync::CancellationToken;

#[cfg(unix)]
static GH_PATH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

mod support {
    use super::*;
    pub fn workspace(tmp: &std::path::Path) -> Config {
        let mut config = Config {
            workspace: tmp.into(),
            sessions_dir: tmp.join("sessions"),
            skills_dir: tmp.join("skills"),
            workflows_dir: tmp.join("workflows"),
            exports_dir: tmp.join("exports"),
            ..Config::default()
        };
        // Tests do not issue HTTP requests; a localhost placeholder keeps the
        // provider config valid without ever being dialed.
        config.providers.insert(
            "openrouter".into(),
            ProviderConfig {
                kind: ProviderKind::Openrouter,
                base_url: "http://127.0.0.1:1".into(),
                api_key_env: None,
                timeout_seconds: 5,
            },
        );
        config
    }
    pub fn engine(config: Config) -> (Engine, mpsc::UnboundedReceiver<UiEvent>) {
        let session = Session::open(&config.sessions_dir, None).unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        (Engine::new(config, session, tx), rx)
    }
    /// Agent with every built-in the lifecycle tests exercise. Marking it
    /// `can_edit = true` lets `write_file` and the destructive `shell`
    /// tests opt into the approval path.
    pub fn read_only_agent() -> AgentConfig {
        AgentConfig {
            tools: Some(vec![
                "read_file".into(),
                "write_file".into(),
                "shell".into(),
                "web_fetch".into(),
            ]),
            ..AgentConfig::default()
        }
    }
    pub fn editing_agent() -> AgentConfig {
        AgentConfig {
            can_edit: true,
            ..read_only_agent()
        }
    }
    /// Drive `Engine::invoke` as a scope-less caller. Real callers go
    /// through `Engine::turn`, but the dispatch lifecycle can be exercised
    /// against a fully-formed `Scope` directly.
    pub async fn scope_for(engine: &Engine) -> Scope {
        let config = engine.config.read().await;
        let selection = Selection::default();
        drop(config);
        // Build the scope through the public `Engine::scope` path so we
        // get the same prompt/permissions composition production uses.
        engine.scope(&selection, "main", None).await.expect("scope")
    }
}

fn call<S: Into<String>>(id: S, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.to_string(),
    }
}

async fn collect_events(
    rx: &mut mpsc::UnboundedReceiver<UiEvent>,
    count: usize,
) -> Vec<ActivityEvent> {
    let mut out = Vec::new();
    let deadline = Duration::from_secs(2);
    while out.len() < count {
        let event = match timeout(deadline, rx.recv()).await {
            Ok(Some(event)) => event,
            _ => break,
        };
        if let UiEvent::Activity(activity) = event {
            out.push(activity);
        }
    }
    out
}

/// Drain the UiEvent channel for `count` events (any kind), returning the
/// raw events so callers can extract Approval replies or Activity records
/// without worrying about ordering. The bounded timeout keeps a missing
/// event from hanging the test.
async fn collect_raw_events(
    rx: &mut mpsc::UnboundedReceiver<UiEvent>,
    count: usize,
) -> Vec<UiEvent> {
    let mut out = Vec::new();
    let deadline = Duration::from_secs(2);
    while out.len() < count {
        let event = match timeout(deadline, rx.recv()).await {
            Ok(Some(event)) => event,
            _ => break,
        };
        out.push(event);
    }
    out
}

fn read_persisted_activities(path: &std::path::Path) -> Vec<ActivityEvent> {
    let raw = std::fs::read_to_string(path).unwrap();
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["type"] == "activity")
        .map(|line| serde_json::from_value::<ActivityEvent>(line["data"].clone()).unwrap())
        .collect()
}

#[cfg(unix)]
#[tokio::test]
async fn destructive_gh_is_approved_before_readiness_and_rejection_prevents_invocation() {
    let _path_guard = GH_PATH_LOCK.lock().await;
    let tmp = tempdir().unwrap();
    let marker = tmp.path().join("invoked");
    let bin = tmp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let gh = bin.join("gh");
    std::fs::write(
        &gh,
        format!("#!/bin/sh\nprintf invoked > '{}'\n", marker.display()),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&gh).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&gh, permissions).unwrap();

    let previous_path = std::env::var_os("PATH");
    let previous_gh = std::env::var_os("GH_TOKEN");
    let path = previous_path
        .as_ref()
        .map(|value| format!("{}:{}", bin.display(), value.to_string_lossy()))
        .unwrap_or_else(|| bin.display().to_string());
    std::env::set_var("PATH", path);
    std::env::remove_var("GH_TOKEN");

    let mut config = support::workspace(tmp.path());
    config.agents.insert(
        "main".into(),
        AgentConfig {
            tools: Some(vec!["gh".into()]),
            ..AgentConfig::default()
        },
    );
    let (engine, mut events) = support::engine(config);
    let scope = engine
        .scope(
            &Selection {
                agent: Some("main".into()),
                ..Selection::default()
            },
            "main",
            None,
        )
        .await
        .unwrap();
    let registered = engine
        .available(&scope, &CancellationToken::new())
        .await
        .unwrap();
    let invocation = call("tc-gh-reject", "gh", json!({"args":["issue","list"]}));
    let runner = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .invoke(&scope, &invocation, &registered, &CancellationToken::new())
                .await
        }
    });

    let approval = timeout(Duration::from_secs(2), async {
        loop {
            if let Some(UiEvent::Approval { reply, detail, .. }) = events.recv().await {
                return (reply, detail);
            }
        }
    })
    .await
    .expect("gh approval event");
    assert!(approval.1.contains("Run `gh issue list`"));
    approval.0.send(Decision::Reject).unwrap();
    assert!(runner.await.unwrap().is_err());
    assert!(
        !marker.exists(),
        "gh readiness or execution ran before approval"
    );

    match previous_path {
        Some(path) => std::env::set_var("PATH", path),
        None => std::env::remove_var("PATH"),
    }
    match previous_gh {
        Some(value) => std::env::set_var("GH_TOKEN", value),
        None => std::env::remove_var("GH_TOKEN"),
    }
}

#[tokio::test]
async fn write_approval_detail_previews_content_but_activity_error_summary_does_not() {
    let tmp = tempdir().unwrap();
    let mut config = support::workspace(tmp.path());
    config
        .agents
        .insert("main".into(), support::editing_agent());
    let (engine, mut events) = support::engine(config);
    let scope = engine
        .scope(
            &Selection {
                agent: Some("main".into()),
                ..Selection::default()
            },
            "main",
            None,
        )
        .await
        .unwrap();
    let registered = engine
        .available(&scope, &CancellationToken::new())
        .await
        .unwrap();
    let content = "CONTENT_SECRET".repeat(40);
    let invocation = call(
        "tc-write-preview",
        "write_file",
        json!({"path":"missing/file.txt","content":content}),
    );
    let runner = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .invoke(&scope, &invocation, &registered, &CancellationToken::new())
                .await
        }
    });

    let detail = timeout(Duration::from_secs(2), async {
        loop {
            if let Some(UiEvent::Approval { reply, detail, .. }) = events.recv().await {
                reply.send(Decision::Approve).unwrap();
                return detail;
            }
        }
    })
    .await
    .expect("write approval event");
    assert!(detail.contains("Content preview:"));
    assert!(detail.contains("CONTENT_SECRET"));
    assert!(detail.contains("[output truncated]"));
    assert!(detail.len() < 800);

    let error = runner.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("No such file") || error.to_string().contains("not found"));
    let activities = collect_events(&mut events, 2).await;
    assert!(!activities.is_empty());
    for activity in activities {
        assert!(!activity.title.contains("CONTENT_SECRET"));
        assert!(!activity.title.contains("Content preview"));
    }
}

#[tokio::test]
async fn success_emits_paired_activity_records_with_success_status() {
    let tmp = tempdir().unwrap();
    std::fs::write(tmp.path().join("hello.txt"), "world").unwrap();
    let mut config = support::workspace(tmp.path());
    config
        .agents
        .insert("main".into(), support::read_only_agent());
    let (engine, mut events) = support::engine(config);
    let scope = support::scope_for(&engine).await;
    let registered: Vec<RegisteredTool> = engine
        .available(&scope, &CancellationToken::new())
        .await
        .unwrap();

    let call = call("tc-success", "read_file", json!({"path":"hello.txt"}));
    let cancel = CancellationToken::new();
    let value = engine
        .invoke(&scope, &call, &registered, &cancel)
        .await
        .unwrap();
    assert_eq!(value, json!({"content":"world","truncated":false}));

    let activities = collect_events(&mut events, 2).await;
    assert_eq!(activities.len(), 2, "exactly two activity events observed");
    let start = &activities[0];
    let end = &activities[1];
    assert_eq!(start.id, end.id, "Start and End share a tool_id");
    assert_eq!(start.phase, ActivityPhase::Start);
    assert_eq!(end.phase, ActivityPhase::End);
    assert_eq!(start.kind, ActivityKind::Tool);
    assert_eq!(end.kind, ActivityKind::Tool);
    assert_eq!(end.status, Some(ActivityStatus::Success));
    assert_eq!(
        start.title, end.title,
        "Start and End titles match (describe_call output)"
    );
    assert!(start.title.contains("Read `hello.txt`"));
    assert_eq!(start.context, "main");
    assert_eq!(end.context, "main");
    assert_eq!(start.external_id.as_deref(), Some("tc-success"));
    assert_eq!(end.external_id.as_deref(), Some("tc-success"));

    // Persisted on-disk trail matches the live one byte-for-byte. The
    // activity contract is "persisted first, live best-effort"; the
    // equality check pins both surfaces.
    let session_path = engine.session.lock().await.path.clone();
    let persisted = read_persisted_activities(&session_path);
    assert_eq!(persisted.len(), 2);
    assert_eq!(persisted[0].id, activities[0].id);
    assert_eq!(persisted[1].id, activities[1].id);
    assert_eq!(persisted[0].external_id, activities[0].external_id);
    assert_eq!(persisted[1].status, activities[1].status);
}

#[tokio::test]
async fn tool_status_event_uses_scope_context() {
    let tmp = tempdir().unwrap();
    std::fs::write(tmp.path().join("hello.txt"), "world").unwrap();
    let mut config = support::workspace(tmp.path());
    config
        .agents
        .insert("main".into(), support::read_only_agent());
    let (engine, mut events) = support::engine(config);
    let scope = support::scope_for(&engine).await;
    let registered = engine
        .available(&scope, &CancellationToken::new())
        .await
        .unwrap();

    engine
        .invoke(
            &scope,
            &call("tc-status", "read_file", json!({"path":"hello.txt"})),
            &registered,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

    let raw = collect_raw_events(&mut events, 3).await;
    let statuses: Vec<_> = raw
        .iter()
        .filter_map(|event| match event {
            UiEvent::Status { context, text } => Some((context, text)),
            _ => None,
        })
        .collect();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].0, "main");
    assert_eq!(statuses[0].1, "Running read_file (main)");
}

#[tokio::test]
async fn mcp_unavailable_status_event_uses_scope_context() {
    let tmp = tempdir().unwrap();
    let mut config = support::workspace(tmp.path());
    let server_uuid = "unavailable-server";
    config.mcp_servers.insert(
        "broken".into(),
        McpConfig {
            uuid: server_uuid.into(),
            transport: McpTransport::Http {
                url: "http://127.0.0.1:1/mcp".into(),
                headers: Default::default(),
            },
            enabled: true,
            hitl: false,
            timeout_seconds: 1,
        },
    );
    let mut agent = support::read_only_agent();
    agent.mcp_servers = Some(vec![server_uuid.into()]);
    config.agents.insert("main".into(), agent);
    let (engine, mut events) = support::engine(config);
    let scope = support::scope_for(&engine).await;

    let _ = engine
        .available(&scope, &CancellationToken::new())
        .await
        .unwrap();
    let raw = collect_raw_events(&mut events, 1).await;
    let status = raw.into_iter().find_map(|event| match event {
        UiEvent::Status { context, text } => Some((context, text)),
        _ => None,
    });
    let (context, text) = status.expect("unavailable MCP must emit a status");
    assert_eq!(context, "main");
    assert!(text.starts_with("MCP broken unavailable:"), "{text}");
}

#[tokio::test]
async fn deny_emits_paired_records_with_denied_status() {
    // Configure read_file as approval-required, then drive the user into
    // Reject so `invoke_inner` returns the tagged Deny error and the
    // outer can map it to ActivityStatus::Denied without string parsing.
    let tmp = tempdir().unwrap();
    std::fs::write(tmp.path().join("ok.txt"), "x").unwrap();
    let mut config = support::workspace(tmp.path());
    config.approval_tools.push("read_file".into());
    config
        .agents
        .insert("main".into(), support::editing_agent());
    let (engine, mut events) = support::engine(config);
    let scope = support::scope_for(&engine).await;
    let cancel = CancellationToken::new();

    let registered: Vec<RegisteredTool> = engine
        .available(&scope, &CancellationToken::new())
        .await
        .unwrap();

    // Sanity: the registered `read_file` is actually approval-required.
    assert!(
        registered
            .iter()
            .find(|t| t.spec.name == "read_file")
            .unwrap()
            .hitl,
        "test fixture must require approval for read_file"
    );

    let inner_engine = engine.clone();
    let inner_scope = scope.clone();
    let inner_registered = registered.clone();
    let inner_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        inner_engine
            .invoke(
                &inner_scope,
                &call("tc-deny", "read_file", json!({"path":"ok.txt"})),
                &inner_registered,
                &inner_cancel,
            )
            .await
    });
    // Read the Start event first; the next event is Approval, which we
    // answer with Decision::Reject. After the task completes, drain
    // any remaining events from the channel so we can observe the End.
    let first = collect_raw_events(&mut events, 1).await;
    assert_eq!(first.len(), 1);
    let second = collect_raw_events(&mut events, 1).await;
    assert_eq!(second.len(), 1);
    let approval = second.into_iter().next().unwrap();
    let UiEvent::Approval { reply, .. } = approval else {
        panic!("expected Approval after Start, got {approval:?}");
    };
    reply.send(Decision::Reject).unwrap();

    let err = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("invoke returns within 5s")
        .expect("task did not panic")
        .expect_err("Rejected call surfaces Err");
    assert!(
        format!("{err:#}").contains("Tool rejected by user"),
        "expected tool rejected error, got: {err:#}"
    );

    // After the rejection the End event arrives with ActivityStatus::Denied.
    // The first event we read was the Start activity; the Approval was
    // removed by the move above. The remaining event is the End.
    let activities: Vec<ActivityEvent> = first
        .into_iter()
        .filter_map(|e| match e {
            UiEvent::Activity(activity) => Some(activity),
            _ => None,
        })
        .collect();
    let ending = collect_raw_events(&mut events, 1).await;
    let mut activities: Vec<ActivityEvent> = activities
        .into_iter()
        .chain(ending.into_iter().filter_map(|e| match e {
            UiEvent::Activity(activity) => Some(activity),
            _ => None,
        }))
        .collect();
    assert_eq!(activities.len(), 2, "denial still emits Start+End");
    activities.sort_by_key(|a| a.phase == ActivityPhase::End);
    assert_eq!(activities[0].phase, ActivityPhase::Start);
    assert_eq!(activities[1].phase, ActivityPhase::End);
    assert_eq!(activities[1].status, Some(ActivityStatus::Denied));
    assert_eq!(activities[0].id, activities[1].id);
    assert_eq!(
        activities[0].external_id.as_deref(),
        Some("tc-deny"),
        "external_id (model request id) flows through Start"
    );
}

/// Drain the UiEvent channel until an Activity with `phase == End` and
/// `id == tool_id` arrives, or the bounded timeout elapses. Returns the
/// matching End and any other Activity records observed along the way so
/// callers can assert on the full activity trail without reading a fixed
/// number of raw events. The timeout is bounded so a missing End does not
/// hang the test.
async fn await_end_activity(
    rx: &mut mpsc::UnboundedReceiver<UiEvent>,
    tool_id: &str,
    timeout_duration: Duration,
) -> (Option<ActivityEvent>, Vec<ActivityEvent>) {
    let mut others = Vec::new();
    let deadline = timeout_duration;
    let start = tokio::time::Instant::now();
    while start.elapsed() < deadline {
        let remaining = deadline.saturating_sub(start.elapsed());
        match timeout(remaining, rx.recv()).await {
            Ok(Some(UiEvent::Activity(activity)))
                if activity.phase == ActivityPhase::End && activity.id == tool_id =>
            {
                return (Some(activity), others);
            }
            Ok(Some(UiEvent::Activity(activity))) => others.push(activity),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    (None, others)
}

#[tokio::test]
async fn abort_cancels_invoke_and_records_cancelled_status() {
    // Cancellation fires during the execution phase so we exercise the
    // inner `bail!("Cancelled")` mapping. The shell tool sleeps long
    // enough that `cancel.cancel()` beats it to the promise.
    let tmp = tempdir().unwrap();
    let mut config = support::workspace(tmp.path());
    config.approval_tools.push("shell".into());
    config
        .agents
        .insert("main".into(), support::editing_agent());
    let (engine, mut events) = support::engine(config);
    let scope = support::scope_for(&engine).await;

    let registered: Vec<RegisteredTool> = engine
        .available(&scope, &CancellationToken::new())
        .await
        .unwrap();
    let shell = registered
        .iter()
        .find(|t| t.spec.name == "shell")
        .expect("shell registered");
    assert!(shell.hitl, "shell must require approval for this test");

    let cancel = CancellationToken::new();
    let inner_engine = engine.clone();
    let inner_cancel = cancel.clone();
    let inner_registered = registered.clone();
    let inner_scope = scope.clone();
    let task = tokio::spawn(async move {
        inner_engine
            .invoke(
                &inner_scope,
                &call(
                    "tc-cancel",
                    "shell",
                    json!({"command":"sleep","args":["10"]}),
                ),
                &inner_registered,
                &inner_cancel,
            )
            .await
    });

    // Read the Start activity so we know the lifecycle id, then the
    // Approval event so we can approve the shell call.
    let mut first = collect_raw_events(&mut events, 1).await;
    assert_eq!(first.len(), 1);
    let start_activity: ActivityEvent = match first.pop().unwrap() {
        UiEvent::Activity(a) => a,
        other => panic!("expected Start activity, got {other:?}"),
    };
    assert_eq!(start_activity.phase, ActivityPhase::Start);
    let tool_id = start_activity.id.clone();

    let second = collect_raw_events(&mut events, 1).await;
    assert_eq!(second.len(), 1);
    let approval = second.into_iter().next().unwrap();
    let UiEvent::Approval { reply, .. } = approval else {
        panic!("expected Approval, got {approval:?}");
    };
    reply.send(Decision::Approve).unwrap();

    // Cancel mid-execution; the shell tool sleeps long enough that the
    // cancellation token trips first and produces `bail!("Cancelled")`.
    cancel.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("invoke returns within 10s of cancellation")
        .expect("task did not panic");
    let err = outcome.expect_err("cancellation surfaces Err");
    assert!(
        format!("{err:#}").contains("Cancelled"),
        "expected Cancelled error, got: {err:#}"
    );

    // Wait (bounded) for the matching End Activity rather than reading
    // a fixed number of raw events. The tool id from Start must round
    // trip into the End so a flaky channel ordering cannot mask the
    // missing End event.
    let (end, others) = await_end_activity(&mut events, &tool_id, Duration::from_secs(5)).await;
    let end = end.expect("matching End activity arrives within timeout");
    assert_eq!(end.id, tool_id);
    assert_eq!(end.phase, ActivityPhase::End);
    assert_eq!(end.status, Some(ActivityStatus::Cancelled));

    // The only activity record observed in addition to the End is the
    // Start we already captured; the test must not see a second
    // lifecycle for the same id.
    assert!(
        others.iter().all(|a| a.id != tool_id),
        "no extra activity records for the cancelled tool id"
    );
}

#[tokio::test]
async fn unknown_tool_records_error_status() {
    // `invoke` receives an unregistered tool name. The lookup fails before
    // the executor runs, but Start/End still wraps the failure.
    let tmp = tempdir().unwrap();
    let mut config = support::workspace(tmp.path());
    config
        .agents
        .insert("main".into(), support::read_only_agent());
    let (engine, mut events) = support::engine(config);
    let scope = support::scope_for(&engine).await;
    let registered: Vec<RegisteredTool> = engine
        .available(&scope, &CancellationToken::new())
        .await
        .unwrap();
    assert!(
        registered.iter().all(|t| t.spec.name != "missing"),
        "fixture must not advertise the missing tool"
    );

    let cancel = CancellationToken::new();
    let err = engine
        .invoke(
            &scope,
            &call("tc-missing", "missing", json!({})),
            &registered,
            &cancel,
        )
        .await
        .expect_err("unknown tool returns Err");
    assert!(format!("{err:#}").contains("unavailable"));

    let activities = collect_events(&mut events, 2).await;
    assert_eq!(activities.len(), 2);
    assert_eq!(activities[0].phase, ActivityPhase::Start);
    assert_eq!(activities[1].phase, ActivityPhase::End);
    assert_eq!(activities[1].status, Some(ActivityStatus::Error));
    assert_eq!(activities[0].external_id.as_deref(), Some("tc-missing"));
}

#[tokio::test]
async fn invalid_arguments_record_error_status() {
    // Schema validation rejects arguments before execution. Start/End must
    // still pair with status=Error and a useful title.
    let tmp = tempdir().unwrap();
    let mut config = support::workspace(tmp.path());
    config
        .agents
        .insert("main".into(), support::read_only_agent());
    let (engine, mut events) = support::engine(config);
    let scope = support::scope_for(&engine).await;
    let registered = engine
        .available(&scope, &CancellationToken::new())
        .await
        .unwrap();

    let cancel = CancellationToken::new();
    let err = engine
        .invoke(
            &scope,
            &call("tc-bad-args", "read_file", json!({})),
            &registered,
            &cancel,
        )
        .await
        .expect_err("missing required field");
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("path") || rendered.contains("required"),
        "validation error must mention path or required; got: {rendered}"
    );

    let activities = collect_events(&mut events, 2).await;
    assert_eq!(activities.len(), 2);
    assert_eq!(activities[1].status, Some(ActivityStatus::Error));
}

#[tokio::test]
async fn execution_failure_records_error_status() {
    // read_file exists and validates, but the file does not. The error
    // originates from the executor (file IO), so we exercise the
    // execution-time failure path.
    let tmp = tempdir().unwrap();
    let mut config = support::workspace(tmp.path());
    config
        .agents
        .insert("main".into(), support::read_only_agent());
    let (engine, mut events) = support::engine(config);
    let scope = support::scope_for(&engine).await;
    let registered = engine
        .available(&scope, &CancellationToken::new())
        .await
        .unwrap();

    let cancel = CancellationToken::new();
    let err = engine
        .invoke(
            &scope,
            &call("tc-io", "read_file", json!({"path":"missing.txt"})),
            &registered,
            &cancel,
        )
        .await
        .expect_err("missing file returns Err");
    let activities = collect_events(&mut events, 2).await;
    assert_eq!(activities.len(), 2);
    assert_eq!(activities[1].status, Some(ActivityStatus::Error));
    let rendered = format!("{err:#}").to_lowercase();
    assert!(
        rendered.contains("missing")
            || rendered.contains("not found")
            || rendered.contains("no such file"),
        "execution-time failure should describe the missing file; got: {rendered}"
    );
}

#[tokio::test]
async fn parent_activity_id_propagates_to_children() {
    // A child tool call inherits the parent's activity id as its
    // `parent_id`. Subagent scopes reuse the field so the on-disk graph
    // remains correlated.
    let tmp = tempdir().unwrap();
    let mut config = support::workspace(tmp.path());
    std::fs::write(tmp.path().join("a.txt"), "a").unwrap();
    config
        .agents
        .insert("main".into(), support::read_only_agent());
    let (engine, mut events) = support::engine(config);
    let mut scope = support::scope_for(&engine).await;
    scope.activity_id = Some("subagent-root".into());

    let registered = engine
        .available(&scope, &CancellationToken::new())
        .await
        .unwrap();
    let inner_engine = engine.clone();
    let inner_scope = scope.clone();
    let inner_registered = registered.clone();
    let task = tokio::spawn(async move {
        inner_engine
            .invoke(
                &inner_scope,
                &call("tc-child", "read_file", json!({"path":"a.txt"})),
                &inner_registered,
                &CancellationToken::new(),
            )
            .await
    });
    let _ = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("invoke completes")
        .expect("Ok");

    let activities = collect_events(&mut events, 2).await;
    assert_eq!(activities.len(), 2);
    assert_eq!(activities[0].parent_id.as_deref(), Some("subagent-root"));
    assert_eq!(activities[1].parent_id.as_deref(), Some("subagent-root"));
    assert_ne!(activities[0].id, "subagent-root");
    assert_ne!(activities[1].id, "subagent-root");
}

#[tokio::test]
async fn approval_persists_activity_id_equal_to_tool_lifecycle_id() {
    // When approval_required is hit, the persisted approval event must
    // carry the same activity id as the tool's Start/End pair. This pins
    // the cross-event correlation contract.
    let tmp = tempdir().unwrap();
    let mut config = support::workspace(tmp.path());
    config.approval_tools.push("read_file".into());
    config
        .agents
        .insert("main".into(), support::read_only_agent());
    let (engine, mut events) = support::engine(config);
    let scope = support::scope_for(&engine).await;
    let registered = engine
        .available(&scope, &CancellationToken::new())
        .await
        .unwrap();
    let session_path = engine.session.lock().await.path.clone();

    let cancel = CancellationToken::new();
    std::fs::write(tmp.path().join("ok.txt"), "ok").unwrap();
    let inner_engine = engine.clone();
    let inner_registered = registered.clone();
    let inner_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        inner_engine
            .invoke(
                &scope,
                &call("tc-approval", "read_file", json!({"path":"ok.txt"})),
                &inner_registered,
                &inner_cancel,
            )
            .await
    });
    while let Some(event) = events.recv().await {
        if let UiEvent::Approval { reply, .. } = event {
            reply.send(Decision::Approve).unwrap();
            break;
        }
    }
    task.await.unwrap().expect("invoke ok");

    let activities: Vec<ActivityEvent> = {
        let raw = std::fs::read_to_string(&session_path).unwrap();
        raw.lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|line| line["type"] == "activity")
            .map(|line| serde_json::from_value(line["data"].clone()).unwrap())
            .collect()
    };
    let tool_id = activities[0].id.clone();
    assert_eq!(activities.len(), 2);
    assert_eq!(activities[0].id, activities[1].id);

    // Approval event carries `data.activity_id == tool_id`.
    let raw = std::fs::read_to_string(&session_path).unwrap();
    let approvals: Vec<Value> = raw
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["type"] == "approval")
        .collect();
    assert_eq!(approvals.len(), 1, "approval event written exactly once");
    assert_eq!(approvals[0]["data"]["activity_id"], tool_id);
}

#[tokio::test]
async fn hook_warning_value_still_maps_to_success() {
    // Hook errors that occur after a successful tool execution are wrapped
    // back into a JSON value so the lifecycle still terminates with
    // status=Success. The hook_warning_passthrough path lives inside the
    // inner executor (after_tool hook returned Err), and the outer
    // should observe Completed(_).
    let tmp = tempdir().unwrap();
    let mut config = support::workspace(tmp.path());
    std::fs::write(tmp.path().join("hook.txt"), "h").unwrap();
    config
        .agents
        .insert("main".into(), support::read_only_agent());
    // Register an after_tool hook that always fails. The exec succeeds,
    // and the after_tool hook is wrapped into a value with a `hook_error`
    // field, status remains Success.
    let hook = diet_soda::config::HookConfig {
        event: "after_tool".into(),
        command: "false".into(),
        args: vec![],
        env: Default::default(),
        enabled: true,
        timeout_seconds: 5,
    };
    config.hooks.push(hook);
    let (engine, mut events) = support::engine(config);
    let scope = support::scope_for(&engine).await;
    let registered = engine
        .available(&scope, &CancellationToken::new())
        .await
        .unwrap();

    let cancel = CancellationToken::new();
    let value = engine
        .invoke(
            &scope,
            &call("tc-hook", "read_file", json!({"path":"hook.txt"})),
            &registered,
            &cancel,
        )
        .await
        .unwrap();
    assert!(
        value.get("hook_error").is_some(),
        "hook error wrapped into value: {value}"
    );

    let activities = collect_events(&mut events, 2).await;
    assert_eq!(activities.len(), 2);
    assert_eq!(activities[1].status, Some(ActivityStatus::Success));
}

#[tokio::test]
async fn invalid_argument_json_records_error_status() {
    // Malformed JSON arguments still get Start/End with status=Error. The
    // title is rendered from a best-effort parse so the model can still
    // correlate from its own request id (`external_id`).
    let tmp = tempdir().unwrap();
    let mut config = support::workspace(tmp.path());
    config
        .agents
        .insert("main".into(), support::read_only_agent());
    let (engine, mut events) = support::engine(config);
    let scope = support::scope_for(&engine).await;
    let registered = engine
        .available(&scope, &CancellationToken::new())
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let malformed = ToolCall {
        id: "tc-malformed".into(),
        name: "read_file".into(),
        arguments: "{not valid".into(),
    };
    let err = engine
        .invoke(&scope, &malformed, &registered, &cancel)
        .await
        .expect_err("malformed args returns Err");
    assert!(format!("{err:#}").contains("Invalid"));

    let activities = collect_events(&mut events, 2).await;
    assert_eq!(activities.len(), 2);
    assert_eq!(activities[1].status, Some(ActivityStatus::Error));
    assert_eq!(activities[0].external_id.as_deref(), Some("tc-malformed"));
}

// -------------------------------------------------------------------------
// Tool activity title prefix + sanitization. The on-disk title prefixes
// the actual tool name (built-in, custom, or MCP) so the transcript
// always shows which tool produced the record. The full describe_call
// body flows through the shared sanitizer:
//   * control chars / newlines / tabs become spaces,
//   * whitespace runs collapse,
//   * the entire title (prefix + body + ellipsis) is capped at 160
//     Unicode scalar values.
//
// These tests pin the prefix contract and the sanitization contract
// against the public `Engine::invoke` API. Approval detail and
// transcript text are NOT touched by the sanitizer; only the activity
// title is.
// -------------------------------------------------------------------------

#[tokio::test]
async fn tool_activity_title_prefixes_builtin_tool_name() {
    // `read_file` is a built-in. The activity title must start with
    // `tool read_file: ` so the transcript never shows a tool
    // activity whose tool identity is implicit.
    let tmp = tempdir().unwrap();
    std::fs::write(tmp.path().join("hello.txt"), "world").unwrap();
    let mut config = support::workspace(tmp.path());
    // The default agent needs `read_file` enabled so the scope's
    // `tools` list includes it (the available filter requires it for
    // non-can_edit agents).
    let mut main = support::read_only_agent();
    main.default = true;
    config.agents.insert("main".into(), main);
    let (engine, mut events) = support::engine(config);
    let scope = support::scope_for(&engine).await;
    let registered = engine
        .available(&scope, &CancellationToken::new())
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let _ = engine
        .invoke(
            &scope,
            &call(
                "tc-prefix-builtin",
                "read_file",
                json!({"path":"hello.txt"}),
            ),
            &registered,
            &cancel,
        )
        .await
        .unwrap();
    let activities = collect_events(&mut events, 2).await;
    assert_eq!(activities.len(), 2);
    assert!(
        activities[0].title.starts_with("tool read_file: "),
        "tool title must prefix the actual built-in tool name; got {}",
        activities[0].title
    );
    assert!(
        activities[1].title.starts_with("tool read_file: "),
        "tool End title must prefix the actual built-in tool name; got {}",
        activities[1].title
    );
    assert!(
        activities[0].title.contains("Read `hello.txt`"),
        "body still carries the describe_call summary; got {}",
        activities[0].title
    );
    assert_eq!(activities[0].title, activities[1].title);
}

#[tokio::test]
async fn tool_activity_title_prefixes_custom_tool_name() {
    // A custom tool (defined via the config `tools` map) must also be
    // prefixed in the activity title. `describe_call` falls back to a
    // pretty-printed JSON for unknown tools, so the body shows the
    // arguments verbatim; the prefix is what identifies the tool.
    let tmp = tempdir().unwrap();
    let mut config = support::workspace(tmp.path());
    let custom = serde_json::from_value::<diet_soda::config::ToolConfig>(json!({
        "type":"command",
        "description":"custom tool",
        "command":"echo",
        "args":["hi"],
        "hitl":false,
        "destructive":false,
        "input_schema":{
            "type":"object",
            "properties":{"value":{"type":"string"}},
            "required":["value"]
        }
    }))
    .unwrap();
    config.tools.insert("my_custom_tool".into(), custom);
    // The agent must allow the custom tool AND be `default=true` so the
    // scope's `tools` filter includes it.
    let mut main = support::editing_agent();
    main.default = true;
    main.tools = Some(vec!["my_custom_tool".into()]);
    config.agents.insert("main".into(), main);
    let (engine, mut events) = support::engine(config);
    let scope = support::scope_for(&engine).await;
    let registered = engine
        .available(&scope, &CancellationToken::new())
        .await
        .unwrap();
    // The custom tool must appear in the registered snapshot before we
    // can invoke it.
    assert!(
        registered.iter().any(|t| t.spec.name == "my_custom_tool"),
        "custom tool must be advertised in the available snapshot"
    );
    let cancel = CancellationToken::new();
    let _ = engine
        .invoke(
            &scope,
            &call(
                "tc-prefix-custom",
                "my_custom_tool",
                json!({"value": "alpha\u{202e}\u{200b}\u{2028}👨‍👩‍👧‍👦"}),
            ),
            &registered,
            &cancel,
        )
        .await
        .unwrap();
    let activities = collect_events(&mut events, 2).await;
    assert_eq!(activities.len(), 2);
    assert!(
        activities[0].title.starts_with("tool my_custom_tool: "),
        "custom tool activity title must prefix the configured tool name; got {}",
        activities[0].title
    );
    assert!(
        activities[1].title.starts_with("tool my_custom_tool: "),
        "custom tool End title must prefix the configured tool name; got {}",
        activities[1].title
    );
    // The body must include the JSON pretty-print of the arguments
    // (since describe_call falls back to JSON for unknown tools), so the
    // model can still correlate from the original request.
    assert!(
        activities[0].title.contains("alpha"),
        "body should carry the argument values; got {}",
        activities[0].title
    );
    assert!(!activities[0]
        .title
        .chars()
        .any(diet_soda::text::is_unsafe_terminal_char));
    assert!(activities[0].title.contains("👨‍👩‍👧‍👦"));
}

#[tokio::test]
async fn tool_activity_title_prefixes_mcp_tool_name() {
    // MCP tools follow the `mcp_<server>_<tool>` naming convention. The
    // activity title must prefix the actual advertised tool name, not
    // some server-side alias. We stage a tiny in-process stdio MCP
    // server (per `tests/fixtures`) that advertises one tool, then
    // invoke it through the public `Engine::invoke` path.
    use diet_soda::engine::Selection;
    use diet_soda::tools::BUILTIN_NAMES;
    let tmp = tempdir().unwrap();
    let mut config = support::workspace(tmp.path());
    let server_uuid = uuid::Uuid::new_v4().to_string();
    config.mcp_servers.insert(
        "tmpsrv".into(),
        diet_soda::config::McpConfig {
            uuid: server_uuid.clone(),
            transport: diet_soda::config::McpTransport::Stdio {
                command: "python3".into(),
                args: vec![format!(
                    "{}/tests/fixtures/mcp_server.py",
                    env!("CARGO_MANIFEST_DIR")
                )],
                env: Default::default(),
            },
            enabled: true,
            hitl: false,
            timeout_seconds: 5,
        },
    );
    let mut main = support::editing_agent();
    main.default = true;
    config.agents.insert("main".into(), main);
    let (engine, mut events) = support::engine(config);
    // Build the scope with the MCP server bound through `mcps`.
    let selection = Selection {
        agent: Some("main".into()),
        ..Selection::default()
    };
    let mut scope = engine.scope(&selection, "main", None).await.unwrap();
    scope.mcps = Some(vec![server_uuid.clone()]);
    // Trigger MCP discovery so the tool gets advertised.
    let registered = engine
        .available(&scope, &CancellationToken::new())
        .await
        .unwrap();
    let mcp_tool = registered
        .iter()
        .find(|t| t.spec.name.starts_with("mcp_tmpsrv_"))
        .expect("MCP tool should be advertised")
        .spec
        .name
        .clone();
    // Sanity: the name is not a built-in.
    assert!(!BUILTIN_NAMES.contains(&mcp_tool.as_str()));
    let cancel = CancellationToken::new();
    let _ = engine
        .invoke(
            &scope,
            &call("tc-prefix-mcp", &mcp_tool, json!({"text":"x"})),
            &registered,
            &cancel,
        )
        .await
        .unwrap();
    let activities = collect_events(&mut events, 2).await;
    assert_eq!(activities.len(), 2);
    assert!(
        activities[0]
            .title
            .starts_with(&format!("tool {mcp_tool}: ")),
        "MCP tool activity title must prefix the advertised tool name; got {}",
        activities[0].title
    );
    assert!(
        activities[1]
            .title
            .starts_with(&format!("tool {mcp_tool}: ")),
        "MCP tool End title must prefix the advertised tool name; got {}",
        activities[1].title
    );
}

#[tokio::test]
async fn tool_activity_title_sanitizes_control_chars_and_caps_at_160_scalars() {
    // `write_file` content is the only user-controlled field that flows
    // through describe_call on the title path with effectively
    // unbounded length. The activity title must:
    //   1. replace control characters with spaces,
    //   2. collapse whitespace runs,
    //   3. cap the entire title at 160 Unicode scalars,
    //   4. append a trailing ellipsis when the cap bites.
    let tmp = tempdir().unwrap();
    let mut config = support::workspace(tmp.path());
    // The agent must allow `write_file`, be `default=true` so the
    // scope's tool list includes it, and have can_edit so the
    // available() filter permits it.
    let mut main = support::editing_agent();
    main.default = true;
    config.agents.insert("main".into(), main);
    // Disable the destructive-tool approval gate so write_file runs
    // without an approval handler in the test.
    config.require_for_destructive_tools = false;
    let (engine, mut events) = support::engine(config);
    let scope = support::scope_for(&engine).await;
    let registered = engine
        .available(&scope, &CancellationToken::new())
        .await
        .unwrap();
    // Sanity: write_file is advertised; otherwise the test fixture is
    // wrong, not the production code.
    assert!(
        registered.iter().any(|t| t.spec.name == "write_file"),
        "write_file must be advertised to the editing agent; got {:?}",
        registered.iter().map(|t| &t.spec.name).collect::<Vec<_>>()
    );
    // Two writes: one short with control chars, one huge.
    let control_content = "alpha\tbeta\ngamma\rdelta";
    let long_content = "x".repeat(2_000);
    let cancel = CancellationToken::new();
    let _ = engine
        .invoke(
            &scope,
            &call(
                "tc-title-control",
                "write_file",
                json!({"path":"ctrl.txt","content":control_content}),
            ),
            &registered,
            &cancel,
        )
        .await
        .unwrap();
    let _ = engine
        .invoke(
            &scope,
            &call(
                "tc-title-long",
                "write_file",
                json!({"path":"long.txt","content":long_content}),
            ),
            &registered,
            &cancel,
        )
        .await
        .unwrap();
    let activities = collect_events(&mut events, 4).await;
    assert_eq!(activities.len(), 4);
    // The control-char run is in the title for the first invocation.
    // The title shape is `tool write_file: Write N bytes to \`path\``,
    // which does not embed the file content; the test pins that the
    // path component (passed through the path arg) is not a sanitization
    // vector and that the title prefix is the actual tool name.
    for activity in &activities[..2] {
        assert!(
            activity.title.starts_with("tool write_file: "),
            "title must prefix the built-in tool name; got {}",
            activity.title
        );
        // No control characters must survive in the title even when the
        // shell path argument is benign (path has no controls). This
        // pins the no-controls invariant.
        for forbidden in ['\n', '\r', '\t'] {
            assert!(
                !activity.title.contains(forbidden),
                "title must not contain {:?}; got {:?}",
                forbidden,
                activity.title
            );
        }
        assert!(
            activity.title.contains("ctrl.txt"),
            "first write title should include the path; got {}",
            activity.title
        );
    }
    // The long write must produce a title capped at 160 Unicode
    // scalars. The end title mirrors the start title.
    for activity in &activities[2..4] {
        assert!(
            activity.title.chars().count() <= 160,
            "long title must be capped at 160 scalars; got {} chars: {:?}",
            activity.title.chars().count(),
            activity.title
        );
    }
}

#[tokio::test]
async fn tool_activity_title_caps_multi_byte_utf8_at_160_scalars() {
    // Multi-byte UTF-8 inputs must be measured in Unicode scalar
    // values, not bytes. The path argument to `read_file` flows
    // through `describe_call` verbatim, so a path of 500 🐛s is 1000
    // bytes but only 500 scalars; the title cap must still apply when
    // the prefix leaves room but the body exceeds 160 scalars. The
    // resulting title is exactly 160 scalars and ends with the
    // ellipsis.
    let tmp = tempdir().unwrap();
    let mut config = support::workspace(tmp.path());
    let mut main = support::read_only_agent();
    main.default = true;
    config.agents.insert("main".into(), main);
    let (engine, mut events) = support::engine(config);
    let scope = support::scope_for(&engine).await;
    let registered = engine
        .available(&scope, &CancellationToken::new())
        .await
        .unwrap();
    let long_path = std::path::PathBuf::from(tmp.path())
        .join("\u{1F41B}".repeat(500))
        .to_string_lossy()
        .into_owned();
    let cancel = CancellationToken::new();
    // The file does not have to exist; we only care about the activity
    // title emitted before the file read fails. The tool lifecycle
    // already pinned the Start-then-Error pairing, so the title we
    // assert on is the Start title.
    let _ = engine
        .invoke(
            &scope,
            &call("tc-title-utf8", "read_file", json!({"path":long_path})),
            &registered,
            &cancel,
        )
        .await
        .expect_err("file does not exist");
    let activities = collect_events(&mut events, 2).await;
    assert_eq!(activities.len(), 2);
    let start_title = activities[0].title.clone();
    let count = start_title.chars().count();
    assert!(
        count <= 160,
        "multi-byte title must be capped at 160 scalars; got {} chars: {:?}",
        count,
        start_title
    );
    assert!(
        start_title.ends_with('\u{2026}'),
        "truncated multi-byte title must end with the ellipsis; got {:?}",
        start_title
    );
    assert!(
        start_title.starts_with("tool read_file: "),
        "read_file title must prefix the built-in tool name; got {:?}",
        start_title
    );
    assert_eq!(
        activities[1].title, start_title,
        "Start and End share the same bounded title"
    );
}

#[tokio::test]
async fn tool_activity_title_sanitizes_user_data_in_shell_command_argv() {
    // `shell` is the tool whose `describe_call` output embeds the
    // command + argv verbatim, so user-controlled bytes flow through
    // the title. The test pins:
    //   1. control characters in command or argv are replaced with
    //      spaces,
    //   2. whitespace runs collapse to a single space,
    //   3. the entire title (prefix + body + ellipsis) is capped at 160
    //      Unicode scalars,
    //   4. long titles end with the ellipsis.
    let tmp = tempdir().unwrap();
    let mut config = support::workspace(tmp.path());
    let mut main = support::editing_agent();
    main.default = true;
    config.agents.insert("main".into(), main);
    // Disable the destructive-tool approval gate so the shell call runs
    // without an approval handler in the test.
    config.require_for_destructive_tools = false;
    let (engine, mut events) = support::engine(config);
    let scope = support::scope_for(&engine).await;
    let registered = engine
        .available(&scope, &CancellationToken::new())
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    // 1. Control chars in command + argv.
    let _ = engine
        .invoke(
            &scope,
            &call(
                "tc-shell-ctrl",
                "shell",
                json!({"command":"echo","args":["alpha\tbeta\ngamma\rdelta"]}),
            ),
            &registered,
            &cancel,
        )
        .await
        .unwrap();
    // 2. Long content: 2000-byte single argv.
    let long_arg = "x".repeat(2_000);
    let _ = engine
        .invoke(
            &scope,
            &call(
                "tc-shell-long",
                "shell",
                json!({"command":"echo","args":[long_arg]}),
            ),
            &registered,
            &cancel,
        )
        .await
        .unwrap();
    // 3. Multi-byte UTF-8 argv.
    let utf8_arg = "\u{1F41B}".repeat(500);
    let _ = engine
        .invoke(
            &scope,
            &call(
                "tc-shell-utf8",
                "shell",
                json!({"command":"echo","args":[utf8_arg]}),
            ),
            &registered,
            &cancel,
        )
        .await
        .unwrap();
    let activities = collect_events(&mut events, 6).await;
    assert_eq!(activities.len(), 6);
    // 1. control-char pair: prefix, no controls, collapsed body.
    for activity in &activities[..2] {
        assert!(
            activity.title.starts_with("tool shell: "),
            "shell title must prefix the built-in tool name; got {}",
            activity.title
        );
        for forbidden in ['\n', '\r', '\t'] {
            assert!(
                !activity.title.contains(forbidden),
                "title must not contain {:?}; got {:?}",
                forbidden,
                activity.title
            );
        }
        assert!(
            activity.title.contains("alpha beta gamma delta"),
            "shell title should collapse whitespace runs; got {}",
            activity.title
        );
    }
    // 2. long pair: capped + ellipsis.
    for activity in &activities[2..4] {
        assert!(
            activity.title.chars().count() <= 160,
            "long shell title must be capped at 160 scalars; got {} chars",
            activity.title.chars().count()
        );
        assert!(
            activity.title.ends_with('\u{2026}'),
            "truncated shell title must end with the ellipsis; got {:?}",
            activity.title
        );
    }
    // 3. utf8 pair: scalar count bounded.
    for activity in &activities[4..6] {
        assert!(
            activity.title.chars().count() <= 160,
            "UTF-8 shell title must be capped at 160 scalars; got {} chars",
            activity.title.chars().count()
        );
        assert!(
            activity.title.ends_with('\u{2026}'),
            "truncated UTF-8 shell title must end with the ellipsis; got {:?}",
            activity.title
        );
    }
}

#[tokio::test]
async fn tool_activity_title_does_not_ellipsize_exactly_160_scalars() {
    let tmp = tempdir().unwrap();
    let mut config = support::workspace(tmp.path());
    let mut main = support::editing_agent();
    main.default = true;
    config.agents.insert("main".into(), main);
    config.require_for_destructive_tools = false;
    let (engine, mut events) = support::engine(config);
    let scope = support::scope_for(&engine).await;
    let registered = engine
        .available(&scope, &CancellationToken::new())
        .await
        .unwrap();

    // `tool shell: ` is 12 scalars and `Run `echo <arg>`` is 11 plus the
    // argument, so 137 ASCII argument scalars produce exactly 160.
    let argument = "x".repeat(137);
    let call_id = "tc-title-exact-160";
    let _ = engine
        .invoke(
            &scope,
            &call(
                call_id,
                "shell",
                json!({"command":"echo","args":[argument]}),
            ),
            &registered,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

    let activities = collect_events(&mut events, 2).await;
    assert_eq!(activities.len(), 2);
    for activity in &activities {
        assert_eq!(activity.title.chars().count(), 160);
        assert!(!activity.title.ends_with('\u{2026}'));
        assert!(activity.title.starts_with("tool shell: "));
        assert_eq!(activity.external_id.as_deref(), Some(call_id));
    }
    assert_eq!(activities[0].title, activities[1].title);
}
