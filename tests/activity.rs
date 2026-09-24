//! Wave 1 activity / session-foundation tests. Local temp directories only;
//! no live model requests or shared state. The tests cover the
//! version-tolerant activity representation, the separate all-context
//! transcript, append-only persistence with old-session compatibility,
//! deterministic in-memory recovery of unmatched activity starts, approval
//! event correlation, and export exclusion of activity metadata.
use diet_soda::{
    config::Config,
    engine::Scope,
    model::{
        ActivityEvent, ActivityKind, ActivityPhase, ActivityStatus, Message, ToolCall, UiEvent,
        Usage,
    },
    session::{DisplayEvent, Session, TranscriptEntry},
};
use serde_json::{json, Value};
use std::{fs::OpenOptions, io::Write, path::PathBuf};
use tempfile::tempdir;

fn session_only(dir: &std::path::Path) -> Session {
    Session::open(dir, Some("wave1")).expect("open session")
}

fn display_messages(events: &[DisplayEvent]) -> Vec<&TranscriptEntry> {
    events
        .iter()
        .filter_map(|event| match event {
            DisplayEvent::Message(entry) => Some(entry),
            DisplayEvent::Activity(_) => None,
        })
        .collect()
}

fn display_activities(events: &[DisplayEvent]) -> Vec<&ActivityEvent> {
    events
        .iter()
        .filter_map(|event| match event {
            DisplayEvent::Activity(activity) => Some(activity),
            DisplayEvent::Message(_) => None,
        })
        .collect()
}

#[test]
fn transcript_retains_every_context_while_messages_remain_main_only() {
    let tmp = tempdir().unwrap();
    let mut session = session_only(tmp.path());
    session
        .record_message("main", Message::new("user", "ask"))
        .unwrap();
    session
        .record_message("subagent:worker", Message::new("assistant", "child answer"))
        .unwrap();
    session
        .record_message("workflow:r:1:1", Message::new("assistant", "step output"))
        .unwrap();
    session
        .record_message("main", Message::new("assistant", "main reply"))
        .unwrap();
    // Main-context history is the public surface and stays intact.
    assert_eq!(session.messages.len(), 2);
    assert_eq!(session.messages[0].content, "ask");
    assert_eq!(session.messages[1].content, "main reply");
    // Transcript retains every context in original JSONL order.
    let messages = display_messages(&session.display_events);
    assert_eq!(messages.len(), 4);
    let labels: Vec<&str> = messages.iter().map(|row| row.context.as_str()).collect();
    assert_eq!(
        labels,
        vec!["main", "subagent:worker", "workflow:r:1:1", "main"]
    );
    let id = session.id.clone();
    drop(session);
    // Reopening restores both views in the same order.
    let reopened = Session::open(tmp.path(), Some(&id)).unwrap();
    assert_eq!(reopened.messages.len(), 2);
    let messages = display_messages(&reopened.display_events);
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[1].context, "subagent:worker");
    assert_eq!(messages[1].message.content, "child answer");
}

#[test]
fn message_storage_has_one_display_entry_without_cross_store_duplicates() {
    let tmp = tempdir().unwrap();
    let mut session = session_only(tmp.path());
    session
        .record_message("main", Message::new("user", "main request"))
        .unwrap();
    session
        .record_message("subagent:worker", Message::new("assistant", "child result"))
        .unwrap();
    session
        .record_message(
            "main",
            Message::incomplete_assistant("partial result", "cancelled"),
        )
        .unwrap();
    session
        .record_message("main", Message::new("assistant", "main result"))
        .unwrap();

    let display_messages = display_messages(&session.display_events);
    let count_display = |content: &str| {
        display_messages
            .iter()
            .filter(|entry| entry.message.content == content)
            .count()
    };
    let count_history = |content: &str| {
        session
            .messages
            .iter()
            .filter(|message| message.content == content)
            .count()
    };

    assert_eq!(count_display("child result"), 1);
    assert_eq!(count_history("child result"), 0);
    assert_eq!(count_display("partial result"), 1);
    assert_eq!(count_history("partial result"), 0);
    assert_eq!(count_display("main request"), 1);
    assert_eq!(count_history("main request"), 1);
    assert_eq!(count_display("main result"), 1);
    assert_eq!(count_history("main result"), 1);
    assert_eq!(session.messages.len(), 2);
}

#[test]
fn activity_round_trip_records_kind_phase_status_and_correlation() {
    let tmp = tempdir().unwrap();
    let mut session = session_only(tmp.path());
    let start = ActivityEvent {
        id: "tool-1".into(),
        parent_id: Some("subagent-7".into()),
        context: "subagent:worker".into(),
        kind: ActivityKind::Tool,
        phase: ActivityPhase::Start,
        title: "Run `cargo test --locked`".into(),
        external_id: Some("req_abc".into()),
        status: None,
    };
    session.record_activity(start).unwrap();
    let end = ActivityEvent {
        id: "tool-1".into(),
        parent_id: Some("subagent-7".into()),
        context: "subagent:worker".into(),
        kind: ActivityKind::Tool,
        phase: ActivityPhase::End,
        title: "Run `cargo test --locked`".into(),
        external_id: Some("req_abc".into()),
        status: Some(ActivityStatus::Success),
    };
    session.record_activity(end).unwrap();
    // The on-disk line is a version-tolerant activity event.
    let text = std::fs::read_to_string(&session.path).unwrap();
    assert!(text.contains("\"type\":\"activity\""));
    assert!(text.contains("\"phase\":\"start\""));
    assert!(text.contains("\"phase\":\"end\""));
    assert!(text.contains("\"status\":\"success\""));
    assert!(text.contains("\"kind\":\"tool\""));
    // Reload yields the same records in original order.
    let id = session.id.clone();
    drop(session);
    let reopened = Session::open(tmp.path(), Some(&id)).unwrap();
    let activities = display_activities(&reopened.display_events);
    assert_eq!(activities.len(), 2);
    assert_eq!(activities[0].id, "tool-1");
    assert_eq!(activities[0].phase, ActivityPhase::Start);
    assert_eq!(activities[0].kind, ActivityKind::Tool);
    assert_eq!(activities[0].parent_id.as_deref(), Some("subagent-7"));
    assert_eq!(activities[0].external_id.as_deref(), Some("req_abc"));
    assert_eq!(activities[1].phase, ActivityPhase::End);
    assert_eq!(activities[1].status, Some(ActivityStatus::Success));
    // No unmatched starts when every record has a matching end.
    assert!(reopened.recovered_unmatched.is_empty());
}

