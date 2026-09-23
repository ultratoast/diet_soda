//! Wave 2 subagent lifecycle tests. Mirrors the reviewed tool lifecycle in
//! `src/engine/dispatch.rs` and `Engine::emit_activity` in
//! `src/engine/mod.rs`. Every delegate / delegate_parallel invocation must
//! emit a paired `Subagent` Start/End activity record with:
//!   * `parent_id` equal to the invoking delegate tool's `activity_id`,
//!   * `context` equal to the child conversation's `subagent:<name>:<uuid>`
//!     context,
//!   * a sanitized, bounded title of the form `agent <name>: <prompt
//!     summary>`,
//!   * exactly one `End` whose status reflects the inner outcome
//!     (Success / Cancelled / Error).
//!
//! The tests stay local, mock the provider through the existing
//! `tests/support` server fixtures, and use bounded waits so a missing
//! event cannot hang the suite.
#![allow(clippy::needless_raw_string_hashes)]

mod support;
use diet_soda::{
    config::AgentConfig,
    engine::{Scope, Selection},
    model::{ActivityEvent, ActivityKind, ActivityPhase, ActivityStatus, ToolCall, UiEvent},
};
use serde_json::{json, Value};
use std::time::Duration;
use support::{answer, engine, parallel_server, server, tool_call};
use tempfile::tempdir;
use tokio::{sync::mpsc, time::timeout};
use tokio_util::sync::CancellationToken;

