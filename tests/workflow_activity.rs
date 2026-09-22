//! Wave 2 WorkflowStep activity lifecycle tests. Mirrors the reviewed
//! tool and subagent lifecycles in `src/engine/dispatch.rs` and
//! `Engine::emit_activity` in `src/engine/mod.rs`. Every workflow step
//! attempt inside the attempt loop must emit a paired `WorkflowStep`
//! Start/End activity record with:
//!   * a freshly-allocated UUID per attempt,
//!   * `parent_id == None`,
//!   * `context` exactly `workflow:<run_id>:<step>:<attempt>`,
//!   * `external_id == run_id`,
//!   * a sanitized, bounded title of the form
//!     `workflow <title> step <1-based index>: <step prompt summary>`,
//!   * exactly one matching `End` whose status reflects the conversation
//!     outcome (Success / Cancelled / Error), persisted before any
//!     post-step HITL gate or retry/skip advance.
//!
//! The tests stay local, mock the provider through the existing
//! `tests/support` server fixtures, and use bounded waits so a missing
//! event cannot hang the suite.
#![allow(clippy::needless_raw_string_hashes)]

mod support;
use diet_soda::{
    engine::Selection,
    model::{ActivityEvent, ActivityKind, ActivityPhase, ActivityStatus, Decision, UiEvent},
    workflow::{self, Step, Workflow},
};
use serde_json::{json, Value};
use std::{collections::HashSet, sync::atomic::Ordering, time::Duration};
use support::{answer, config, engine, server, tool_call};
use tempfile::tempdir;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Drain the UiEvent channel until `count` workflow-step activity records
/// have been observed (any other events are skipped). The bounded timeout
/// keeps a missing event from hanging the suite.
async fn collect_step_activities(
    rx: &mut mpsc::UnboundedReceiver<UiEvent>,
    count: usize,
) -> Vec<ActivityEvent> {
    let mut out = Vec::new();
    let deadline = Duration::from_secs(5);
    while out.len() < count {
        match tokio::time::timeout(deadline, rx.recv()).await {
            Ok(Some(UiEvent::Activity(a))) if a.kind == ActivityKind::WorkflowStep => out.push(a),
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    out
}

/// Read all workflow-step activity records (Start and End) from the
/// on-disk session log.
fn read_persisted_step_activities(path: &std::path::Path) -> Vec<ActivityEvent> {
    let raw = std::fs::read_to_string(path).unwrap();
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["type"] == "activity")
        .map(|line| serde_json::from_value::<ActivityEvent>(line["data"].clone()).unwrap())
        .filter(|a| a.kind == ActivityKind::WorkflowStep)
        .collect()
}

/// Read all approval events (any kind) from the on-disk session log so we
/// can correlate the persisted `activity_id` field with the originating
/// step lifecycle.
fn read_persisted_approvals(path: &std::path::Path) -> Vec<Value> {
    let raw = std::fs::read_to_string(path).unwrap();
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["type"] == "approval")
        .collect()
}

/// Drive the live event channel until the next `UiEvent::Approval` arrives,
/// then send the supplied decision. Activity events that arrive first are
/// drained silently. The helper bounds the wait so a missing approval
/// cannot hang the suite.
async fn send_next_approval(rx: &mut mpsc::UnboundedReceiver<UiEvent>, decision: Decision) {
    let deadline = Duration::from_secs(5);
    loop {
        match tokio::time::timeout(deadline, rx.recv()).await {
            Ok(Some(UiEvent::Approval { reply, .. })) => {
                reply.send(decision).unwrap();
                return;
            }
            Ok(Some(_)) => {}
            _ => panic!("timed out waiting for an approval event"),
        }
    }
}

fn simple_workflow(title: &str, steps: Vec<(&str, bool)>) -> Workflow {
    Workflow {
        title: title.into(),
        author: "test".into(),
        steps: steps
            .into_iter()
            .map(|(prompt, hitl)| Step {
                agent: None,
                model: "openai/gpt-4.1-mini".into(),
                prompt: prompt.into(),
                mcps: vec![],
                hitl,
            })
            .collect(),
    }
}

// -------------------------------------------------------------------------
// 1. Two-step success produces two paired Start/End records and only
//    the first step's post-step gate is presented (no gate after the
//    final step). Each pair gets its own fresh id and context.
// -------------------------------------------------------------------------