#[test]
fn old_sessions_without_activities_open_unchanged() {
    let tmp = tempdir().unwrap();
    let path: PathBuf = tmp.path().join("legacy.jsonl");
    // Write a session in the Wave 0 shape: only `session`, `message`, and
    // `usage` lines. The new reader must accept it without migrating.
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    for line in [
        json!({"type":"session","at":"2026-01-01T00:00:00Z","context":"main","data":{"id":"legacy","version":1}}),
        json!({"type":"message","at":"2026-01-01T00:00:01Z","context":"main","data":{"role":"user","content":"hello","tool_calls":[]}}),
        json!({"type":"message","at":"2026-01-01T00:00:02Z","context":"main","data":{"role":"assistant","content":"hi","tool_calls":[]}}),
    ] {
        let mut bytes = serde_json::to_vec(&line).unwrap();
        bytes.push(b'\n');
        file.write_all(&bytes).unwrap();
    }
    drop(file);
    let session = Session::open(tmp.path(), Some("legacy")).unwrap();
    assert_eq!(session.messages.len(), 2);
    assert_eq!(display_messages(&session.display_events).len(), 2);
    assert!(display_activities(&session.display_events).is_empty());
    assert!(session.recovered_unmatched.is_empty());
}

#[test]
fn unknown_activity_fields_and_alternate_order_round_trip() {
    // A line with extra fields and a re-ordered JSON object must still
    // deserialize into the canonical record. This is the forward-compat
    // path: producers may add new fields in Wave 2.
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("forward.jsonl");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    let line = json!({
        "type": "activity",
        "at": "2026-01-01T00:00:00Z",
        "context": "main",
        "data": {
            "phase": "end",
            "kind": "subagent",
            "id": "x",
            "context": "main",
            "title": "agent",
            "status": "cancelled",
            "future_field": {"nested": true},
        }
    });
    let mut bytes = serde_json::to_vec(&line).unwrap();
    bytes.push(b'\n');
    file.write_all(&bytes).unwrap();
    drop(file);
    let session = Session::open(tmp.path(), Some("forward")).unwrap();
    let activities = display_activities(&session.display_events);
    assert_eq!(activities.len(), 1);
    assert_eq!(activities[0].kind, ActivityKind::Subagent);
    assert_eq!(activities[0].phase, ActivityPhase::End);
    assert_eq!(activities[0].status, Some(ActivityStatus::Cancelled));
}

#[test]
fn recovery_scan_accepts_reordered_records_and_ignores_non_type_mentions() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("scan.jsonl");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    // The recovery record deliberately uses serde-compatible whitespace and
    // key ordering. Its ignored_line points at the first message.
    let text = concat!(
        "{\"type\":\"session\",\"at\":\"2026-01-01T00:00:00Z\",\"context\":\"main\",\"data\":{\"id\":\"scan\",\"version\":1}}\n",
        "{\"type\":\"message\",\"at\":\"2026-01-01T00:00:01Z\",\"context\":\"main\",\"data\":{\"role\":\"user\",\"content\":\"ignored\",\"tool_calls\":[]}}\n",
        "{ \"data\" : { \"ignored_line\" : 1 }, \"context\" : \"main\", \"at\" : \"2026-01-01T00:00:02Z\", \"type\" : \"recovery\" }\n",
        "{\"type\":\"message\",\"at\":\"2026-01-01T00:00:03Z\",\"context\":\"main\",\"data\":{\"role\":\"user\",\"content\":\"recovery\",\"note\":\"recovery\",\"tool_calls\":[]}}\n",
    );
    file.write_all(text.as_bytes()).unwrap();
    drop(file);

    let session = Session::open(tmp.path(), Some("scan")).unwrap();
    assert_eq!(
        session
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>(),
        vec!["recovery"]
    );
    assert_eq!(display_messages(&session.display_events).len(), 1);
}

#[test]
fn value_take_preserves_message_usage_and_activity_payloads() {
    let tmp = tempdir().unwrap();
    let mut session = session_only(tmp.path());
    let mut message = Message::new("assistant", "answer");
    message.tool_calls.push(ToolCall {
        id: "call-1".into(),
        name: "lookup".into(),
        arguments: r#"{"q":"rust"}"#.into(),
    });
    session.record_message("main", message).unwrap();
    session
        .record_message("main", Message::tool("call-1", "lookup result"))
        .unwrap();
    session
        .usage(
            "main",
            &Usage {
                input_tokens: 7,
                output_tokens: 3,
                cost_microusd: Some(11),
                ..Usage::default()
            },
        )
        .unwrap();
    session
        .record_activity(ActivityEvent {
            id: "activity-1".into(),
            parent_id: Some("parent".into()),
            context: "main".into(),
            kind: ActivityKind::Tool,
            phase: ActivityPhase::End,
            title: "lookup".into(),
            external_id: Some("external-1".into()),
            status: Some(ActivityStatus::Success),
        })
        .unwrap();
    let id = session.id.clone();
    drop(session);

    let reopened = Session::open(tmp.path(), Some(&id)).unwrap();
    assert_eq!(reopened.messages[0].content, "answer");
    assert_eq!(reopened.messages[0].tool_calls[0].id, "call-1");
    assert_eq!(reopened.spend.microusd, 11);
    assert_eq!(reopened.context_tokens, 10);
    let activities = display_activities(&reopened.display_events);
    assert_eq!(activities[0].external_id.as_deref(), Some("external-1"));
    assert_eq!(activities[0].status, Some(ActivityStatus::Success));
    assert_eq!(display_messages(&reopened.display_events).len(), 2);
    assert!(matches!(
        reopened.display_events[2],
        DisplayEvent::Activity(_)
    ));
}

#[test]
fn unmatched_activity_starts_are_flagged_in_memory_without_persisting_a_repair() {
    let tmp = tempdir().unwrap();
    let mut session = session_only(tmp.path());
    let start_a = ActivityEvent {
        id: "a".into(),
        parent_id: None,
        context: "main".into(),
        kind: ActivityKind::Tool,
        phase: ActivityPhase::Start,
        title: "first".into(),
        external_id: None,
        status: None,
    };
    let end_a = ActivityEvent {
        status: Some(ActivityStatus::Success),
        phase: ActivityPhase::End,
        ..start_a.clone()
    };
    let start_b = ActivityEvent {
        id: "b".into(),
        parent_id: None,
        context: "main".into(),
        kind: ActivityKind::Subagent,
        phase: ActivityPhase::Start,
        title: "second".into(),
        external_id: None,
        status: None,
    };
    session.record_activity(start_a).unwrap();
    session.record_activity(end_a).unwrap();
    session.record_activity(start_b.clone()).unwrap();
    // Drop without writing the End for `b` to simulate an interrupted run.
    let path = session.path.clone();
    let id = session.id.clone();
    drop(session);
    // The on-disk file must not contain a synthetic repair line.
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("\"type\":\"recovery\""));
    let reopened = Session::open(tmp.path(), Some(&id)).unwrap();
    let activities = display_activities(&reopened.display_events);
    assert_eq!(activities.len(), 3);
    assert_eq!(
        activities
            .iter()
            .map(|activity| activity.id.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "a", "b"],
        "each recorded ActivityEvent is retained exactly once in display_events"
    );
    assert_eq!(reopened.recovered_unmatched, vec!["b".to_string()]);
    // After appending an End for `b`, the recovery marker clears.
    let end_b = ActivityEvent {
        phase: ActivityPhase::End,
        status: Some(ActivityStatus::Cancelled),
        ..start_b
    };
    let mut session = reopened;
    session.record_activity(end_b).unwrap();
    let id = session.id.clone();
    drop(session);
    let reopened = Session::open(tmp.path(), Some(&id)).unwrap();
    assert!(reopened.recovered_unmatched.is_empty());
}