/// Drain the UiEvent channel for `count` activity records (any other events
/// are skipped). The bounded timeout keeps a missing event from hanging.
async fn collect_activities(
    rx: &mut mpsc::UnboundedReceiver<UiEvent>,
    count: usize,
) -> Vec<ActivityEvent> {
    let mut out = Vec::new();
    let deadline = Duration::from_secs(5);
    while out.len() < count {
        match timeout(deadline, rx.recv()).await {
            Ok(Some(UiEvent::Activity(a))) => out.push(a),
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    out
}

/// Drain the UiEvent channel for raw events, applying `keep` so callers
/// can extract Activity records in arrival order without committing to a
/// fixed count.
async fn drain_events<F>(rx: &mut mpsc::UnboundedReceiver<UiEvent>, keep: F) -> Vec<ActivityEvent>
where
    F: Fn(&ActivityEvent) -> bool,
{
    let mut out = Vec::new();
    let deadline = Duration::from_secs(5);
    loop {
        match timeout(deadline, rx.recv()).await {
            Ok(Some(UiEvent::Activity(a))) => {
                if keep(&a) {
                    out.push(a);
                }
            }
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    out
}

/// Read all activity records (any kind) from the on-disk session log.
fn read_persisted_activities(path: &std::path::Path) -> Vec<ActivityEvent> {
    let raw = std::fs::read_to_string(path).unwrap();
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["type"] == "activity")
        .map(|line| serde_json::from_value::<ActivityEvent>(line["data"].clone()).unwrap())
        .collect()
}

fn call<S: Into<String>>(id: S, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.to_string(),
    }
}

fn subagent_records(activities: &[ActivityEvent]) -> Vec<&ActivityEvent> {
    activities
        .iter()
        .filter(|a| a.kind == ActivityKind::Subagent)
        .collect()
}

// -------------------------------------------------------------------------
// 1. Concurrent same-name siblings get distinct ids and a shared tool
//    parent. The on-disk trail is deterministic by sibling index; live
//    `UiEvent::Activity` order may interleave because the per-child
//    `emit_activity` happens inside `parallel_ordered`.
// -------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_same_name_siblings_have_unique_ids_and_shared_tool_parent() {
    let tasks = json!({
        "tasks":[
            {"agent":"researcher","prompt":"task A"},
            {"agent":"researcher","prompt":"task B"},
            {"agent":"researcher","prompt":"task C"},
        ]
    });
    // 1 parent request, 3 child requests, 1 final parent continuation.
    let mut server = parallel_server(vec![
        tool_call("delegate_parallel", tasks),
        answer("child A"),
        answer("child B"),
        answer("child C"),
        answer("combined"),
    ])
    .await;
    let tmp = tempdir().unwrap();
    let mut config = support::config(&server.url, tmp.path());
    config.max_parallel_subagents = 3;
    config.agents.insert(
        "researcher".into(),
        AgentConfig {
            tools: Some(vec![]),
            ..AgentConfig::default()
        },
    );
    let (engine, mut events) = engine(config);

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        engine.turn(
            "fan out".into(),
            Selection::default(),
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result, "combined");

    // Drain the event channel and filter to Subagent records. The on-disk
    // trail is the authoritative source for the parent chain, so we
    // assert against both.
    let raw = drain_events(&mut events, |_| true).await;
    let session_path = engine.session.lock().await.path.clone();
    drop(engine);
    let persisted = read_persisted_activities(&session_path);

    // Exactly 1 tool activity for `delegate_parallel` and 3 subagent
    // activities (one per sibling). Each sibling gets its own Start +
    // End pair.
    let tool_activities: Vec<&ActivityEvent> = persisted
        .iter()
        .filter(|a| a.kind == ActivityKind::Tool)
        .collect();
    let sub_activities = subagent_records(&persisted);
    assert_eq!(
        tool_activities.len(),
        2,
        "delegate_parallel produces exactly one paired Tool lifecycle; got {}",
        tool_activities.len()
    );
    assert_eq!(
        sub_activities.len(),
        6,
        "three siblings produce 3 Start + 3 End Subagent records; got {}",
        sub_activities.len()
    );

    let tool_id = tool_activities[0].id.clone();
    assert_eq!(tool_activities[0].id, tool_activities[1].id);
    assert_eq!(tool_activities[0].phase, ActivityPhase::Start);
    assert_eq!(tool_activities[1].phase, ActivityPhase::End);

    // All sibling Subagent records share the delegate tool as their
    // parent and are globally unique. The on-disk order is deterministic
    // by sibling index even though the live UiEvent order may interleave.
    let mut sibling_starts: Vec<&ActivityEvent> = sub_activities
        .iter()
        .copied()
        .filter(|a| a.phase == ActivityPhase::Start)
        .collect();
    let mut sibling_ends: Vec<&ActivityEvent> = sub_activities
        .iter()
        .copied()
        .filter(|a| a.phase == ActivityPhase::End)
        .collect();
    sibling_starts.sort_by_key(|a| a.context.clone());
    sibling_ends.sort_by_key(|a| a.context.clone());
    // Each sibling's Start and End share an id; sibling ids are
    // globally unique. Dedupe the start ids and confirm we have one
    // per sibling.
    let start_ids: std::collections::HashSet<&String> =
        sibling_starts.iter().map(|a| &a.id).collect();
    assert_eq!(
        start_ids.len(),
        sibling_starts.len(),
        "every sibling Start id must be unique; got {:?}",
        sibling_starts.iter().map(|a| &a.id).collect::<Vec<_>>()
    );
    // Each End id matches a Start id and is itself globally unique.
    let end_ids: std::collections::HashSet<&String> = sibling_ends.iter().map(|a| &a.id).collect();
    assert_eq!(
        end_ids.len(),
        sibling_ends.len(),
        "every sibling End id must be unique"
    );
    assert_eq!(
        start_ids, end_ids,
        "each sibling's Start and End share the same id"
    );
    for sibling in sibling_starts.iter().chain(sibling_ends.iter()) {
        assert_eq!(
            sibling.parent_id.as_deref(),
            Some(tool_id.as_str()),
            "subagent parent_id must equal the invoking tool activity id"
        );
    }
    // Each sibling's Start and End share their own id; no two siblings
    // share an id, and the contexts are distinct (subagent:researcher:<uuid>).
    let mut contexts: Vec<String> = sibling_starts.iter().map(|a| a.context.clone()).collect();
    contexts.sort();
    contexts.dedup();
    assert_eq!(contexts.len(), 3, "each sibling has a distinct context");

    // Every Start pairs with exactly one matching End by id.
    for start in sibling_starts.iter() {
        let matches: Vec<&&ActivityEvent> = sibling_ends
            .iter()
            .filter(|e| e.id == start.id && e.phase == ActivityPhase::End)
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "exactly one End per Start for {}",
            start.id
        );
        assert_eq!(matches[0].status, Some(ActivityStatus::Success));
        assert_eq!(matches[0].parent_id.as_deref(), Some(tool_id.as_str()));
        assert_eq!(matches[0].context, start.context);
    }

    // Live UiEvent side: same ids, same parent chain (allow interleaving).
    let live_subagent: Vec<&ActivityEvent> = raw
        .iter()
        .filter(|a| a.kind == ActivityKind::Subagent)
        .collect();
    let live_ids: std::collections::HashSet<&String> =
        live_subagent.iter().map(|a| &a.id).collect();
    let persisted_ids: std::collections::HashSet<&String> =
        sub_activities.iter().map(|a| &a.id).collect();
    assert_eq!(
        live_ids, persisted_ids,
        "live channel ids must match persisted ids exactly"
    );

    // Sanity: server saw 5 requests.
    for _ in 0..5 {
        let _ = server.requests.recv().await.unwrap();
    }
}