#[tokio::test]
async fn two_step_success_emits_two_pairs_and_only_first_post_step_gate() {
    let mut server = server(vec![answer("first result"), answer("final result")]).await;
    let tmp = tempdir().unwrap();
    let (engine, mut events) = engine(config(&server.url, tmp.path()));
    let session_path = engine.session.lock().await.path.clone();
    let workflow = simple_workflow(
        "pair-test",
        vec![
            ("first {{input}}", true),
            ("second {{previous_result}}", true),
        ],
    );
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
    // Drive the first approval so the workflow advances. There must be
    // exactly one approval event for a two-step workflow with the final
    // step's hitl disabled by spec (no gate on final).
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
            reply.send(Decision::Approve).unwrap();
            break;
        }
    }
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result, "final result");
    // No further approval may arrive after the workflow finishes; the
    // final step never gates.
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, UiEvent::Approval { .. }));
    }
    // Live UiEvents may be drained by the approval driver above. The
    // on-disk log is authoritative for the activity lifecycle, so we
    // do not assert a specific live-event count here.
    let _live = collect_step_activities(&mut events, 4).await;
    let persisted = read_persisted_step_activities(&session_path);
    // The persisted log is authoritative.
    assert_eq!(
        persisted.len(),
        4,
        "two steps × (Start + End) = 4 WorkflowStep records"
    );
    let starts: Vec<&ActivityEvent> = persisted
        .iter()
        .filter(|a| a.phase == ActivityPhase::Start)
        .collect();
    let ends: Vec<&ActivityEvent> = persisted
        .iter()
        .filter(|a| a.phase == ActivityPhase::End)
        .collect();
    assert_eq!(starts.len(), 2);
    assert_eq!(ends.len(), 2);
    // Pairing: each Start's id matches exactly one End's id and vice
    // versa. The pairs are not interleaved: Start₁, End₁, Start₂, End₂.
    for (start, end) in starts.iter().zip(ends.iter()) {
        assert_eq!(start.id, end.id, "Start and End must share an id");
        assert_eq!(start.context, end.context);
        assert_eq!(start.title, end.title);
        assert_eq!(start.external_id, end.external_id);
        assert_eq!(start.parent_id, None);
        assert_eq!(end.parent_id, None);
        assert_eq!(end.status, Some(ActivityStatus::Success));
    }
    // The two pairs are distinct: no shared id, distinct contexts.
    let pair_ids: HashSet<&str> = starts.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(pair_ids.len(), 2, "two pairs must have distinct ids");
    let contexts: HashSet<&str> = persisted.iter().map(|a| a.context.as_str()).collect();
    assert_eq!(contexts.len(), 2, "two pairs must have distinct contexts");
    // Each context encodes the 1-based step index and the attempt number
    // (attempt 1 for both successful first-tries).
    let mut contexts_sorted: Vec<&str> = contexts.into_iter().collect();
    contexts_sorted.sort();
    assert!(contexts_sorted[0].ends_with(":1:1"));
    assert!(contexts_sorted[1].ends_with(":2:1"));
    // The order on disk is Start₁, End₁, Start₂, End₂: each End arrives
    // before the next step's Start, which is the explicit ordering
    // documented in the producer (execute → emit End → advance). We
    // pin this here so a future refactor cannot silently reorder them.
    assert_eq!(persisted[0].phase, ActivityPhase::Start);
    assert_eq!(persisted[1].phase, ActivityPhase::End);
    assert_eq!(persisted[2].phase, ActivityPhase::Start);
    assert_eq!(persisted[3].phase, ActivityPhase::End);
    assert_eq!(persisted[0].id, persisted[1].id);
    assert_eq!(persisted[2].id, persisted[3].id);
    assert_ne!(persisted[0].id, persisted[2].id);
    // The shared external_id is the workflow run_id, so every record
    // for a single run can be correlated.
    let external_ids: HashSet<&str> = persisted
        .iter()
        .filter_map(|a| a.external_id.as_deref())
        .collect();
    assert_eq!(external_ids.len(), 1, "all records share one run_id");
    // Live UiEvents may be drained by the approval driver above. The
    // on-disk log is authoritative for the activity lifecycle, so we
    // do not assert a specific live-event count here.
    let _live = collect_step_activities(&mut events, 4).await;
    // The persisted approval must carry the first step's activity id.
    let approvals = read_persisted_approvals(&session_path);
    let step_approvals: Vec<&Value> = approvals
        .iter()
        .filter(|v| {
            v["context"]
                .as_str()
                .map(|c| c.starts_with("workflow:"))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(
        step_approvals.len(),
        1,
        "exactly one step-level approval for a two-step success"
    );
    let approval_id = step_approvals[0]["data"]["activity_id"].as_str().unwrap();
    assert_eq!(
        approval_id, starts[0].id,
        "post-step approval activity_id must equal the first step's id"
    );
    // Final step had no gate, so the second step's id does not appear
    // in any persisted approval event.
    assert!(approvals
        .iter()
        .all(|v| v["data"]["activity_id"].as_str() != Some(starts[1].id.as_str())));
    // Drain the requests the engine made so the server fixture stays
    // happy.
    server.requests.recv().await.unwrap();
    let _ = server.requests.recv().await;
    assert!(server.count.load(Ordering::SeqCst) >= 2);
}

// -------------------------------------------------------------------------
// 2. A successful step that is retried yields two distinct attempt
//    pairs (different ids, different contexts). The first pair closes
//    before the second pair's Start is persisted.
// -------------------------------------------------------------------------

#[tokio::test]
async fn successful_step_retry_yields_distinct_attempt_ids_and_contexts() {
    // Two-step workflow: step 1 hitl=true, step 2 (final) hitl=false.
    // After step 1's first success the gate fires; we send Retry so the
    // engine re-runs step 1 (attempt 2). Step 1's second success gates
    // again; we send Approve so step 2 runs. Step 2 has no gate (final).
    let mut server = server(vec![
        answer("first attempt"),
        answer("retry attempt"),
        answer("final"),
    ])
    .await;
    let tmp = tempdir().unwrap();
    let (engine, mut events) = engine(config(&server.url, tmp.path()));
    let session_path = engine.session.lock().await.path.clone();
    let workflow = simple_workflow(
        "retry-success",
        vec![("first {{input}}", true), ("second", false)],
    );
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
    // Two approvals: first Retry, then Approve.
    send_next_approval(&mut events, Decision::Retry).await;
    send_next_approval(&mut events, Decision::Approve).await;
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result, "final");
    // 3 attempts total: step 1 attempt 1, step 1 attempt 2, step 2
    // attempt 1. Each attempt is its own paired lifecycle.
    let persisted = read_persisted_step_activities(&session_path);
    assert_eq!(
        persisted.len(),
        6,
        "three attempts × (Start + End) = 6 WorkflowStep records; got {}",
        persisted.len()
    );
    let starts: Vec<&ActivityEvent> = persisted
        .iter()
        .filter(|a| a.phase == ActivityPhase::Start)
        .collect();
    let ends: Vec<&ActivityEvent> = persisted
        .iter()
        .filter(|a| a.phase == ActivityPhase::End)
        .collect();
    assert_eq!(starts.len(), 3);
    assert_eq!(ends.len(), 3);
    // Each pair is unique by id and globally unique across the run.
    let pair_ids: HashSet<&str> = starts.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(pair_ids.len(), 3, "three attempts must have distinct ids");
    // Contexts encode the step index and attempt number. The first two
    // pairs belong to step 1 (attempts 1 and 2); the third belongs to
    // step 2 attempt 1.
    let step1_attempt1_starts: Vec<&&ActivityEvent> = starts
        .iter()
        .filter(|a| a.context.ends_with(":1:1"))
        .collect();
    let step1_attempt2_starts: Vec<&&ActivityEvent> = starts
        .iter()
        .filter(|a| a.context.ends_with(":1:2"))
        .collect();
    let step2_attempt1_starts: Vec<&&ActivityEvent> = starts
        .iter()
        .filter(|a| a.context.ends_with(":2:1"))
        .collect();
    assert_eq!(step1_attempt1_starts.len(), 1);
    assert_eq!(step1_attempt2_starts.len(), 1);
    assert_eq!(step2_attempt1_starts.len(), 1);
    // The contexts within each pair are identical.
    assert_eq!(starts[0].context, ends[0].context);
    assert_eq!(starts[1].context, ends[1].context);
    assert_eq!(starts[2].context, ends[2].context);
    // End statuses are all Success because every attempt succeeded.
    for end in &ends {
        assert_eq!(end.status, Some(ActivityStatus::Success));
    }
    // On-disk order: each End appears before the next step's Start.
    // The first End is the first attempt's close; the second End is the
    // second attempt's close; the third Start is step 2's first attempt.
    let step1_attempt1_end_index = persisted
        .iter()
        .position(|a| a.context.ends_with(":1:1") && a.phase == ActivityPhase::End)
        .unwrap();
    let step1_attempt2_start_index = persisted
        .iter()
        .position(|a| a.context.ends_with(":1:2") && a.phase == ActivityPhase::Start)
        .unwrap();
    let step1_attempt2_end_index = persisted
        .iter()
        .position(|a| a.context.ends_with(":1:2") && a.phase == ActivityPhase::End)
        .unwrap();
    let step2_attempt1_start_index = persisted
        .iter()
        .position(|a| a.context.ends_with(":2:1") && a.phase == ActivityPhase::Start)
        .unwrap();
    assert!(step1_attempt1_end_index < step1_attempt2_start_index);
    assert!(step1_attempt2_end_index < step2_attempt1_start_index);
    // The persisted approvals must carry the activity id of the
    // originating attempt (not the workflow id). Two approvals total:
    // one for each step-1 attempt. Step 2 never gates so its id never
    // appears in the approval log.
    let approvals = read_persisted_approvals(&session_path);
    let step_approvals: Vec<&Value> = approvals
        .iter()
        .filter(|v| {
            v["context"]
                .as_str()
                .map(|c| c.starts_with("workflow:"))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(step_approvals.len(), 2);
    let first_attempt_id = step1_attempt1_starts[0].id.clone();
    let second_attempt_id = step1_attempt2_starts[0].id.clone();
    let final_step_id = step2_attempt1_starts[0].id.clone();
    assert_eq!(
        step_approvals[0]["data"]["activity_id"].as_str().unwrap(),
        first_attempt_id
    );
    assert_eq!(
        step_approvals[1]["data"]["activity_id"].as_str().unwrap(),
        second_attempt_id
    );
    assert_ne!(first_attempt_id, second_attempt_id);
    assert!(approvals
        .iter()
        .all(|v| v["data"]["activity_id"].as_str() != Some(final_step_id.as_str())));
    server.requests.recv().await.unwrap();
    let _ = server.requests.recv().await;
    let _ = server.requests.recv().await;
}

// -------------------------------------------------------------------------
// 3. A failed step that is retried yields a Success-or-Error first pair
//    (Status::Error here, because the conversation errored) and a fresh
//    second pair (its own id and context). The persisted Approval for
//    the error path carries the errored attempt's activity_id; the
//    retry's approval carries the second attempt's activity_id.
// -------------------------------------------------------------------------

#[tokio::test]
async fn failed_step_retry_yields_error_then_new_pair_with_distinct_id_and_context() {
    // Drive the conversation into an error path by configuring the
    // provider to return a stream that times out before producing a
    // finish_reason. The engine surfaces this as an IncompleteStreamError
    // so the conversation returns Err.
    use support::Reply;
    let stalled = Reply {
        status: 200,
        content_type: "text/event-stream".into(),
        body: format!(
            "data: {}\r\n\r\n",
            json!({"choices": [{"delta": {"content": "partial"}}]})
        ),
        headers: vec![],
        header_delay: None,
        chunk_delay: None,
        stall: Some(Duration::from_secs(3)),
    };
    // Two-step workflow. Step 1 hitl=true, step 2 hitl=false (final).
    // Step 1's first attempt errors → gate fires → Retry. Step 1's
    // second attempt succeeds → gate fires → Approve. Step 2 runs
    // without a gate.
    let mut server = server(vec![stalled, answer("retry answer"), answer("final")]).await;
    let tmp = tempdir().unwrap();
    let mut cfg = config(&server.url, tmp.path());
    cfg.providers.get_mut("openrouter").unwrap().timeout_seconds = 1;
    let (engine, mut events) = engine(cfg);
    let session_path = engine.session.lock().await.path.clone();
    let workflow = simple_workflow(
        "retry-error",
        vec![("first {{input}}", true), ("second", false)],
    );
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
    send_next_approval(&mut events, Decision::Retry).await;
    send_next_approval(&mut events, Decision::Approve).await;
    let result = tokio::time::timeout(Duration::from_secs(8), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result, "final");
    let persisted = read_persisted_step_activities(&session_path);
    // 3 attempts total: step 1 attempt 1 (Error), step 1 attempt 2
    // (Success), step 2 attempt 1 (Success). 6 WorkflowStep records.
    assert_eq!(persisted.len(), 6, "three attempts × Start + End");
    let starts: Vec<&ActivityEvent> = persisted
        .iter()
        .filter(|a| a.phase == ActivityPhase::Start)
        .collect();
    let ends: Vec<&ActivityEvent> = persisted
        .iter()
        .filter(|a| a.phase == ActivityPhase::End)
        .collect();
    assert_eq!(starts.len(), 3);
    assert_eq!(ends.len(), 3);
    let pair_ids: HashSet<&str> = starts.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(pair_ids.len(), 3, "every attempt must allocate a fresh id");
    // The Error status belongs to the first attempt; identify it by
    // context ending in :1:1 (since step 1's first attempt = :1:1).
    let first_attempt_end: Vec<&ActivityEvent> = ends
        .iter()
        .copied()
        .filter(|e| e.context.ends_with(":1:1"))
        .collect();
    let second_attempt_end: Vec<&ActivityEvent> = ends
        .iter()
        .copied()
        .filter(|e| e.context.ends_with(":1:2"))
        .collect();
    assert_eq!(first_attempt_end.len(), 1);
    assert_eq!(second_attempt_end.len(), 1);
    assert_eq!(
        first_attempt_end[0].status,
        Some(ActivityStatus::Error),
        "first attempt errored"
    );
    assert_eq!(
        second_attempt_end[0].status,
        Some(ActivityStatus::Success),
        "retry attempt succeeded"
    );
    // The error-prompt approval carries the failed attempt's id, the
    // retry-success approval carries the second attempt's id. Step 2's
    // final attempt never gates, so its id does not appear in any
    // persisted approval event.
    let approvals = read_persisted_approvals(&session_path);
    let step_approvals: Vec<&Value> = approvals
        .iter()
        .filter(|v| {
            v["context"]
                .as_str()
                .map(|c| c.starts_with("workflow:"))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(step_approvals.len(), 2);
    let first_attempt_id = starts
        .iter()
        .find(|a| a.context.ends_with(":1:1"))
        .unwrap()
        .id
        .clone();
    let second_attempt_id = starts
        .iter()
        .find(|a| a.context.ends_with(":1:2"))
        .unwrap()
        .id
        .clone();
    let final_step_id = starts
        .iter()
        .find(|a| a.context.ends_with(":2:1"))
        .unwrap()
        .id
        .clone();
    assert_eq!(
        step_approvals[0]["data"]["activity_id"].as_str().unwrap(),
        first_attempt_id
    );
    assert_eq!(
        step_approvals[1]["data"]["activity_id"].as_str().unwrap(),
        second_attempt_id
    );
    assert_ne!(first_attempt_id, second_attempt_id);
    assert!(approvals
        .iter()
        .all(|v| v["data"]["activity_id"].as_str() != Some(final_step_id.as_str())));
    let _ = server.requests.recv().await;
    let _ = server.requests.recv().await;
    let _ = server.requests.recv().await;
}

// -------------------------------------------------------------------------
// 4. Tool activities executed inside a step's conversation parent to
//    the step's activity id. This pins the spec ordering: set
//    `scope.activity_id` to the step id before `engine.conversation` so
//    nested tool lifecycles correlate to the originating step.
// -------------------------------------------------------------------------

#[tokio::test]
async fn tool_activities_inside_a_step_parent_to_the_step_activity_id() {
    let mut server = server(vec![
        tool_call("web_fetch", json!({"url": "https://example.test"})),
        answer("done"),
    ])
    .await;
    let tmp = tempdir().unwrap();
    let (engine, mut events) = engine(config(&server.url, tmp.path()));
    let session_path = engine.session.lock().await.path.clone();
    let workflow = simple_workflow("tool-parent", vec![("step", false)]);
    let runner = engine.clone();
    let task = tokio::spawn(async move {
        workflow::run(
            &runner,
            workflow,
            "".into(),
            Selection::default(),
            CancellationToken::new(),
        )
        .await
    });
    // Drain a generous pool of events: 2 tool (Start + End), 2 step
    // (Start + End), possibly 0 approvals.
    let mut all_activity = Vec::new();
    let deadline = Duration::from_secs(5);
    while all_activity.len() < 4 {
        match tokio::time::timeout(deadline, events.recv()).await {
            Ok(Some(UiEvent::Activity(a))) => all_activity.push(a),
            Ok(Some(UiEvent::Approval { reply, .. })) => {
                reply.send(Decision::Approve).unwrap();
            }
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result, "done");
    let persisted = read_persisted_step_activities(&session_path);
    assert_eq!(persisted.len(), 2, "one step × Start + End");
    let step_id = persisted
        .iter()
        .find(|a| a.phase == ActivityPhase::Start)
        .unwrap()
        .id
        .clone();
    // Read all activity records (any kind) from the on-disk log so we
    // can find the tool lifecycle and assert its parent_id.
    let raw = std::fs::read_to_string(&session_path).unwrap();
    let tool_activities: Vec<ActivityEvent> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["type"] == "activity")
        .map(|v| serde_json::from_value::<ActivityEvent>(v["data"].clone()).unwrap())
        .filter(|a| a.kind == ActivityKind::Tool)
        .collect();
    assert_eq!(
        tool_activities.len(),
        2,
        "tool lifecycle must produce Start + End"
    );
    for tool in &tool_activities {
        assert_eq!(
            tool.parent_id.as_deref(),
            Some(step_id.as_str()),
            "tool activity parent_id must equal the step's id"
        );
    }
    let _ = server.requests.recv().await;
    let _ = server.requests.recv().await;
}

// -------------------------------------------------------------------------
// 5. The persisted approval event for the post-step HITL gate carries
//    the step's activity_id, not the workflow id or a stale value.
// -------------------------------------------------------------------------

#[tokio::test]
async fn post_step_approval_records_step_activity_id() {
    let mut server = server(vec![answer("first"), answer("final")]).await;
    let tmp = tempdir().unwrap();
    let (engine, mut events) = engine(config(&server.url, tmp.path()));
    let session_path = engine.session.lock().await.path.clone();
    // Two-step workflow so the first step's post-step HITL gate fires.
    let workflow = simple_workflow(
        "approval-corr",
        vec![("first {{input}}", true), ("second", false)],
    );
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
    send_next_approval(&mut events, Decision::Approve).await;
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result, "final");
    let persisted = read_persisted_step_activities(&session_path);
    // Two pairs: step 1 + step 2 (both single attempts).
    assert_eq!(persisted.len(), 4);
    let step1_id = persisted
        .iter()
        .find(|a| a.phase == ActivityPhase::Start && a.context.ends_with(":1:1"))
        .unwrap()
        .id
        .clone();
    let run_id = persisted[0].external_id.clone().unwrap();
    let approvals = read_persisted_approvals(&session_path);
    let step_approvals: Vec<&Value> = approvals
        .iter()
        .filter(|v| {
            v["context"]
                .as_str()
                .map(|c| c.starts_with("workflow:"))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(step_approvals.len(), 1);
    let approval_id = step_approvals[0]["data"]["activity_id"].as_str().unwrap();
    assert_eq!(approval_id, step1_id);
    // The approval id must not be the workflow run_id; the correlation
    // is to the step lifecycle, not the outer run.
    assert_ne!(approval_id, run_id);
    let _ = server.requests.recv().await;
    let _ = server.requests.recv().await;
}

// -------------------------------------------------------------------------
// 6. Final step (last in workflow.steps) produces no approval event
//    even when hitl=true. The End lifecycle still persists.
// -------------------------------------------------------------------------

#[tokio::test]
async fn final_step_produces_no_approval_even_when_hitl_is_true() {
    let mut server = server(vec![answer("a"), answer("final")]).await;
    let tmp = tempdir().unwrap();
    let (engine, mut events) = engine(config(&server.url, tmp.path()));
    let session_path = engine.session.lock().await.path.clone();
    let workflow = simple_workflow("no-final-gate", vec![("first", true), ("final", true)]);
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
    send_next_approval(&mut events, Decision::Approve).await;
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result, "final");
    let persisted = read_persisted_step_activities(&session_path);
    // 2 steps × (Start + End) = 4 records, all Ends are Success.
    assert_eq!(persisted.len(), 4);
    for a in persisted.iter().filter(|a| a.phase == ActivityPhase::End) {
        assert_eq!(a.status, Some(ActivityStatus::Success));
    }
    let approvals = read_persisted_approvals(&session_path);
    let step_approvals: Vec<&Value> = approvals
        .iter()
        .filter(|v| {
            v["context"]
                .as_str()
                .map(|c| c.starts_with("workflow:"))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(
        step_approvals.len(),
        1,
        "only the first step gates; final step never does"
    );
    // The single persisted approval must correlate with the first
    // step's id, not the final step's id.
    let first_step_id = persisted
        .iter()
        .find(|a| a.phase == ActivityPhase::Start && a.context.ends_with(":1:1"))
        .unwrap()
        .id
        .clone();
    let final_step_id = persisted
        .iter()
        .find(|a| a.phase == ActivityPhase::Start && a.context.ends_with(":2:1"))
        .unwrap()
        .id
        .clone();
    assert_eq!(
        step_approvals[0]["data"]["activity_id"].as_str().unwrap(),
        first_step_id
    );
    assert!(approvals
        .iter()
        .all(|v| v["data"]["activity_id"].as_str() != Some(final_step_id.as_str())));
    let _ = server.requests.recv().await;
    let _ = server.requests.recv().await;
}

// -------------------------------------------------------------------------
// 7. Title helper invariants: control chars become spaces, whitespace
//    collapses, length is capped at 160 chars (Unicode scalars). The
//    title prefix encodes the workflow title and the 1-based step index.
//    We exercise the helper through the public workflow run path so we
//    don't need to expose private internals to the integration suite.
// -------------------------------------------------------------------------

#[tokio::test]
async fn step_titles_sanitize_collapse_and_cap_at_160_chars() {
    // 4 step prompt shapes: normal, control-char heavy, long ASCII
    // requiring truncation, and multi-byte UTF-8 (🍞 repeated). The
    // persisted Start titles must match the documented shape and stay
    // within 160 Unicode scalars.
    let mut server = server(vec![
        answer("normal"),
        answer("control"),
        answer("long"),
        answer("utf8"),
    ])
    .await;
    let tmp = tempdir().unwrap();
    let (engine, _events) = engine(config(&server.url, tmp.path()));
    let session_path = engine.session.lock().await.path.clone();
    let long_prompt = "x".repeat(500);
    let utf8_prompt = "🍞".repeat(500);
    let workflow = Workflow {
        title: "plan".into(),
        author: "test".into(),
        steps: vec![
            Step {
                agent: None,
                model: "openai/gpt-4.1-mini".into(),
                prompt: "draft the plan".into(),
                mcps: vec![],
                hitl: false,
            },
            Step {
                agent: None,
                model: "openai/gpt-4.1-mini".into(),
                prompt: "draft\nthe\tplan\rwith\rinternal\twhitespace".into(),
                mcps: vec![],
                hitl: false,
            },
            Step {
                agent: None,
                model: "openai/gpt-4.1-mini".into(),
                prompt: long_prompt,
                mcps: vec![],
                hitl: false,
            },
            Step {
                agent: None,
                model: "openai/gpt-4.1-mini".into(),
                prompt: utf8_prompt,
                mcps: vec![],
                hitl: false,
            },
        ],
    };
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        workflow::run(
            &engine,
            workflow,
            "".into(),
            Selection::default(),
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result, "utf8");
    let persisted = read_persisted_step_activities(&session_path);
    let starts: Vec<&ActivityEvent> = persisted
        .iter()
        .filter(|a| a.phase == ActivityPhase::Start)
        .collect();
    assert_eq!(starts.len(), 4);
    let titles: Vec<&str> = starts.iter().map(|a| a.title.as_str()).collect();
    // Normal step: no truncation, no whitespace collapse.
    assert_eq!(titles[0], "workflow plan step 1: draft the plan");
    // Control chars: collapse to a single space and trim.
    assert_eq!(
        titles[1],
        "workflow plan step 2: draft the plan with internal whitespace"
    );
    // Long ASCII: capped at 160 chars with an ellipsis.
    assert!(
        titles[2].chars().count() <= 160,
        "long ASCII title must be capped at 160 chars; got {}",
        titles[2].chars().count()
    );
    assert!(titles[2].ends_with('…'));
    assert!(titles[2].starts_with("workflow plan step 3: "));
    // Multi-byte UTF-8: scalar count stays ≤ 160.
    assert!(
        titles[3].chars().count() <= 160,
        "UTF-8 title must be capped at 160 scalars; got {}",
        titles[3].chars().count()
    );
    assert!(titles[3].ends_with('…'));
    assert!(titles[3].starts_with("workflow plan step 4: "));
    let _ = server.requests.recv().await;
    let _ = server.requests.recv().await;
    let _ = server.requests.recv().await;
    let _ = server.requests.recv().await;
}

// -------------------------------------------------------------------------
// 8. Pair/unique-id invariants: across a multi-step run, every
//    WorkflowStep Start id is globally unique and the on-disk trail is
//    composed entirely of Start+End pairs (no orphan Start, no orphan
//    End). Final step and earlier step ids never collide even with
//    retries.
// -------------------------------------------------------------------------

#[tokio::test]
async fn all_step_activity_ids_are_globally_unique_across_multi_step_run() {
    // A two-step workflow where the first step retries once. That gives
    // us 3 Start records (id 1: pair 1, id 2: pair 2, id 3: step 2
    // pair) and 3 End records. The set of ids across Start and End must
    // be the same size as the number of distinct ids, and every Start
    // must have a matching End.
    let mut server = server(vec![
        answer("first"),
        answer("first retry"),
        answer("final"),
    ])
    .await;
    let tmp = tempdir().unwrap();
    let (engine, mut events) = engine(config(&server.url, tmp.path()));
    let session_path = engine.session.lock().await.path.clone();
    let workflow = simple_workflow(
        "unique-ids",
        vec![("first {{input}}", true), ("second", false)],
    );
    let task = tokio::spawn(async move {
        workflow::run(
            &engine,
            workflow,
            "x".into(),
            Selection::default(),
            CancellationToken::new(),
        )
        .await
    });
    send_next_approval(&mut events, Decision::Retry).await;
    send_next_approval(&mut events, Decision::Approve).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let persisted = read_persisted_step_activities(&session_path);
    // First step × 2 attempts = 2 pairs (Start + End); second step = 1
    // pair; total 6 records.
    assert_eq!(persisted.len(), 6);
    let starts: Vec<&ActivityEvent> = persisted
        .iter()
        .filter(|a| a.phase == ActivityPhase::Start)
        .collect();
    let ends: Vec<&ActivityEvent> = persisted
        .iter()
        .filter(|a| a.phase == ActivityPhase::End)
        .collect();
    assert_eq!(starts.len(), 3);
    assert_eq!(ends.len(), 3);
    let ids: Vec<&str> = persisted.iter().map(|a| a.id.as_str()).collect();
    let unique: HashSet<&str> = ids.iter().copied().collect();
    assert_eq!(
        unique.len(),
        3,
        "every WorkflowStep id must be globally unique across the run"
    );
    // Each Start has exactly one matching End.
    for start in &starts {
        let matching_ends: Vec<&&ActivityEvent> =
            ends.iter().filter(|e| e.id == start.id).collect();
        assert_eq!(
            matching_ends.len(),
            1,
            "Start {} must have exactly one matching End",
            start.id
        );
    }
    // Pair ids and external_id (workflow run_id) — the run_id is shared
    // across all records, but every record's external_id equals it.
    let external_ids: HashSet<&str> = persisted
        .iter()
        .filter_map(|a| a.external_id.as_deref())
        .collect();
    assert_eq!(external_ids.len(), 1);
    // The contexts must encode the step index and attempt number
    // distinctly: step 1 attempt 1, step 1 attempt 2, step 2 attempt 1.
    let mut contexts: Vec<&str> = persisted.iter().map(|a| a.context.as_str()).collect();
    contexts.sort();
    let unique_contexts: HashSet<&str> = contexts.iter().copied().collect();
    assert_eq!(
        unique_contexts.len(),
        3,
        "3 attempts ⇒ 3 unique contexts; got {:?}",
        unique_contexts
    );
    let _ = server.requests.recv().await;
    let _ = server.requests.recv().await;
    let _ = server.requests.recv().await;
}

// -------------------------------------------------------------------------
// 9. Sanity: ordering of Start/End with respect to the post-step
//    approval. The End event must persist before the approval is
//    presented (the workflow only asks after the End is durable), and
//    the Start of the next step (or the workflow_complete event) only
//    appears after the prior End + the Approve decision was sent.
// -------------------------------------------------------------------------

#[tokio::test]
async fn end_persists_before_post_step_approval_event() {
    // Two-step workflow so the first step's post-step HITL gate fires.
    let mut server = server(vec![answer("first"), answer("final")]).await;
    let tmp = tempdir().unwrap();
    let (engine, mut events) = engine(config(&server.url, tmp.path()));
    let session_path = engine.session.lock().await.path.clone();
    let workflow = simple_workflow("order", vec![("first {{input}}", true), ("second", false)]);
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
    let mut step_activity = Vec::new();
    let deadline = Duration::from_secs(5);
    let mut end_before_approval_seen = false;
    loop {
        match tokio::time::timeout(deadline, events.recv()).await {
            Ok(Some(UiEvent::Activity(a))) if a.kind == ActivityKind::WorkflowStep => {
                if a.phase == ActivityPhase::End {
                    end_before_approval_seen = true;
                }
                step_activity.push(a)
            }
            Ok(Some(UiEvent::Approval { reply, .. })) => {
                assert!(
                    end_before_approval_seen,
                    "End must be on the live channel before the approval arrives; got: {:?}",
                    step_activity
                );
                reply.send(Decision::Approve).unwrap();
                break;
            }
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    // The on-disk order pins this: End appears before any approval.
    let raw = std::fs::read_to_string(&session_path).unwrap();
    let mut step_end_offset = None;
    let mut approval_offset = None;
    for (i, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line).unwrap();
        match v["type"].as_str() {
            Some("activity") => {
                if v["data"]["phase"] == "end"
                    && v["data"]["kind"] == "workflow_step"
                    && step_end_offset.is_none()
                {
                    step_end_offset = Some(i);
                }
            }
            Some("approval") if approval_offset.is_none() => {
                approval_offset = Some(i);
            }
            _ => {}
        }
    }
    let end_line = step_end_offset.expect("workflow_step End must exist on disk");
    let approval_line = approval_offset.expect("approval event must exist on disk");
    assert!(
        end_line < approval_line,
        "End line {end_line} must precede approval line {approval_line}"
    );
    let _ = server.requests.recv().await;
    let _ = server.requests.recv().await;
}

// -------------------------------------------------------------------------
// 10. Render-before-Start: a step whose prompt template references an
//     undeclared variable fails the `template::render` call BEFORE the
//     Start activity record is emitted. The on-disk log therefore has
//     no orphan Start for that step. This pins the invariant that the
//     template render is a precondition for the lifecycle.
// -------------------------------------------------------------------------

#[tokio::test]
async fn undeclared_template_variable_in_step_prompt_fails_before_start_is_emitted() {
    // The step prompt references `{{unknown_var}}` which is not in the
    // render variable set, so `template::render` fails with
    // `Undefined template variable: unknown_var`. The engine must
    // surface this Err before `engine.emit_activity(start)` runs.
    // We bypass `Workflow::validate` (which would reject the prompt at
    // load time) by constructing the Workflow directly so we exercise
    // the run-time render path.
    let server = server(vec![answer("would-be-final")]).await;
    let tmp = tempdir().unwrap();
    let (engine, mut events) = engine(config(&server.url, tmp.path()));
    let session_path = engine.session.lock().await.path.clone();
    let workflow = Workflow {
        title: "render-fail".into(),
        author: "test".into(),
        steps: vec![Step {
            agent: None,
            model: "openai/gpt-4.1-mini".into(),
            prompt: "before {{unknown_var}} after".into(),
            mcps: vec![],
            hitl: false,
        }],
    };
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        workflow::run(
            &engine,
            workflow,
            "".into(),
            Selection::default(),
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap();
    // The run returns Err (template render failure). We do not care
    // about the exact message here; just that it propagates.
    assert!(
        result.is_err(),
        "run must surface the render error; got: {result:?}"
    );
    // The on-disk log must NOT contain a `WorkflowStep` Start for this
    // attempt: render-before-Start guarantees no orphan Start is ever
    // persisted when the render fails.
    let persisted = read_persisted_step_activities(&session_path);
    assert!(
        persisted.is_empty(),
        "render failure must not leave an unmatched Start on disk; got: {persisted:?}"
    );
    // Live channel should also not have a Start record. Drain
    // activities for a bounded window and confirm none appear.
    let live: Vec<ActivityEvent> = {
        let mut out = Vec::new();
        let deadline = Duration::from_secs(2);
        while let Ok(Some(event)) = tokio::time::timeout(deadline, events.recv()).await {
            if let UiEvent::Activity(activity) = event {
                if activity.kind == ActivityKind::WorkflowStep
                    && activity.phase == ActivityPhase::Start
                {
                    out.push(activity);
                }
            }
        }
        out
    };
    assert!(
        live.is_empty(),
        "live channel must not surface a Start before render failure; got: {live:?}"
    );
    // The server fixture never received a request, since the render
    // failed before the conversation call.
    assert_eq!(
        server.count.load(Ordering::SeqCst),
        0,
        "no provider request should reach the server when render fails first"
    );
}

// -------------------------------------------------------------------------
// 11. The workflow title in the activity title is sanitized through
//     the shared helper, so a workflow whose title contains control
//     characters still produces a display-safe `WorkflowStep` title.
//     The step index and prompt summary remain part of the prefix
//     after the sanitized title; the on-disk Start/End share the same
//     sanitized title.
// -------------------------------------------------------------------------

#[tokio::test]
async fn workflow_title_with_control_chars_is_sanitized_in_step_title() {
    let mut server = server(vec![answer("ok")]).await;
    let tmp = tempdir().unwrap();
    let (engine, _events) = engine(config(&server.url, tmp.path()));
    let session_path = engine.session.lock().await.path.clone();
    let workflow = Workflow {
        title: "name\twith\ncontrol\rmixed-up whitespace".into(),
        author: "test".into(),
        steps: vec![Step {
            agent: None,
            model: "openai/gpt-4.1-mini".into(),
            prompt: "draft the plan".into(),
            mcps: vec![],
            hitl: false,
        }],
    };
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        workflow::run(
            &engine,
            workflow,
            "".into(),
            Selection::default(),
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result, "ok");
    let persisted = read_persisted_step_activities(&session_path);
    assert_eq!(persisted.len(), 2);
    let start = persisted
        .iter()
        .find(|a| a.phase == ActivityPhase::Start)
        .unwrap();
    let end = persisted
        .iter()
        .find(|a| a.phase == ActivityPhase::End)
        .unwrap();
    // Control characters must be gone from the workflow title prefix.
    for forbidden in ['\n', '\r', '\t'] {
        assert!(
            !start.title.contains(forbidden),
            "workflow title must be sanitized; {:?} survived in {:?}",
            forbidden,
            start.title
        );
    }
    // The collapsed workflow title and step index/prompt appear
    // verbatim in the persisted Start title.
    assert_eq!(
        start.title,
        "workflow name with control mixed-up whitespace step 1: draft the plan"
    );
    assert_eq!(start.title, end.title);
    let _ = server.requests.recv().await;
}

#[tokio::test]
async fn workflow_titles_strip_bidi_zero_width_and_line_separator_characters() {
    let mut server = server(vec![answer("ok")]).await;
    let tmp = tempdir().unwrap();
    let (engine, _events) = engine(config(&server.url, tmp.path()));
    let session_path = engine.session.lock().await.path.clone();
    let workflow = Workflow {
        title: "safe\u{202e}title\u{200b}\u{2028}with 👨‍👩‍👧‍👦".into(),
        author: "test".into(),
        steps: vec![Step {
            agent: None,
            model: "openai/gpt-4.1-mini".into(),
            prompt: "answer".into(),
            mcps: vec![],
            hitl: false,
        }],
    };

    tokio::time::timeout(
        Duration::from_secs(5),
        workflow::run(
            &engine,
            workflow,
            "".into(),
            Selection::default(),
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap();

    let persisted = read_persisted_step_activities(&session_path);
    let start = persisted
        .iter()
        .find(|activity| activity.phase == ActivityPhase::Start)
        .unwrap();
    assert_eq!(start.title, "workflow safe title with 👨‍👩‍👧‍👦 step 1: answer");
    assert!(!start
        .title
        .chars()
        .any(diet_soda::text::is_unsafe_terminal_char));
    let _ = server.requests.recv().await;
}