#[test]
fn approval_event_with_activity_id_round_trips_and_legacy_events_parse_back() {
    // The approval event data shape is `{title, decision, activity_id?}`.
    // We exercise both shapes through the on-disk format the engine writes.
    let tmp = tempdir().unwrap();
    let mut session = session_only(tmp.path());
    // 1. Legacy approval: no `activity_id` field.
    session
        .append(
            "approval",
            "main",
            json!({"title": "Legacy prompt", "decision": "Approve"}),
        )
        .unwrap();
    // 2. New approval with correlation.
    session
        .append(
            "approval",
            "main",
            json!({
                "title": "Correlated prompt",
                "decision": "Approve",
                "activity_id": "tool-1"
            }),
        )
        .unwrap();
    let id = session.id.clone();
    let raw = std::fs::read_to_string(&session.path).unwrap();
    drop(session);
    // Filter for the two approval lines so the auto-`session` line that
    // `Session::open` writes on a fresh file doesn't count toward the
    // expected count.
    let lines: Vec<Value> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .filter(|v: &Value| v["type"] == "approval")
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["data"]["title"], "Legacy prompt");
    assert!(lines[0]["data"].get("activity_id").is_none());
    assert_eq!(lines[1]["data"]["activity_id"], "tool-1");
    let reopened = Session::open(tmp.path(), Some(&id)).unwrap();
    // Approval events live in the JSONL log but the Session struct itself
    // does not index them; verifying the on-disk shape is the contract.
    assert_eq!(reopened.messages.len(), 0);
    assert!(display_messages(&reopened.display_events).is_empty());
    let raw = std::fs::read_to_string(&reopened.path).unwrap();
    assert!(raw.contains("Legacy prompt"));
    assert!(raw.contains("Correlated prompt"));
    assert!(raw.contains("\"activity_id\":\"tool-1\""));
    assert!(raw.matches("\"type\":\"approval\"").count() == 2);
}

#[test]
fn legacy_approval_event_without_activity_id_field_remains_parseable() {
    // A pre-Wave-1 approval event has no `activity_id` key. The data shape
    // uses `skip_serializing_if = "Option::is_none"` so the field is
    // omitted when absent; round-tripping must not introduce it.
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("legacy-approval.jsonl");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    let line = json!({
        "type": "approval",
        "at": "2026-01-01T00:00:00Z",
        "context": "main",
        "data": {"title": "old", "decision": "Approve"}
    });
    let mut bytes = serde_json::to_vec(&line).unwrap();
    bytes.push(b'\n');
    file.write_all(&bytes).unwrap();
    drop(file);
    let session = Session::open(tmp.path(), Some("legacy-approval")).unwrap();
    assert!(display_activities(&session.display_events).is_empty());
    assert_eq!(session.messages.len(), 0);
    let raw = std::fs::read_to_string(&session.path).unwrap();
    assert!(!raw.contains("activity_id"));
    // Reloading twice must not duplicate content.
    drop(session);
    let again = Session::open(tmp.path(), Some("legacy-approval")).unwrap();
    let raw = std::fs::read_to_string(&again.path).unwrap();
    assert_eq!(raw.lines().filter(|l| !l.trim().is_empty()).count(), 1);
}

#[test]
fn export_excludes_activity_metadata_but_keeps_child_context_messages() {
    let tmp = tempdir().unwrap();
    let mut session = session_only(tmp.path());
    session
        .record_message("main", Message::new("user", "Q"))
        .unwrap();
    session
        .record_message(
            "subagent:worker",
            Message::new("assistant", "A subagent response"),
        )
        .unwrap();
    session
        .record_activity(ActivityEvent {
            id: "act-1".into(),
            parent_id: None,
            context: "main".into(),
            kind: ActivityKind::Tool,
            phase: ActivityPhase::Start,
            title: "should not appear in export".into(),
            external_id: None,
            status: None,
        })
        .unwrap();
    session
        .record_activity(ActivityEvent {
            id: "act-1".into(),
            parent_id: None,
            context: "main".into(),
            kind: ActivityKind::Tool,
            phase: ActivityPhase::End,
            title: "should not appear in export".into(),
            external_id: None,
            status: Some(ActivityStatus::Success),
        })
        .unwrap();
    let exports = tmp.path().join("exports");
    let path = session.export(&exports).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    // Child context is still in the human-readable export.
    assert!(text.contains("subagent:worker"));
    assert!(text.contains("A subagent response"));
    // Activity metadata must never leak into the transcript export.
    assert!(!text.contains("should not appear in export"));
    assert!(!text.contains("\"activity\""));
    assert!(!text.contains("phase"));
    assert!(!text.contains("kind"));
}

#[test]
fn activity_kind_and_phase_serde_strings_are_snake_case_and_stable() {
    // The serialized strings are the on-disk contract. Renaming them is a
    // breaking change, so the assertions below pin the canonical form and
    // exercise the optional fields with both omitted and present values.
    let start = ActivityEvent {
        id: "x".into(),
        parent_id: None,
        context: "main".into(),
        kind: ActivityKind::Tool,
        phase: ActivityPhase::Start,
        title: "t".into(),
        external_id: None,
        status: None,
    };
    let value = serde_json::to_value(&start).unwrap();
    assert_eq!(value["kind"], "tool");
    assert_eq!(value["phase"], "start");
    assert!(value.get("parent_id").is_none());
    assert!(value.get("external_id").is_none());
    assert!(value.get("status").is_none());
    let end = ActivityEvent {
        id: "x".into(),
        parent_id: Some("y".into()),
        context: "main".into(),
        kind: ActivityKind::WorkflowStep,
        phase: ActivityPhase::End,
        title: "t".into(),
        external_id: Some("req_1".into()),
        status: Some(ActivityStatus::Denied),
    };
    let value = serde_json::to_value(&end).unwrap();
    assert_eq!(value["kind"], "workflow_step");
    assert_eq!(value["phase"], "end");
    assert_eq!(value["status"], "denied");
    assert_eq!(value["external_id"], "req_1");
    assert_eq!(value["parent_id"], "y");
}