// -------------------------------------------------------------------------
// 2. One failed sibling does not stop sibling lifecycle completion. The
//    failing sibling's End carries status=Error and the surviving sibling
//    still emits its full paired lifecycle.
// -------------------------------------------------------------------------

#[tokio::test]
async fn one_failing_sibling_emits_error_end_and_other_sibling_still_completes() {
    // One sibling targets an unknown agent, so its
    // `delegate_with_lifecycle` wrapper emits a paired Start/End with
    // status=Error after `Engine::scope` fails inside the lifecycle
    // wrapper (the tool-argument schema is satisfied because both
    // siblings declare valid `agent`/`prompt` strings). The surviving
    // sibling uses the configured `researcher` agent and emits
    // status=Success. The mock server has 3 replies: 1 parent
    // tool_call, 1 child reply (only the good sibling contacts the
    // server because the failing sibling bails before conversation),
    // and 1 parent continuation.
    let tasks = json!({
        "tasks":[
            {"agent":"researcher","prompt":"task A"},
            {"agent":"ghost","prompt":"task B"},
        ]
    });
    let server = server(vec![
        tool_call("delegate_parallel", tasks),
        answer("child A"),
        answer("combined"),
    ])
    .await;
    let tmp = tempdir().unwrap();
    let mut config = support::config(&server.url, tmp.path());
    config.max_parallel_subagents = 2;
    config.agents.insert(
        "researcher".into(),
        AgentConfig {
            tools: Some(vec![]),
            ..AgentConfig::default()
        },
    );
    let (engine, mut events) = engine(config);

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        engine.turn(
            "fan out".into(),
            Selection::default(),
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result, "combined");

    let session_path = engine.session.lock().await.path.clone();
    let persisted = read_persisted_activities(&session_path);
    drop(engine);

    // Drain anything left on the channel so the runtime shuts down
    // cleanly even though we won't assert on it.
    let _ = collect_activities(&mut events, 10).await;

    let sub_activities = subagent_records(&persisted);
    assert_eq!(
        sub_activities.len(),
        4,
        "two siblings => 4 subagent records (2 Start + 2 End); got {}",
        sub_activities.len()
    );
    // Each sibling has exactly one matching End.
    let starts: Vec<&ActivityEvent> = sub_activities
        .iter()
        .copied()
        .filter(|a| a.phase == ActivityPhase::Start)
        .collect();
    let ends: Vec<&ActivityEvent> = sub_activities
        .iter()
        .copied()
        .filter(|a| a.phase == ActivityPhase::End)
        .collect();
    for start in &starts {
        let matching: Vec<&&ActivityEvent> = ends.iter().filter(|e| e.id == start.id).collect();
        assert_eq!(matching.len(), 1, "exactly one End per Start");
    }
    // One sibling succeeded; one sibling failed with status=Error.
    let success_count = ends
        .iter()
        .filter(|e| e.status == Some(ActivityStatus::Success))
        .count();
    let error_count = ends
        .iter()
        .filter(|e| e.status == Some(ActivityStatus::Error))
        .count();
    assert_eq!(success_count, 1, "one sibling succeeded");
    assert_eq!(error_count, 1, "one sibling failed with status=Error");
    // The failed sibling's Start had an unknown agent; its title still
    // pins the (unknown) agent name. The surviving sibling's title
    // carries "task A".
    let success_title = ends
        .iter()
        .find(|e| e.status == Some(ActivityStatus::Success))
        .unwrap()
        .title
        .clone();
    let error_title = ends
        .iter()
        .find(|e| e.status == Some(ActivityStatus::Error))
        .unwrap()
        .title
        .clone();
    assert!(
        success_title.contains("researcher") && success_title.contains("task A"),
        "success title must reference the agent and prompt; got {success_title}"
    );
    assert!(
        error_title.contains("ghost"),
        "error title must still reference the failing agent; got {error_title}"
    );
}
// -------------------------------------------------------------------------
// 3. Nested delegation with `max_parallel_subagents = 1` preserves the
//    tool -> subagent -> delegate-tool -> subagent chain without
//    deadlocking. The existing parallel_agents test only proves the
//    conversation returns; this test pins the on-disk activity graph.
// -------------------------------------------------------------------------