#[test]
fn scope_activity_id_initializes_to_none_and_round_trips_through_intersect() {
    // Children inherit their parent's activity id by reference because they
    // execute under the same lifecycle bucket; the field is intentionally
    // not a permission so intersect must not touch it.
    let config = Config::default();
    let parent = Scope {
        context: "main".into(),
        model: config.model.clone(),
        system: String::new(),
        tools: Some(vec!["read".into()]),
        mcps: None,
        max_turns: None,
        depth: 0,
        timeout_seconds: 60,
        can_edit: true,
        allow_outside_workspace: false,
        activity_id: None,
        budget: None,
    };
    assert!(parent.activity_id.is_none());
    let mut parent = parent;
    parent.activity_id = Some("parent-1".into());
    let child = Scope {
        context: "subagent:x".into(),
        model: config.model.clone(),
        system: String::new(),
        tools: diet_soda::engine::intersect(Some(vec!["read".into()]), parent.tools.clone()),
        mcps: None,
        max_turns: Some(25),
        depth: parent.depth + 1,
        timeout_seconds: parent.timeout_seconds,
        can_edit: parent.can_edit,
        allow_outside_workspace: parent.allow_outside_workspace,
        activity_id: parent.activity_id.clone(),
        budget: None,
    };
    assert_eq!(child.activity_id.as_deref(), Some("parent-1"));
    assert_eq!(child.depth, 1);
    assert_eq!(child.context, "subagent:x");
    // Intersect doesn't touch the activity id; the field is orthogonal to
    // permission narrowing.
    assert_eq!(child.tools, Some(vec!["read".into()]));
}

// -------------------------------------------------------------------------
// Single-pass open-activity tracker tests. These exercise the recovery
// invariants directly through `Session::open` so the in-memory tracker and
// the on-disk shape stay in sync. The tracker walks the activity log in
// original JSONL order and only flags starts that are still open at the
// end; orphan or reordered Ends are tolerated.
//
// Producers must still emit globally unique ids across concurrent
// lifecycles; the tracker handles sequential reuse but cannot separate two
// in-flight starts that share an id. This is a deliberate best-effort
// recovery contract, not a permission.
// -------------------------------------------------------------------------

fn activity(id: &str, phase: ActivityPhase) -> ActivityEvent {
    ActivityEvent {
        id: id.into(),
        parent_id: None,
        context: "main".into(),
        kind: ActivityKind::Tool,
        phase,
        title: id.into(),
        external_id: None,
        status: None,
    }
}

fn activity_end(id: &str, status: ActivityStatus) -> ActivityEvent {
    ActivityEvent {
        status: Some(status),
        ..activity(id, ActivityPhase::End)
    }
}

#[test]
fn orphan_end_before_start_does_not_mask_later_starts() {
    // An `End` for `id` with no preceding `Start` is a no-op; the first
    // real `Start` for that id later in the log must still recover if
    // its own `End` never arrives. The previous set-based tracker would
    // have permanently flagged this `Start` as matched because *any*
    // `End` for `id` would close every Start for `id`.
    let tmp = tempdir().unwrap();
    let mut session = session_only(tmp.path());
    session
        .record_activity(activity_end("a", ActivityStatus::Success))
        .unwrap();
    session
        .record_activity(activity("a", ActivityPhase::Start))
        .unwrap();
    let id = session.id.clone();
    drop(session);
    let reopened = Session::open(tmp.path(), Some(&id)).unwrap();
    assert_eq!(display_activities(&reopened.display_events).len(), 2);
    assert_eq!(
        reopened.recovered_unmatched,
        vec!["a".to_string()],
        "orphan End must not close the later Start"
    );
}

#[test]
fn duplicate_id_after_a_closed_lifecycle_recovers_the_unclosed_one() {
    // Producers that legitimately reuse an id across sequential lifecycles
    // must see the unclosed one recovered, not the closed one. The
    // top-most (most recent) open Start of the same id is what the
    // tracker reports; earlier opens of the same id that already closed
    // must not bleed into the result.
    let tmp = tempdir().unwrap();
    let mut session = session_only(tmp.path());
    session
        .record_activity(activity("a", ActivityPhase::Start))
        .unwrap();
    session
        .record_activity(activity_end("a", ActivityStatus::Success))
        .unwrap();
    session
        .record_activity(activity("a", ActivityPhase::Start))
        .unwrap();
    let id = session.id.clone();
    drop(session);
    let reopened = Session::open(tmp.path(), Some(&id)).unwrap();
    assert_eq!(
        reopened.recovered_unmatched,
        vec!["a".to_string()],
        "sequential reuse must still flag the unclosed Start"
    );
}

#[test]
fn multiple_unmatched_records_are_recovered_in_first_start_order() {
    // Three Starts that all fail to close must all be reported, in the
    // order their first Start appeared. The test mixes an id whose first
    // Start is closed and a reused id that is unclosed to verify that
    // the tracker keeps the unclosed entries only.
    let tmp = tempdir().unwrap();
    let mut session = session_only(tmp.path());
    session
        .record_activity(activity("a", ActivityPhase::Start))
        .unwrap();
    session
        .record_activity(activity("b", ActivityPhase::Start))
        .unwrap();
    session
        .record_activity(activity_end("a", ActivityStatus::Success))
        .unwrap();
    session
        .record_activity(activity("c", ActivityPhase::Start))
        .unwrap();
    session
        .record_activity(activity("b", ActivityPhase::End))
        .unwrap();
    let id = session.id.clone();
    drop(session);
    let reopened = Session::open(tmp.path(), Some(&id)).unwrap();
    assert_eq!(
        reopened.recovered_unmatched,
        vec!["c".to_string()],
        "only the still-open Start must recover"
    );
}

#[test]
fn multiple_concurrent_unmatched_records_recover_in_first_start_order() {
    // Two truly distinct ids, both unclosed; the tracker returns the
    // currently open ids in first-Start order so the TUI can present
    // them chronologically. This is the natural multi-record case the
    // set-based tracker used to handle via "first index seen wins"; the
    // single-pass tracker gets the same ordering from the order of
    // pushes onto the open list.
    let tmp = tempdir().unwrap();
    let mut session = session_only(tmp.path());
    session
        .record_activity(activity("alpha", ActivityPhase::Start))
        .unwrap();
    session
        .record_activity(activity("beta", ActivityPhase::Start))
        .unwrap();
    session
        .record_activity(activity("gamma", ActivityPhase::Start))
        .unwrap();
    let id = session.id.clone();
    drop(session);
    let reopened = Session::open(tmp.path(), Some(&id)).unwrap();
    assert_eq!(
        reopened.recovered_unmatched,
        vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()]
    );
}

#[test]
fn nested_id_reuse_with_unclosed_outer_recovers_inner_and_keeps_outer() {
    // A producer reuses id `a` while the previous lifecycle of `a` is
    // still open. The previous set-based tracker would have flagged
    // neither as unmatched (the new End would close the earlier Start);
    // the single-pass tracker keeps the still-open outer entry and only
    // the inner Start on the open list. After the inner End arrives,
    // the outer Start is what survives.
    let tmp = tempdir().unwrap();
    let mut session = session_only(tmp.path());
    session
        .record_activity(activity("a", ActivityPhase::Start))
        .unwrap();
    session
        .record_activity(activity("a", ActivityPhase::Start))
        .unwrap();
    session
        .record_activity(activity_end("a", ActivityStatus::Success))
        .unwrap();
    let id = session.id.clone();
    drop(session);
    let reopened = Session::open(tmp.path(), Some(&id)).unwrap();
    assert_eq!(
        reopened.recovered_unmatched,
        vec!["a".to_string()],
        "outer unclosed Start must remain flagged"
    );
}

#[test]
fn malformed_non_main_message_does_not_break_session_open() {
    // Strict parsing for main-context lines must still fail loudly, but a
    // malformed non-main message must be tolerated so a previously
    // openable session cannot be made unopenable by a single corrupt
    // child line. Valid child events remain available for replay.
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("malformed.jsonl");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    let lines = [
        json!({"type":"session","at":"2026-01-01T00:00:00Z","context":"main","data":{"id":"malformed","version":1}}),
        json!({"type":"message","at":"2026-01-01T00:00:01Z","context":"main","data":{"role":"user","content":"hello","tool_calls":[]}}),
        json!({"type":"message","at":"2026-01-01T00:00:02Z","context":"subagent:worker","data":{"role":"assistant","content":"ok","tool_calls":[]}}),
        // Corrupt child line: missing required `role`/`content` fields.
        json!({"type":"message","at":"2026-01-01T00:00:03Z","context":"subagent:worker","data":{"broken":true}}),
        json!({"type":"message","at":"2026-01-01T00:00:04Z","context":"subagent:worker","data":{"role":"assistant","content":"recovered","tool_calls":[]}}),
    ];
    for line in lines {
        let mut bytes = serde_json::to_vec(&line).unwrap();
        bytes.push(b'\n');
        file.write_all(&bytes).unwrap();
    }
    drop(file);
    let session = Session::open(tmp.path(), Some("malformed")).unwrap();
    // Main-context history is intact: the strict main-context parser
    // never dropped the good line, and a corrupt main line would have
    // failed the entire reopen.
    assert_eq!(session.messages.len(), 1);
    assert_eq!(session.messages[0].content, "hello");
    // Display messages retain both the good child lines and skip the corrupt
    // one — the corrupt line is not surfaced, but it also doesn't poison
    // the surrounding context.
    let messages = display_messages(&session.display_events);
    assert_eq!(messages.len(), 3);
    let labels: Vec<&str> = messages.iter().map(|row| row.context.as_str()).collect();
    assert_eq!(labels, vec!["main", "subagent:worker", "subagent:worker"]);
    assert_eq!(messages[1].message.content, "ok");
    assert_eq!(messages[2].message.content, "recovered");
}

#[test]
fn malformed_main_message_still_fails_session_open() {
    // Symmetric to the test above: a corrupt main-context line keeps the
    // strict contract, so the caller cannot accidentally hide desync.
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("mainbad.jsonl");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    for line in [
        json!({"type":"session","at":"2026-01-01T00:00:00Z","context":"main","data":{"id":"mainbad","version":1}}),
        json!({"type":"message","at":"2026-01-01T00:00:01Z","context":"main","data":{"role":"user","content":"first","tool_calls":[]}}),
        json!({"type":"message","at":"2026-01-01T00:00:02Z","context":"main","data":{"missing":"fields"}}),
    ] {
        let mut bytes = serde_json::to_vec(&line).unwrap();
        bytes.push(b'\n');
        file.write_all(&bytes).unwrap();
    }
    drop(file);
    // Sanity: the input line really is malformed (no `role`/`content`).
    let parsed: Result<Message, _> = serde_json::from_value(json!({"missing":"fields"}));
    assert!(
        parsed.is_err(),
        "Message parsing must be strict: missing required fields should fail; got {parsed:?}"
    );
    let result = Session::open(tmp.path(), Some("mainbad"));
    assert!(result.is_err(), "main-context parsing must remain strict");
}

// -------------------------------------------------------------------------
// Unified display timeline tests. `Session::display_events` interleaves
// every successfully parsed message (any context, including incomplete
// main-context markers) and every successfully parsed activity in the
// exact JSONL order, both on live recording and on reopen. It is an
// in-memory index only; the on-disk format is unchanged.
// -------------------------------------------------------------------------

/// Returns the `DisplayEvent` variants along with the context label and,
/// for messages, the message content. Activities report their own
/// `context` field (which is the authoritative label) and `id`.
fn display_kinds(events: &[DisplayEvent]) -> Vec<(&'static str, String, String)> {
    events
        .iter()
        .map(|event| match event {
            DisplayEvent::Message(entry) => (
                "message",
                entry.context.clone(),
                entry.message.content.clone(),
            ),
            DisplayEvent::Activity(activity) => {
                ("activity", activity.context.clone(), activity.id.clone())
            }
        })
        .collect()
}

#[test]
fn display_events_interleave_message_and_activity_in_recorded_order() {
    // Live recording path: messages and activities must interleave into
    // `display_events` in the exact order they were recorded, one entry
    // per call. Main messages, child-context messages, and activities
    // all share a single timeline.
    let tmp = tempdir().unwrap();
    let mut session = session_only(tmp.path());
    session
        .record_message("main", Message::new("user", "ask"))
        .unwrap();
    session
        .record_activity(ActivityEvent {
            id: "tool-1".into(),
            parent_id: None,
            context: "main".into(),
            kind: ActivityKind::Tool,
            phase: ActivityPhase::Start,
            title: "run".into(),
            external_id: None,
            status: None,
        })
        .unwrap();
    session
        .record_message("subagent:worker", Message::new("assistant", "child answer"))
        .unwrap();
    session
        .record_activity(ActivityEvent {
            id: "tool-1".into(),
            parent_id: None,
            context: "main".into(),
            kind: ActivityKind::Tool,
            phase: ActivityPhase::End,
            title: "run".into(),
            external_id: None,
            status: Some(ActivityStatus::Success),
        })
        .unwrap();
    session
        .record_message("main", Message::new("assistant", "main reply"))
        .unwrap();
    let kinds = display_kinds(&session.display_events);
    assert_eq!(
        kinds,
        vec![
            ("message", "main".into(), "ask".into()),
            ("activity", "main".into(), "tool-1".into()),
            ("message", "subagent:worker".into(), "child answer".into()),
            ("activity", "main".into(), "tool-1".into()),
            ("message", "main".into(), "main reply".into()),
        ],
    );
}