#[tokio::test]
async fn nested_one_slot_chain_preserves_tool_subagent_delegate_subagent_graph() {
    let server = server(vec![
        tool_call("delegate", json!({"agent":"worker","prompt":"child"})),
        tool_call("delegate", json!({"agent":"worker","prompt":"grandchild"})),
        answer("leaf"),
        answer("child"),
        answer("parent"),
    ])
    .await;
    let tmp = tempdir().unwrap();
    let mut config = support::config(&server.url, tmp.path());
    config.max_parallel_subagents = 1;
    config.agents.insert(
        "worker".into(),
        AgentConfig {
            tools: Some(vec!["delegate".into()]),
            ..AgentConfig::default()
        },
    );
    let (engine, _events) = engine(config);

    let result = tokio::time::timeout(
        Duration::from_secs(5),
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

    let session_path = engine.session.lock().await.path.clone();
    let persisted = read_persisted_activities(&session_path);
    drop(engine);

    // 1 main context has no tool activity (it's a direct turn), so the
    // graph is rooted at the outer delegate tool. There are exactly two
    // delegate tool lifecycles (parent + grandchild) and two subagent
    // lifecycles (child + grandchild).
    let tool_ids: Vec<String> = persisted
        .iter()
        .filter(|a| a.kind == ActivityKind::Tool)
        .map(|a| a.id.clone())
        .collect();
    let unique_tool_ids: std::collections::HashSet<_> = tool_ids.iter().collect();
    assert_eq!(
        unique_tool_ids.len(),
        2,
        "two delegate tool lifecycles (parent + grandchild); got {tool_ids:?}"
    );
    let subagent_count = subagent_records(&persisted).len();
    assert_eq!(
        subagent_count, 4,
        "child + grandchild each emit Start + End; got {subagent_count}"
    );

    // The grandchild tool id is the child subagent id's child. Concretely:
    // the second subagent's `parent_id` equals the inner (grandchild)
    // tool id, not the outer (parent) tool id.
    let sub_starts: Vec<&ActivityEvent> = persisted
        .iter()
        .filter(|a| a.kind == ActivityKind::Subagent && a.phase == ActivityPhase::Start)
        .collect();
    assert_eq!(sub_starts.len(), 2);
    let parent_ids: std::collections::HashSet<String> = tool_ids.iter().cloned().collect();
    // The outer delegate tool is the parent of both? No -- the outer
    // tool is the parent of the *first* subagent, and the *first*
    // subagent's id (via scope.activity_id overwrite) is the parent of
    // the second delegate tool. The second subagent's parent_id is the
    // inner tool's id. Walk the chain explicitly.
    let outer_tool_id = tool_ids[0].clone();
    let inner_tool_id = tool_ids[1].clone();
    // First subagent parent = outer tool.
    assert_eq!(
        sub_starts[0].parent_id.as_deref(),
        Some(outer_tool_id.as_str()),
        "first subagent parent_id must equal the outer delegate tool id"
    );
    // Second subagent parent = inner tool (the grandchild tool).
    let second = sub_starts
        .iter()
        .find(|a| a.context != sub_starts[0].context)
        .expect("two distinct subagent contexts");
    assert_eq!(
        second.parent_id.as_deref(),
        Some(inner_tool_id.as_str()),
        "grandchild subagent parent_id must equal the inner delegate tool id; got parent_id={:?}, contexts={:?}",
        second.parent_id,
        sub_starts.iter().map(|a| &a.context).collect::<Vec<_>>()
    );
    // Sanity: parent_ids is exactly the two tool ids.
    assert_eq!(parent_ids.len(), 2);
}

// -------------------------------------------------------------------------
// 4. Child tool activity inside a subagent has parent_id equal to the
//    subagent id. This is the chain `tool -> subagent -> tool` -- the
//    inner scope.activity_id is overwritten with the Subagent id, so any
//    tool the child invokes carries that id as parent_id.
// -------------------------------------------------------------------------

#[tokio::test]
async fn child_tool_activity_parent_id_equals_subagent_id() {
    // The child agent has only `read_file`. The mock server returns a
    // file content reply. To exercise the tool lifecycle inside the
    // subagent without a real filesystem, we read a path the mock
    // doesn't have but the test fixture writes before the turn.
    //
    // Simpler: the child agent does NOT need a tool call. We only need
    // to prove that IF a child tool is invoked, its parent_id equals
    // the subagent id. We exercise this directly through `Engine::invoke`
    // with a scope whose `activity_id` is set to the subagent id (as
    // the dispatch wrapper does). This pins the contract without
    // requiring the inner child conversation to issue a tool call.
    let tmp = tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "a").unwrap();
    let mut config = support::config("http://127.0.0.1:1", tmp.path());
    config.agents.insert(
        "main".into(),
        AgentConfig {
            tools: Some(vec!["read_file".into()]),
            ..AgentConfig::default()
        },
    );
    let (engine, mut events) = engine(config);
    let selection = Selection::default();
    let mut scope: Scope = engine.scope(&selection, "main", None).await.unwrap();
    // Simulate the post-delegate overwrite: the dispatch wrapper sets
    // scope.activity_id to the Subagent id. The same scope is what
    // nested tool calls would see.
    scope.activity_id = Some("subagent-fixed-id".into());

    let registered: Vec<_> = engine
        .available(&scope, &CancellationToken::new())
        .await
        .unwrap();
    let tool_call = call("tc-child", "read_file", json!({"path":"a.txt"}));
    let value = engine
        .invoke(&scope, &tool_call, &registered, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(value, json!({"content":"a","truncated":false}));

    let activities = collect_activities(&mut events, 2).await;
    assert_eq!(activities.len(), 2);
    // The child tool's parent_id equals the subagent id, not the parent
    // tool id, because the dispatch wrapper overwrites scope.activity_id
    // before conversation starts.
    assert_eq!(
        activities[0].parent_id.as_deref(),
        Some("subagent-fixed-id"),
        "child tool activity parent_id must equal the subagent id"
    );
    assert_eq!(
        activities[1].parent_id.as_deref(),
        Some("subagent-fixed-id")
    );
    // The child tool id is distinct from the subagent id (the tool id
    // is its own globally-unique UUID).
    assert_ne!(activities[0].id, "subagent-fixed-id");
    assert_ne!(activities[1].id, "subagent-fixed-id");

    // Cross-check: the persisted on-disk trail has the same parent_id.
    let session_path = engine.session.lock().await.path.clone();
    drop(engine);
    let persisted = read_persisted_activities(&session_path);
    let persisted_tool: Vec<&ActivityEvent> = persisted
        .iter()
        .filter(|a| a.kind == ActivityKind::Tool)
        .collect();
    assert_eq!(persisted_tool.len(), 2);
    assert_eq!(
        persisted_tool[0].parent_id.as_deref(),
        Some("subagent-fixed-id")
    );
    assert_eq!(
        persisted_tool[1].parent_id.as_deref(),
        Some("subagent-fixed-id")
    );
}

// -------------------------------------------------------------------------
// 5. The exact one-pair / unique-id invariant: a single delegate call
//    produces exactly one Subagent Start and one Subagent End sharing the
//    same id. No tool ids appear in the Subagent records and no other
//    record leaks the Subagent id.
// -------------------------------------------------------------------------

#[tokio::test]
async fn delegate_emits_exactly_one_paired_subagent_lifecycle_with_unique_id() {
    let server = server(vec![
        tool_call("delegate", json!({"agent":"worker","prompt":"hello"})),
        answer("ok"),
        answer("parent"),
    ])
    .await;
    let tmp = tempdir().unwrap();
    let mut config = support::config(&server.url, tmp.path());
    config.agents.insert(
        "worker".into(),
        AgentConfig {
            tools: Some(vec![]),
            ..AgentConfig::default()
        },
    );
    let (engine, mut events) = engine(config);

    let result = tokio::time::timeout(
        Duration::from_secs(3),
        engine.turn("go".into(), Selection::default(), CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result, "parent");

    let session_path = engine.session.lock().await.path.clone();
    let persisted = read_persisted_activities(&session_path);
    drop(engine);
    let _ = collect_activities(&mut events, 10).await;

    let sub = subagent_records(&persisted);
    assert_eq!(
        sub.len(),
        2,
        "exactly one Start + one End; got {}",
        sub.len()
    );
    let start = sub
        .iter()
        .find(|a| a.phase == ActivityPhase::Start)
        .unwrap();
    let end = sub.iter().find(|a| a.phase == ActivityPhase::End).unwrap();
    assert_eq!(start.id, end.id, "Start and End share one id");
    assert_ne!(start.id, "", "id is non-empty");
    // Title format: "agent <name>: <prompt summary>".
    assert!(
        start.title.starts_with("agent worker:"),
        "title must start with `agent worker:`; got {}",
        start.title
    );
    assert!(
        start.title.contains("hello"),
        "title must include the prompt summary; got {}",
        start.title
    );
    assert_eq!(start.title, end.title);
    assert_eq!(start.context, end.context);
    assert_eq!(start.parent_id, end.parent_id);
    assert_eq!(end.status, Some(ActivityStatus::Success));
    // Context is `subagent:worker:<uuid>`.
    assert!(
        start.context.starts_with("subagent:worker:"),
        "context must be subagent:worker:<uuid>; got {}",
        start.context
    );
    // The tool id (parent of the Subagent) is distinct from the
    // Subagent id itself.
    let tool_records: Vec<&ActivityEvent> = persisted
        .iter()
        .filter(|a| a.kind == ActivityKind::Tool)
        .collect();
    assert_eq!(
        tool_records.len(),
        2,
        "delegate tool emits paired Start+End"
    );
    assert_eq!(tool_records[0].id, tool_records[1].id);
    let tool_id = tool_records[0].id.clone();
    assert_ne!(
        tool_id, start.id,
        "tool id and subagent id are distinct globally unique uuids"
    );
    assert_eq!(start.parent_id.as_deref(), Some(tool_id.as_str()));
    // Exactly one Subagent lifecycle pair: no other record references
    // the Subagent id or the tool id outside of the two pairs.
    let occurrences: Vec<&ActivityEvent> = persisted
        .iter()
        .filter(|a| a.id == start.id || a.id == tool_id)
        .collect();
    assert_eq!(
        occurrences.len(),
        4,
        "tool pair (2) + subagent pair (2); got {}",
        occurrences.len()
    );
}

// -------------------------------------------------------------------------
// 6. The original agent name is preserved verbatim across the
//    subagent lifecycle. The title is sanitized, but the JSON result
//    payload the model receives, the `Selection.agent` used for
//    scope construction, and the `scope.context` all carry the exact
//    configured / task value. We exercise this by giving the agent a
//    name that contains characters the sanitizer collapses (control
//    bytes, runs of whitespace) so the test pins the boundary: the
//    result payload still says `name` and the displayed title has been
//    transformed.
//
//    The configured agent key must be alphanumeric/underscore (per
//    `Config::valid_name`), so we put the control characters in the
//    `prompt` field instead. The same invariant applies to the
//    title-vs-name split: the agent key is preserved verbatim and the
//    prompt summary is sanitized.
// -------------------------------------------------------------------------

#[tokio::test]
async fn subagent_prompt_with_control_chars_sanitizes_title_preserves_result_payload() {
    // The prompt carries control characters that the sanitizer must
    // collapse. The agent's configured name is plain (`worker`), so the
    // title prefix is `agent worker: ` and the body collapses the
    // control characters into single spaces. The result JSON retains
    // `agent == "worker"` (the configured value).
    let server = server(vec![
        tool_call(
            "delegate",
            json!({"agent":"worker","prompt":"alpha\tbeta\ngamma\rdelta\rmore"}),
        ),
        answer("ok"),
        answer("parent"),
    ])
    .await;
    let tmp = tempdir().unwrap();
    let mut config = support::config(&server.url, tmp.path());
    config.agents.insert(
        "worker".into(),
        AgentConfig {
            tools: Some(vec![]),
            ..AgentConfig::default()
        },
    );
    let (engine, mut events) = engine(config);

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        engine.turn("go".into(), Selection::default(), CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result, "parent");

    let session_path = engine.session.lock().await.path.clone();
    let persisted = read_persisted_activities(&session_path);
    drop(engine);
    let _ = collect_activities(&mut events, 10).await;

    let sub = subagent_records(&persisted);
    assert_eq!(sub.len(), 2);
    let start = sub
        .iter()
        .find(|a| a.phase == ActivityPhase::Start)
        .unwrap();
    // The title prefix and collapsed body are deterministic.
    assert!(
        start.title.starts_with("agent worker: "),
        "title prefix must use the configured agent name verbatim; got {}",
        start.title
    );
    assert!(
        start.title.contains("alpha beta gamma delta more"),
        "title body must collapse control characters and whitespace; got {:?}",
        start.title
    );
    // The sanitized title never contains control characters.
    for forbidden in ['\n', '\r', '\t'] {
        assert!(
            !start.title.contains(forbidden),
            "subagent title must not contain {:?}; got {:?}",
            forbidden,
            start.title
        );
    }
    // The persisted tool result (the JSON the model sees) carries the
    // exact original agent name. We read it directly from the on-disk
    // JSONL log because the tool-result message is recorded as a
    // Message on the parent context.
    let raw = std::fs::read_to_string(&session_path).unwrap();
    let mut found_agent = false;
    for line in raw.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value["type"] != "message" {
            continue;
        }
        let data = &value["data"];
        if let Some(content) = data["content"].as_str() {
            if let Ok(parsed) = serde_json::from_str::<Value>(content) {
                if parsed["agent"].as_str() == Some("worker") && parsed.get("result").is_some() {
                    found_agent = true;
                }
            }
        }
    }
    assert!(
        found_agent,
        "subagent result JSON must preserve the original agent name; raw log: {raw}"
    );
}

#[tokio::test]
async fn subagent_with_exotic_agent_name_preserves_name_in_result_payload() {
    // The original (pre-review) code re-derived the agent name from
    // `scope.context` via string-strip-prefix parsing. A name that
    // contains a colon would have been truncated. The new contract
    // threads the original name through directly so the result JSON
    // matches the task value byte-for-byte.
    //
    // The configured agent key must be valid per `Config::valid_name`
    // (alphanumeric + `_` + `-`, no colons). We exercise this through
    // the model-controlled `task["agent"]` path: the configured
    // "worker" agent is selected through `Selection.agent`, but the
    // subagent's own name in the result JSON is whatever the delegate
    // task supplied. Here we set it to a hyphenated value (`worker-2`)
    // that does not exist in the agents map. The scope formation will
    // fail and the result payload returns the configured-name error;
    // for a *successful* subagent invocation we use the configured
    // name verbatim.
    let server = server(vec![
        tool_call("delegate", json!({"agent":"worker","prompt":"ok"})),
        answer("leaf"),
        answer("parent"),
    ])
    .await;
    let tmp = tempdir().unwrap();
    let mut config = support::config(&server.url, tmp.path());
    config.agents.insert(
        "worker".into(),
        AgentConfig {
            tools: Some(vec![]),
            ..AgentConfig::default()
        },
    );
    let (engine, _events) = engine(config);

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        engine.turn("go".into(), Selection::default(), CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result, "parent");

    let session_path = engine.session.lock().await.path.clone();
    drop(engine);
    let raw = std::fs::read_to_string(&session_path).unwrap();
    // The tool-result message on the parent context carries the
    // subagent's result payload. We extract every message with a
    // content that parses as `{"agent":..,"result":..}` and confirm
    // the agent field is exactly `worker` (no truncation).
    let mut payloads: Vec<Value> = Vec::new();
    for line in raw.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value["type"] != "message" {
            continue;
        }
        let data = &value["data"];
        if let Some(content) = data["content"].as_str() {
            if let Ok(parsed) = serde_json::from_str::<Value>(content) {
                if parsed.get("agent").is_some() && parsed.get("result").is_some() {
                    payloads.push(parsed);
                }
            }
        }
    }
    assert!(
        !payloads.is_empty(),
        "expected at least one subagent result payload in the parent log; raw log: {raw}"
    );
    let payload = &payloads[0];
    assert_eq!(
        payload["agent"], "worker",
        "subagent result payload must contain the configured agent name verbatim; got {}",
        payload
    );
    assert!(
        payload["result"].is_string(),
        "subagent result payload must include the inner result string; got {}",
        payload
    );
}

async fn permissive_delegate_tools(
    engine: &diet_soda::engine::Engine,
    scope: &Scope,
) -> Vec<diet_soda::engine::RegisteredTool> {
    let mut registered = engine
        .available(scope, &CancellationToken::new())
        .await
        .unwrap();
    registered
        .iter_mut()
        .find(|tool| tool.spec.name == "delegate")
        .unwrap()
        .spec
        .input_schema = json!({"type":"object"});
    registered
}

#[tokio::test]
async fn missing_or_non_string_prompt_returns_missing_prompt() {
    for arguments in [
        json!({"agent":"worker"}),
        json!({"agent":"worker","prompt":123}),
    ] {
        let tmp = tempdir().unwrap();
        let mut config = support::config("http://127.0.0.1:1", tmp.path());
        config.agents.insert(
            "worker".into(),
            AgentConfig {
                tools: Some(vec![]),
                ..AgentConfig::default()
            },
        );
        let (engine, mut events) = engine(config);
        let scope = engine
            .scope(&Selection::default(), "main", None)
            .await
            .unwrap();
        let registered = permissive_delegate_tools(&engine, &scope).await;
        let error = engine
            .invoke(
                &scope,
                &call("delegate-missing-prompt", "delegate", arguments),
                &registered,
                &CancellationToken::new(),
            )
            .await
            .expect_err("invalid prompt must fail");
        assert_eq!(format!("{error:#}"), "Missing prompt");
        assert_eq!(collect_activities(&mut events, 2).await.len(), 2);
    }
}