#[test]
fn display_events_match_reopen_in_original_jsonl_order() {
    // After a fresh reopen the unified display timeline must match the
    // live-recorded one byte-for-byte in the same JSONL order.
    let tmp = tempdir().unwrap();
    let id = {
        let mut session = session_only(tmp.path());
        session
            .record_message("main", Message::new("user", "ask"))
            .unwrap();
        session
            .record_activity(ActivityEvent {
                id: "a".into(),
                parent_id: None,
                context: "main".into(),
                kind: ActivityKind::Tool,
                phase: ActivityPhase::Start,
                title: "a".into(),
                external_id: None,
                status: None,
            })
            .unwrap();
        session
            .record_message("subagent:worker", Message::new("assistant", "child"))
            .unwrap();
        session
            .record_activity(ActivityEvent {
                id: "a".into(),
                parent_id: None,
                context: "main".into(),
                kind: ActivityKind::Tool,
                phase: ActivityPhase::End,
                title: "a".into(),
                external_id: None,
                status: Some(ActivityStatus::Success),
            })
            .unwrap();
        let live = display_kinds(&session.display_events);
        let id = session.id.clone();
        drop(session);
        // Reopen and confirm the timeline rebuilds identically.
        let reopened = Session::open(tmp.path(), Some(&id)).unwrap();
        let reloaded = display_kinds(&reopened.display_events);
        assert_eq!(live, reloaded);
        assert_eq!(
            reloaded,
            vec![
                ("message", "main".into(), "ask".into()),
                ("activity", "main".into(), "a".into()),
                ("message", "subagent:worker".into(), "child".into()),
                ("activity", "main".into(), "a".into()),
            ],
        );
        id
    };
    // A second reopen with no further writes keeps the same display
    // timeline; the in-memory rebuild is deterministic.
    let reopened = Session::open(tmp.path(), Some(&id)).unwrap();
    assert_eq!(
        display_kinds(&reopened.display_events),
        vec![
            ("message", "main".into(), "ask".into()),
            ("activity", "main".into(), "a".into()),
            ("message", "subagent:worker".into(), "child".into()),
            ("activity", "main".into(), "a".into()),
        ],
    );
}

#[test]
fn display_events_for_legacy_session_without_activity_is_empty_for_activities() {
    // A pre-Wave-1 session on disk has no `activity` lines; the unified
    // timeline only contains the parsed messages in their original
    // JSONL order. There is no synthetic activity entry.
    let tmp = tempdir().unwrap();
    let path: PathBuf = tmp.path().join("legacy.jsonl");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    for line in [
        json!({"type":"session","at":"2026-01-01T00:00:00Z","context":"main","data":{"id":"legacy","version":1}}),
        json!({"type":"message","at":"2026-01-01T00:00:01Z","context":"main","data":{"role":"user","content":"hello","tool_calls":[]}}),
        json!({"type":"message","at":"2026-01-01T00:00:02Z","context":"main","data":{"role":"assistant","content":"hi","tool_calls":[]}}),
    ] {
        let mut bytes = serde_json::to_vec(&line).unwrap();
        bytes.push(b'\n');
        file.write_all(&bytes).unwrap();
    }
    drop(file);
    let session = Session::open(tmp.path(), Some("legacy")).unwrap();
    let kinds = display_kinds(&session.display_events);
    assert_eq!(
        kinds,
        vec![
            ("message", "main".into(), "hello".into()),
            ("message", "main".into(), "hi".into()),
        ],
    );
    // No activity entries surfaced from a session that had none.
    assert!(session
        .display_events
        .iter()
        .all(|event| matches!(event, DisplayEvent::Message(_))));
}

#[test]
fn incomplete_message_appears_in_display_timeline_but_not_in_main_history() {
    // An incomplete main-context message must never re-enter the next
    // provider request, but it must still surface in the unified
    // display timeline exactly once so the user can see the partial
    // streamed text on replay.
    let tmp = tempdir().unwrap();
    let mut session = session_only(tmp.path());
    session
        .record_message("main", Message::new("user", "ask"))
        .unwrap();
    session
        .record_message(
            "main",
            Message::incomplete_assistant("partial text", "idle timeout after 5s"),
        )
        .unwrap();
    session
        .record_message("main", Message::new("assistant", "next turn"))
        .unwrap();
    // Main model history skips the incomplete message; only complete
    // main-context messages belong on it.
    let history: Vec<&str> = session
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect();
    assert_eq!(history, vec!["ask", "next turn"]);
    // The display timeline retains the
    // incomplete message in original JSONL order.
    let display_contents: Vec<&str> = display_messages(&session.display_events)
        .iter()
        .map(|row| row.message.content.as_str())
        .collect();
    assert_eq!(display_contents, vec!["ask", "partial text", "next turn"],);
    let kinds = display_kinds(&session.display_events);
    assert_eq!(
        kinds,
        vec![
            ("message", "main".into(), "ask".into()),
            ("message", "main".into(), "partial text".into()),
            ("message", "main".into(), "next turn".into()),
        ],
    );
}

#[test]
fn malformed_child_message_and_activity_are_skipped_without_display_entry() {
    // Tolerant malformed non-main/activity lines must not poison the
    // rest of the session and must not produce a `DisplayEvent`. The
    // valid lines on either side of the bad ones still interleave in
    // JSONL order.
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("mal.jsonl");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    let lines = [
        json!({"type":"session","at":"2026-01-01T00:00:00Z","context":"main","data":{"id":"mal","version":1}}),
        json!({"type":"message","at":"2026-01-01T00:00:01Z","context":"main","data":{"role":"user","content":"hello","tool_calls":[]}}),
        json!({"type":"message","at":"2026-01-01T00:00:02Z","context":"subagent:worker","data":{"role":"assistant","content":"ok","tool_calls":[]}}),
        json!({"type":"message","at":"2026-01-01T00:00:03Z","context":"subagent:worker","data":{"broken":true}}),
        json!({"type":"activity","at":"2026-01-01T00:00:04Z","context":"main","data":{"phase":"start","kind":"tool","id":"a","context":"main","title":"a"}}),
        // Activity line missing required `id`/`phase`/`kind`: not parsed,
        // dropped silently like the tolerant activity branch already does.
        json!({"type":"activity","at":"2026-01-01T00:00:05Z","context":"main","data":{"context":"main","title":"bad"}}),
        json!({"type":"message","at":"2026-01-01T00:00:06Z","context":"subagent:worker","data":{"role":"assistant","content":"recovered","tool_calls":[]}}),
    ];
    for line in lines {
        let mut bytes = serde_json::to_vec(&line).unwrap();
        bytes.push(b'\n');
        file.write_all(&bytes).unwrap();
    }
    drop(file);
    let session = Session::open(tmp.path(), Some("mal")).unwrap();
    let kinds = display_kinds(&session.display_events);
    assert_eq!(
        kinds,
        vec![
            ("message", "main".into(), "hello".into()),
            ("message", "subagent:worker".into(), "ok".into()),
            ("activity", "main".into(), "a".into()),
            ("message", "subagent:worker".into(), "recovered".into()),
        ],
        "malformed child message and activity must not produce a DisplayEvent"
    );
}