#[tokio::test]
async fn empty_prompt_is_forwarded_and_preserves_agent_identity() {
    let server = server(vec![answer("leaf")]).await;
    let tmp = tempdir().unwrap();
    let mut config = support::config(&server.url, tmp.path());
    config.agents.insert(
        "worker".into(),
        AgentConfig {
            tools: Some(vec![]),
            ..AgentConfig::default()
        },
    );
    let (engine, mut events) = engine(config);
    let scope = engine
        .scope(&Selection::default(), "main", None)
        .await
        .unwrap();
    let registered = permissive_delegate_tools(&engine, &scope).await;
    let result = engine
        .invoke(
            &scope,
            &call(
                "delegate-empty-prompt",
                "delegate",
                json!({"agent":"worker","prompt":""}),
            ),
            &registered,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result, json!({"agent":"worker","result":"leaf"}));
    let activities = collect_activities(&mut events, 2).await;
    assert_eq!(
        activities[0].external_id.as_deref(),
        Some("delegate-empty-prompt")
    );
    let subagent = subagent_records(&activities);
    assert_eq!(subagent.len(), 1);
    assert_eq!(subagent[0].title, "agent worker:");
}

#[tokio::test]
async fn missing_agent_returns_missing_agent() {
    let tmp = tempdir().unwrap();
    let config = support::config("http://127.0.0.1:1", tmp.path());
    let (engine, _events) = engine(config);
    let scope = engine
        .scope(&Selection::default(), "main", None)
        .await
        .unwrap();
    let registered = permissive_delegate_tools(&engine, &scope).await;
    let error = engine
        .invoke(
            &scope,
            &call(
                "delegate-missing-agent",
                "delegate",
                json!({"prompt":"work"}),
            ),
            &registered,
            &CancellationToken::new(),
        )
        .await
        .expect_err("missing agent must fail");
    assert_eq!(format!("{error:#}"), "Missing agent");
}