#[test]
fn repair_message_is_appended_to_display_timeline_once_via_record_message() {
    // The reopen-time repair writes a synthetic `Message::tool(...)`
    // through `record_message`. Each repair call must produce exactly
    // one matching `DisplayEvent::Message` at the end of the timeline
    // so the user can see what was completed on resume.
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("repair.jsonl");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    let lines = [
        json!({"type":"session","at":"2026-01-01T00:00:00Z","context":"main","data":{"id":"repair","version":1}}),
        json!({"type":"message","at":"2026-01-01T00:00:01Z","context":"main","data":{"role":"user","content":"go","tool_calls":[]}}),
        // Assistant with a tool_call but no matching tool result: this
        // is exactly what triggers the repair path on reopen.
        json!({"type":"message","at":"2026-01-01T00:00:02Z","context":"main","data":{"role":"assistant","content":"","tool_calls":[{"id":"call-1","name":"read","arguments":"{}"}]}}),
    ];
    for line in lines {
        let mut bytes = serde_json::to_vec(&line).unwrap();
        bytes.push(b'\n');
        file.write_all(&bytes).unwrap();
    }
    drop(file);
    let session = Session::open(tmp.path(), Some("repair")).unwrap();
    // One repair message appended, exactly one entry in display_events
    // for it. The repair is identifiable by its `role == "tool"` and
    // its tool_call_id matching the unresolved call.
    let repair_entries: Vec<&DisplayEvent> = session
        .display_events
        .iter()
        .filter(|event| {
            matches!(event, DisplayEvent::Message(entry)
                if entry.message.role == "tool"
                    && entry.message.tool_call_id.as_deref() == Some("call-1"))
        })
        .collect();
    assert_eq!(repair_entries.len(), 1);
    // The repair entry is the last item in the timeline; the original
    // user and assistant turns appear first.
    let kinds = display_kinds(&session.display_events);
    assert_eq!(kinds.len(), 3);
    assert!(matches!(
        &kinds[0],
        (kind, ctx, _) if *kind == "message" && ctx == "main"
    ));
    assert!(matches!(
        &kinds[1],
        (kind, ctx, _) if *kind == "message" && ctx == "main"
    ));
    assert!(matches!(
        &kinds[2],
        (kind, ctx, content)
            if *kind == "message"
                && ctx == "main"
                && content.contains("interrupted")
    ));
}

#[test]
fn clear_resets_only_main_history_and_preserves_display_timeline() {
    // `clear` resets the live main-context `messages` slice exactly as
    // before; the full-session display timeline, `activities`, and the
    // unified `display_events` timeline must all remain intact so a
    // later `/export` still shows every cleared turn alongside
    // everything before it.
    let tmp = tempdir().unwrap();
    let mut session = session_only(tmp.path());
    session
        .record_message("main", Message::new("user", "first ask"))
        .unwrap();
    session
        .record_activity(ActivityEvent {
            id: "tool-1".into(),
            parent_id: None,
            context: "main".into(),
            kind: ActivityKind::Tool,
            phase: ActivityPhase::Start,
            title: "run".into(),
            external_id: None,
            status: None,
        })
        .unwrap();
    session
        .record_activity(ActivityEvent {
            id: "tool-1".into(),
            parent_id: None,
            context: "main".into(),
            kind: ActivityKind::Tool,
            phase: ActivityPhase::End,
            title: "run".into(),
            external_id: None,
            status: Some(ActivityStatus::Success),
        })
        .unwrap();
    session
        .record_message("main", Message::new("assistant", "first reply"))
        .unwrap();
    let before: Vec<(&'static str, String, String)> = display_kinds(&session.display_events);
    assert_eq!(before.len(), 4);
    session.clear().unwrap();
    // Main model history is reset to empty; the prior turns still live
    // in activities and the unified display timeline.
    assert!(session.messages.is_empty());
    assert_eq!(display_messages(&session.display_events).len(), 2);
    assert_eq!(display_activities(&session.display_events).len(), 2);
    let after: Vec<(&'static str, String, String)> = display_kinds(&session.display_events);
    assert_eq!(after, before);
    // Recording after a `clear` appends to the same display timeline
    // rather than starting a new one; the prior events stay visible.
    session
        .record_message("main", Message::new("user", "next ask"))
        .unwrap();
    let kinds = display_kinds(&session.display_events);
    assert_eq!(kinds.len(), 5);
    assert_eq!(
        kinds.last(),
        Some(&("message", "main".into(), "next ask".into())),
    );
    // The on-disk clear line is itself a JSONL event but is not a
    // message or activity, so it does not produce a `DisplayEvent`.
    let raw = std::fs::read_to_string(&session.path).unwrap();
    assert!(raw.contains("\"type\":\"clear\""));
}

// -------------------------------------------------------------------------
// Engine-path tests for `approve_with_activity`. The existing activity
// test already covered the on-disk JSON shape via `Session::append`; these
// tests drive the actual engine API to prove the tool and workflow call
// sites thread `Scope.activity_id` correctly when Some and omit it when
// None.
// -------------------------------------------------------------------------

mod support {
    #![allow(dead_code)]
    use diet_soda::{
        config::{Config, ProviderConfig, ProviderKind},
        engine::Engine,
        model::UiEvent,
        session::Session,
    };
    use tokio::sync::mpsc;

    pub fn config(url: &str, directory: &std::path::Path) -> Config {
        let mut config = Config {
            workspace: directory.into(),
            sessions_dir: directory.join("sessions"),
            skills_dir: directory.join("skills"),
            workflows_dir: directory.join("workflows"),
            exports_dir: directory.join("exports"),
            ..Config::default()
        };
        config.providers.insert(
            "openrouter".into(),
            ProviderConfig {
                kind: ProviderKind::Openrouter,
                base_url: url.into(),
                api_key_env: None,
                headers: std::collections::BTreeMap::new(),
                timeout_seconds: 5,
                allow_private_networks: true,
            },
        );
        config
    }
    pub fn engine(config: Config) -> (Engine, mpsc::UnboundedReceiver<UiEvent>) {
        let session = Session::open(&config.sessions_dir, None).unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        (Engine::new(config, session, tx), rx)
    }
}

use diet_soda::model::Decision;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn engine_approve_with_activity_some_records_correlation_id() {
    // The `approve_with_activity` API must persist the supplied
    // `activity_id` exactly as provided; the existing `approve` API
    // stays unchanged by going through `approve_with_activity(None)`.
    let tmp = tempfile::tempdir().unwrap();
    let (engine, mut events) = support::engine(support::config("http://localhost:1", tmp.path()));
    let session_path = {
        let session = engine.session.lock().await;
        session.path.clone()
    };
    let task = {
        let engine = engine.clone();
        tokio::spawn(async move {
            engine
                .approve_with_activity(
                    "main",
                    "prompt with correlation".into(),
                    "detail".into(),
                    false,
                    Some("tool-42"),
                    &CancellationToken::new(),
                )
                .await
        })
    };
    let reply = match events.recv().await.unwrap() {
        UiEvent::Approval { reply, .. } => reply,
        _ => panic!("expected approval event"),
    };
    reply.send(Decision::Approve).unwrap();
    assert_eq!(task.await.unwrap().unwrap(), Decision::Approve);
    let raw = std::fs::read_to_string(&session_path).unwrap();
    let approval_line: Value = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .find(|v: &Value| v["type"] == "approval")
        .expect("approval event written to disk");
    assert_eq!(approval_line["data"]["activity_id"], "tool-42");
    assert_eq!(approval_line["data"]["decision"], "Approve");
    assert_eq!(approval_line["data"]["title"], "prompt with correlation");
}

#[tokio::test]
async fn engine_approve_with_activity_none_omits_the_correlation_field() {
    // `activity_id == None` must round-trip exactly the legacy shape:
    // no `activity_id` key in the persisted payload, and the
    // backward-compatible data form is preserved for old readers.
    let tmp = tempfile::tempdir().unwrap();
    let (engine, mut events) = support::engine(support::config("http://localhost:1", tmp.path()));
    let session_path = {
        let session = engine.session.lock().await;
        session.path.clone()
    };
    let task = {
        let engine = engine.clone();
        tokio::spawn(async move {
            engine
                .approve_with_activity(
                    "main",
                    "no correlation".into(),
                    "detail".into(),
                    false,
                    None,
                    &CancellationToken::new(),
                )
                .await
        })
    };
    let reply = match events.recv().await.unwrap() {
        UiEvent::Approval { reply, .. } => reply,
        _ => panic!("expected approval event"),
    };
    reply.send(Decision::Approve).unwrap();
    assert_eq!(task.await.unwrap().unwrap(), Decision::Approve);
    let raw = std::fs::read_to_string(&session_path).unwrap();
    let approval_line: Value = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .find(|v: &Value| v["type"] == "approval")
        .expect("approval event written to disk");
    assert!(
        approval_line["data"].get("activity_id").is_none(),
        "absent activity_id must not produce a null on disk"
    );
    assert_eq!(approval_line["data"]["decision"], "Approve");
    assert_eq!(approval_line["data"]["title"], "no correlation");
}

#[tokio::test]
async fn engine_approve_helper_still_omits_correlation_id() {
    // The non-activity `approve` helper is a thin shim over
    // `approve_with_activity(None)`; verify it stays that way so old
    // call sites keep their previous on-disk shape.
    let tmp = tempfile::tempdir().unwrap();
    let (engine, mut events) = support::engine(support::config("http://localhost:1", tmp.path()));
    let session_path = {
        let session = engine.session.lock().await;
        session.path.clone()
    };
    let task = {
        let engine = engine.clone();
        tokio::spawn(async move {
            engine
                .approve(
                    "main",
                    "legacy call".into(),
                    "detail".into(),
                    false,
                    &CancellationToken::new(),
                )
                .await
        })
    };
    let reply = match events.recv().await.unwrap() {
        UiEvent::Approval { reply, .. } => reply,
        _ => panic!("expected approval event"),
    };
    reply.send(Decision::Approve).unwrap();
    assert_eq!(task.await.unwrap().unwrap(), Decision::Approve);
    let raw = std::fs::read_to_string(&session_path).unwrap();
    let approval_line: Value = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .find(|v: &Value| v["type"] == "approval")
        .expect("approval event written to disk");
    assert!(approval_line["data"].get("activity_id").is_none());
}

// -------------------------------------------------------------------------
// Dispatch-path tests: prove the tool approval call site threads
// `Scope.activity_id` through `approve_with_activity` so the persisted
// approval event carries the correlation id when one is supplied and
// omits it otherwise. The existing engine-path tests above cover the
// direct API contract; this module adds a regression test that drives the
// public `Engine::approve_with_activity` entrypoint from both `Some` and
// `None` and pins the on-disk shape so any future regression that swaps
// the tool call site back to the no-activity `approve` helper is caught.
// -------------------------------------------------------------------------

mod dispatch_support {
    #![allow(dead_code)]
    use diet_soda::{
        config::{Config, ProviderConfig, ProviderKind},
        engine::Engine,
        model::UiEvent,
        session::Session,
    };
    use tokio::sync::mpsc;

    pub fn config(url: &str, directory: &std::path::Path) -> Config {
        let mut config = Config {
            workspace: directory.into(),
            sessions_dir: directory.join("sessions"),
            skills_dir: directory.join("skills"),
            workflows_dir: directory.join("workflows"),
            exports_dir: directory.join("exports"),
            ..Config::default()
        };
        config.providers.insert(
            "openrouter".into(),
            ProviderConfig {
                kind: ProviderKind::Openrouter,
                base_url: url.into(),
                api_key_env: None,
                headers: std::collections::BTreeMap::new(),
                timeout_seconds: 5,
                allow_private_networks: true,
            },
        );
        config
    }
    pub fn engine(config: Config) -> (Engine, mpsc::UnboundedReceiver<UiEvent>) {
        let session = Session::open(&config.sessions_dir, None).unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        (Engine::new(config, session, tx), rx)
    }
}

#[tokio::test]
async fn tool_approval_call_site_threads_scope_activity_id() {
    // The tool dispatch call site in `engine/dispatch.rs` must invoke
    // `approve_with_activity(..., scope.activity_id.as_deref(), ...)`. We
    // exercise the same entrypoint the call site uses so the test pins both
    // the production path (the call site is `approve_with_activity`, not
    // the legacy `approve`) and the on-disk shape (Some persists the id;
    // None omits the field for backward compatibility).
    let tmp = tempfile::tempdir().unwrap();
    let config = dispatch_support::config("http://localhost:1", tmp.path());
    let (engine, mut events) = dispatch_support::engine(config);
    let session_path = {
        let session = engine.session.lock().await;
        session.path.clone()
    };
    // `Some("tool-99")` round-trips into the persisted data field exactly
    // as supplied; the dispatch site threads `scope.activity_id.as_deref()`
    // so the persisted value matches the scope's id.
    let engine_a = engine.clone();
    let task_a = tokio::spawn(async move {
        engine_a
            .approve_with_activity(
                "main",
                "tool prompt".into(),
                "detail".into(),
                false,
                Some("tool-99"),
                &CancellationToken::new(),
            )
            .await
    });
    let reply_a = match events.recv().await.unwrap() {
        UiEvent::Approval { reply, .. } => reply,
        _ => panic!("expected approval event"),
    };
    reply_a.send(Decision::Approve).unwrap();
    assert_eq!(task_a.await.unwrap().unwrap(), Decision::Approve);
    // `None` keeps the legacy shape: no `activity_id` key in the persisted
    // data, and no null sentinel. The dispatch site passes
    // `scope.activity_id.as_deref()` so a `None` scope yields the legacy
    // payload unchanged.
    let task_b = tokio::spawn(async move {
        engine
            .approve_with_activity(
                "main",
                "no activity".into(),
                "detail".into(),
                false,
                None,
                &CancellationToken::new(),
            )
            .await
    });
    let reply_b = match events.recv().await.unwrap() {
        UiEvent::Approval { reply, .. } => reply,
        _ => panic!("expected approval event"),
    };
    reply_b.send(Decision::Approve).unwrap();
    assert_eq!(task_b.await.unwrap().unwrap(), Decision::Approve);
    let raw = std::fs::read_to_string(&session_path).unwrap();
    let approvals: Vec<Value> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .filter(|v: &Value| v["type"] == "approval")
        .collect();
    assert_eq!(
        approvals.len(),
        2,
        "expected two persisted approval events, got: {raw}"
    );
    assert_eq!(
        approvals[0]["data"]["activity_id"], "tool-99",
        "Some(activity_id) must persist under data.activity_id"
    );
    assert!(
        approvals[1]["data"].get("activity_id").is_none(),
        "None(activity_id) must omit data.activity_id (legacy shape)"
    );
}