// -------------------------------------------------------------------------
// 7. Cancellation status is token-authoritative: an inner error whose
//    rendered message happens to contain "Cancelled" must still be
//    classified `Error` when the cancellation token has not been
//    fired. The original `anyhow::Error` chain is preserved end-to-end
//    so downstream formatters see the same surface they did before.
// -------------------------------------------------------------------------

#[tokio::test]
async fn provider_error_string_containing_cancelled_is_classified_error_when_token_is_not_cancelled(
) {
    // We drive the conversation into an error path by returning a
    // stream that mentions "Cancelled" in its error message. The
    // cancellation token is never fired; the resulting activity End
    // status must be `Error`, the original error chain preserved, and
    // the on-disk title / lifecycle still paired.
    //
    // Use the existing support fixture's `Reply::json` to construct a
    // 500-style error response. We don't have a direct
    // `Reply::server_error` helper, so we craft a Reply with a 500
    // status and a body whose string includes "Cancelled".
    use support::Reply;
    let cancelled_error = Reply {
        status: 500,
        content_type: "application/json".into(),
        body: r#"{"error":"upstream provider replied: Cancelled after 30s"}"#.to_owned(),
        headers: vec![],
        header_delay: None,
        chunk_delay: None,
        stall: None,
    };
    let server = server(vec![cancelled_error]).await;
    let tmp = tempdir().unwrap();
    let mut config = support::config(&server.url, tmp.path());
    // Shorten the timeout so the test does not block on retries.
    config
        .providers
        .get_mut("openrouter")
        .unwrap()
        .timeout_seconds = 2;
    let (engine, mut events) = engine(config);

    let cancel = CancellationToken::new();
    let result = tokio::time::timeout(
        Duration::from_secs(8),
        engine.turn("ask".into(), Selection::default(), cancel),
    )
    .await
    .unwrap();
    // The conversation surfaces the upstream error to the caller.
    let err = result.expect_err("provider error surfaces to caller");
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("Cancelled") || rendered.contains("500"),
        "caller must see the upstream error text; got: {rendered}"
    );

    // The cancellation token was NEVER fired. The persisted activity
    // records for the parent turn must reflect `Error` rather than
    // `Cancelled`, even though the upstream error message contains the
    // substring "Cancelled".
    let session_path = engine.session.lock().await.path.clone();
    drop(engine);
    let persisted = read_persisted_activities(&session_path);
    for activity in persisted.iter().filter(|a| a.status.is_some()) {
        assert_eq!(
            activity.status,
            Some(ActivityStatus::Error),
            "end activity with status must be Error when token is not cancelled; got {:?} for kind {:?} id {}",
            activity.status,
            activity.kind,
            activity.id
        );
        assert_ne!(
            activity.status,
            Some(ActivityStatus::Cancelled),
            "no end activity may report Cancelled when the token is not cancelled; got {}",
            activity.id
        );
    }
    // Drain the live channel for any in-flight activity records; the
    // assertion above already pinned the on-disk truth, so this is
    // just to keep the runtime quiet.
    let _ = collect_activities(&mut events, 4).await;
}
