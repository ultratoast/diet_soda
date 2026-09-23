//! Provider edge-case regression suite. Each test pins a single documented
//! contract of the SSE decoder and the OpenAI/Anthropic streaming path so
//! future refactors cannot quietly change them. The suite uses the local
//! `support::server` fixture so it stays deterministic and credential-free;
//! no live model requests are issued.
mod support;
use diet_soda::model::{Message, ToolCall, UiEvent};
use diet_soda::{
    config::{ProviderConfig, ProviderKind},
    provider::{
        phases, IncompleteStreamError, ModelProvider, ModelRequest, RemoteProvider, SseDecoder,
    },
    session::{DisplayEvent, Session, TranscriptEntry},
};
use serde_json::{json, Value};
use std::time::Duration;
use support::*;
use tokio_util::sync::CancellationToken;

fn request(model: diet_soda::config::ModelConfig) -> ModelRequest {
    ModelRequest {
        model,
        system: "system".into(),
        messages: vec![],
        tools: vec![],
        context: "test".into(),
    }
}

fn status_events(events: &mut tokio::sync::mpsc::UnboundedReceiver<UiEvent>) -> Vec<String> {
    std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            UiEvent::Status { text, .. } => Some(text),
            _ => None,
        })
        .collect()
}

fn assert_status_sequence(statuses: &[String], streaming: bool) {
    assert_eq!(
        &statuses[..statuses.len().min(2)],
        &[phases::CONNECTING.to_owned(), phases::WAITING.to_owned()],
        "request must begin Connecting → Waiting; got {statuses:?}"
    );
    if streaming {
        assert_eq!(
            statuses.len(),
            3,
            "one Streaming phase is required; got {statuses:?}"
        );
        assert!(
            statuses[2].starts_with(phases::STREAMING_PREFIX)
                && statuses[2].contains("first data "),
            "Streaming must report first data; got {statuses:?}"
        );
    } else {
        assert_eq!(
            statuses.len(),
            2,
            "metadata-only events must not stream; got {statuses:?}"
        );
    }
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

// A. Invalid SSE JSON mid-stream: must surface IncompleteStreamError with
// partial text preserved, not a raw serde error.
#[tokio::test]
async fn invalid_sse_json_after_partial_text_is_incomplete_with_partial() {
    let mut body = format!(
        "data: {}\r\n\r\n",
        json!({"choices": [{"delta": {"content": "kept "}}]})
    );
    body.push_str("data: {not-json\r\n\r\n");
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
    let error = provider
        .stream(
            request(config.model.clone()),
            &events,
            &CancellationToken::new(),
        )
        .await
        .err()
        .expect("invalid SSE JSON must error");
    let incomplete = error
        .downcast_ref::<IncompleteStreamError>()
        .expect("malformed SSE JSON after partial text must surface IncompleteStreamError");
    assert_eq!(incomplete.message.content, "kept ");
    assert!(
        incomplete.message.tool_calls.is_empty(),
        "tool fragments must never persist in the incomplete marker"
    );
    assert!(
        incomplete.reason.contains("Invalid provider SSE JSON"),
        "reason should mention the JSON parse failure; got: {}",
        incomplete.reason
    );
}

// B. Invalid UTF-8 in the SSE body: the decoder surfaces the failure so the
// provider can convert it to IncompleteStreamError like other transport
// errors.
#[tokio::test]
async fn invalid_utf8_body_is_an_error() {
    let server = server(vec![Reply {
        status: 200,
        content_type: "text/event-stream".into(),
        body: "data: {\r\n\r\n".into(),
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
    let mut parser = SseDecoder::default();
    assert!(
        parser.push(&[0xff, 0xfe, b'\n']).is_err(),
        "invalid UTF-8 must error in decoder"
    );
    let _ = provider
        .stream(
            request(config.model.clone()),
            &events,
            &CancellationToken::new(),
        )
        .await;
}

// C. Clean EOF with finish_reason on an empty stream (no deltas at all):
// zero-content completion must succeed with an empty message.
#[tokio::test]
async fn clean_eof_finish_reason_only_no_content_succeeds() {
    let body = format!(
        "data: {}\r\n\r\n",
        json!({"choices": [{"finish_reason": "stop", "delta": {}}]})
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
    let response = provider
        .stream(
            request(config.model.clone()),
            &events,
            &CancellationToken::new(),
        )
        .await
        .expect("finish_reason-only stream must succeed");
    assert_eq!(response.message.content, "");
    assert!(response.message.tool_calls.is_empty());
}

// D. finish_reason "null" literal must NOT satisfy the clean-EOF rule.
#[tokio::test]
async fn eof_with_only_null_finish_reason_is_incomplete() {
    let mut body = format!(
        "data: {}\r\n\r\n",
        json!({"choices": [{"delta": {"content": "partial"}}]})
    );
    body.push_str(&format!(
        "data: {}\r\n\r\n",
        json!({"choices": [{"finish_reason": null, "delta": {}}]})
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
    let error = provider
        .stream(
            request(config.model.clone()),
            &events,
            &CancellationToken::new(),
        )
        .await
        .err()
        .expect("null finish_reason must not authorize clean EOF");
    let incomplete = error
        .downcast_ref::<IncompleteStreamError>()
        .expect("must surface IncompleteStreamError");
    assert_eq!(incomplete.message.content, "partial");
}

// E. Tool call id sent in pieces across deltas must concatenate.
#[tokio::test]
async fn split_tool_id_and_name_reassemble() {
    let server = server(vec![Reply::sse(
        vec![
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_","function":{"name":"we"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"x","function":{"name":"b_fetch","arguments":"{\"url\":\"u\"}"}}]}}]}),
        ],
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
        .expect("split id/name must reassemble");
    assert_eq!(response.message.tool_calls[0].id, "call_x");
    assert_eq!(response.message.tool_calls[0].name, "web_fetch");
    assert_eq!(
        serde_json::from_str::<Value>(&response.message.tool_calls[0].arguments).unwrap(),
        json!({"url":"u"})
    );
}

// E2. OpenAI-compatible tool-call-only output may complete at clean EOF when
// the call has an id, name, empty arguments, and a finish reason, even without
// a [DONE] event. Empty arguments are normalized to an empty JSON object.
#[tokio::test]
async fn tool_call_only_clean_eof_normalizes_empty_arguments() {
    let body = format!(
        "data: {}\r\n\r\n",
        json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_empty",
                        "function": {"name": "lookup", "arguments": ""}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })
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
    let response = provider
        .stream(
            request(config.model.clone()),
            &events,
            &CancellationToken::new(),
        )
        .await
        .expect("complete tool-call-only stream must succeed at clean EOF");

    assert_eq!(response.message.tool_calls.len(), 1);
    assert_eq!(response.message.tool_calls[0].id, "call_empty");
    assert_eq!(response.message.tool_calls[0].name, "lookup");
    assert_eq!(response.message.tool_calls[0].arguments, "{}");
}

// E3. A non-empty malformed argument fragment must not qualify as a complete
// clean-EOF tool call.
#[tokio::test]
async fn tool_call_only_clean_eof_with_invalid_arguments_is_incomplete() {
    let body = format!(
        "data: {}\r\n\r\n",
        json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_invalid",
                        "function": {"name": "lookup", "arguments": "not-json"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })
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
    let error = provider
        .stream(
            request(config.model.clone()),
            &events,
            &CancellationToken::new(),
        )
        .await
        .err()
        .expect("invalid non-empty tool arguments must fail at clean EOF");
    let incomplete = error
        .downcast_ref::<IncompleteStreamError>()
        .expect("invalid tool arguments must surface IncompleteStreamError");
    assert!(incomplete.message.tool_calls.is_empty());
}

// F1. Tool-call-only output is meaningful first data and emits exactly one
// Streaming phase before completion metadata.
#[tokio::test]
async fn tool_call_only_stream_emits_one_ordered_streaming_status() {
    let server = server(vec![Reply::sse(
        vec![
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"noop","arguments":"{}"}}]}}]}),
            json!({"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1}}),
        ],
        true,
    )])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let config = config(&server.url, tmp.path());
    let provider = RemoteProvider::new(config.providers["openrouter"].clone()).unwrap();
    let (events, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    provider
        .stream(
            request(config.model.clone()),
            &events,
            &CancellationToken::new(),
        )
        .await
        .expect("tool-call-only stream must succeed");
    drop(events);
    let statuses = status_events(&mut events_rx);
    assert_status_sequence(&statuses, true);
}

// F. Reasoning-only response (no content, no tool calls) must succeed and
// preserve reasoning text; tool-call fragment must not leak.
#[tokio::test]
async fn reasoning_only_response_succeeds() {
    let server = server(vec![Reply::sse(
        vec![
            json!({"choices":[{"delta":{"reasoning":"thinking hard"}}]}),
            json!({"choices":[{"finish_reason":"stop","delta":{}}]}),
        ],
        false,
    )])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let config = config(&server.url, tmp.path());
    let provider = RemoteProvider::new(config.providers["openrouter"].clone()).unwrap();
    let (events, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    let response = provider
        .stream(
            request(config.model.clone()),
            &events,
            &CancellationToken::new(),
        )
        .await
        .expect("reasoning-only stream must succeed");
    assert_eq!(response.message.reasoning.as_deref(), Some("thinking hard"));
    assert_eq!(response.message.content, "");
    drop(events);
    let statuses = status_events(&mut events_rx);
    assert_status_sequence(&statuses, true);
}

// F2. Usage and finish metadata are not meaningful first data and must not
// create a Streaming phase.
#[tokio::test]
async fn usage_and_finish_only_events_do_not_emit_streaming_status() {
    let server = server(vec![Reply::sse(
        vec![
            json!({"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1}}),
            json!({"choices":[{"finish_reason":"stop","delta":{}}]}),
        ],
        true,
    )])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let config = config(&server.url, tmp.path());
    let provider = RemoteProvider::new(config.providers["openrouter"].clone()).unwrap();
    let (events, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    provider
        .stream(
            request(config.model.clone()),
            &events,
            &CancellationToken::new(),
        )
        .await
        .expect("usage/finish-only stream must succeed");
    drop(events);
    let statuses = status_events(&mut events_rx);
    assert_status_sequence(&statuses, false);
}

// G. Oversized SSE event: decoder limit must trip before the 16 MB provider cap.
#[tokio::test]
async fn sse_event_size_limit_trips() {
    let mut parser = SseDecoder::default();
    let big = "x".repeat(2_100_000);
    let line = format!("data: {big}\n\n");
    let result = parser.push(line.as_bytes());
    assert!(result.is_err(), "2MB+ SSE event must trip decoder limit");
}

// H. Cancellation BEFORE first delta (during headers): must produce
// IncompleteStreamError, empty partial message, no tool-call leakage.
#[tokio::test]
async fn cancellation_before_first_delta() {
    let server = server(vec![Reply {
        status: 200,
        content_type: "text/event-stream".into(),
        body: format!(
            "data: {}\r\n\r\n",
            json!({"choices": [{"delta": {"content": "late"}}]})
        ),
        headers: vec![],
        header_delay: Some(Duration::from_secs(5)),
        chunk_delay: None,
        stall: None,
    }])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let config = config(&server.url, tmp.path());
    let provider = RemoteProvider::new(config.providers["openrouter"].clone()).unwrap();
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        c2.cancel();
    });
    let error = provider
        .stream(request(config.model.clone()), &events, &cancel)
        .await
        .err()
        .expect("cancellation must error");
    let incomplete = error
        .downcast_ref::<IncompleteStreamError>()
        .expect("cancellation before first delta must surface IncompleteStreamError");
    assert_eq!(incomplete.message.content, "");
    assert_eq!(incomplete.reason, "cooperative cancellation");
}

// I. Post-sentinel frames in the same chunk must be ignored: a valid delta
// after [DONE] must not append content, and the stream must succeed.
#[tokio::test]
async fn events_after_done_are_ignored_within_chunk() {
    let mut body = format!(
        "data: {}\r\n\r\n",
        json!({"choices":[{"delta":{"content":"ok"}}]})
    );
    body.push_str("data: [DONE]\r\n\r\n");
    body.push_str(&format!(
        "data: {}\r\n\r\n",
        json!({"choices":[{"delta":{"content":"IGNORED"}}]})
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
        .expect("trailing frames after [DONE] must not fail an already-complete stream");
    assert_eq!(
        response.message.content, "ok",
        "trailing frame after [DONE] must not append content"
    );
}

#[tokio::test]
async fn openai_done_returns_before_post_done_body_idle_timeout() {
    let mut body = format!(
        "data: {}\r\n\r\n",
        json!({"choices":[{"delta":{"content":"complete"}}]})
    );
    body.push_str("data: [DONE]\r\n\r\n");
    body.push_str(&format!(
        "data: {}\r\n\r\n",
        json!({"choices":[{"delta":{"content":"IGNORED"}}]})
    ));
    let server = server(vec![Reply {
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
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        provider.stream(
            request(config.model.clone()),
            &events,
            &CancellationToken::new(),
        ),
    )
    .await
    .expect("completed OpenAI stream must not wait for the body to close")
    .expect("[DONE] must complete the stream");
    assert_eq!(response.message.content, "complete");
    assert!(response.message.incomplete.is_none());
}

#[tokio::test]
async fn anthropic_message_stop_returns_before_post_stop_body_idle_timeout() {
    let mut body = String::new();
    body.push_str(&format!(
        "data: {}\r\n\r\n",
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})
    ));
    body.push_str(&format!(
        "data: {}\r\n\r\n",
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"complete"}})
    ));
    body.push_str("data: {\"type\":\"message_stop\"}\r\n\r\n");
    body.push_str(&format!(
        "data: {}\r\n\r\n",
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"IGNORED"}})
    ));
    let server = server(vec![Reply {
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
    let config = config(&server.url, tmp.path());
    let provider = RemoteProvider::new(ProviderConfig {
        kind: ProviderKind::Anthropic,
        base_url: server.url.clone(),
        api_key_env: None,
        headers: std::collections::BTreeMap::new(),
        timeout_seconds: 1,
    })
    .unwrap();
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        provider.stream(
            request(config.model.clone()),
            &events,
            &CancellationToken::new(),
        ),
    )
    .await
    .expect("completed Anthropic stream must not wait for the body to close")
    .expect("message_stop must complete the stream");
    assert_eq!(response.message.content, "complete");
    assert!(response.message.incomplete.is_none());
}

// J. Session reopen: incomplete assistant never re-enters model history and
// tool calls from completed turns are completed with synthetic tool results.
#[tokio::test]
async fn session_reopen_preserves_history_validity() {
    let tmp = tempfile::tempdir().unwrap();
    let mut session = Session::open(tmp.path(), Some("edge-reopen")).unwrap();
    session
        .record_message("main", Message::new("user", "q"))
        .unwrap();
    // Completed assistant turn with a tool call and NO tool result.
    let mut assistant = Message::new("assistant", "using tool");
    assistant.tool_calls.push(ToolCall {
        id: "tc1".into(),
        name: "noop".into(),
        arguments: "{}".into(),
    });
    session.record_message("main", assistant).unwrap();
    session
        .record_message(
            "main",
            Message::incomplete_assistant("partial", "idle timeout"),
        )
        .unwrap();
    drop(session);
    let reopened = Session::open(tmp.path(), Some("edge-reopen")).unwrap();
    // The repair pass must backfill a synthetic tool result for the
    // unmatched tool call. The incomplete assistant must not enter the
    // main-context history (it is a transcript marker only), so the
    // resumed history is user + assistant + tool result.
    assert_eq!(
        reopened.messages.len(),
        3,
        "user + assistant + synthetic tool result"
    );
    assert!(reopened.messages[2].role == "tool");
    assert_eq!(reopened.messages[2].tool_call_id.as_deref(), Some("tc1"));
    assert!(
        reopened.messages.iter().all(|m| m.incomplete.is_none()),
        "incomplete marker must not enter history"
    );
    let messages = display_messages(&reopened.display_events);
    assert_eq!(messages.len(), 4);
    assert!(messages.iter().any(|row| row.message.incomplete.is_some()));
}

// K. Anthropic clean EOF without message_stop must be incomplete.
#[tokio::test]
async fn anthropic_eof_without_message_stop_is_incomplete() {
    let server = server(vec![Reply {
        status: 200,
        content_type: "text/event-stream".into(),
        body: format!(
            "data: {}\r\n\r\n",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"partial "}})
        ),
        headers: vec![],
        header_delay: None,
        chunk_delay: None,
        stall: None,
    }])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let config = config(&server.url, tmp.path());
    let provider = RemoteProvider::new(ProviderConfig {
        kind: ProviderKind::Anthropic,
        base_url: server.url.clone(),
        api_key_env: None,
        headers: std::collections::BTreeMap::new(),
        timeout_seconds: 5,
    })
    .unwrap();
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let error = provider
        .stream(
            request(config.model.clone()),
            &events,
            &CancellationToken::new(),
        )
        .await
        .err()
        .expect("anthropic EOF without message_stop must error");
    let incomplete = error
        .downcast_ref::<IncompleteStreamError>()
        .expect("must surface IncompleteStreamError");
    assert_eq!(incomplete.message.content, "partial ");
}

// L. Provider-streamed error event mid-stream: partial text preserved.
#[tokio::test]
async fn provider_error_event_midstream_is_incomplete_with_partial() {
    let mut body = format!(
        "data: {}\r\n\r\n",
        json!({"choices": [{"delta": {"content": "so far"}}]})
    );
    body.push_str(&format!(
        "data: {}\r\n\r\n",
        json!({"error": {"message": "overloaded"}})
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
    let error = provider
        .stream(
            request(config.model.clone()),
            &events,
            &CancellationToken::new(),
        )
        .await
        .err()
        .expect("provider error event must error");
    let incomplete = error
        .downcast_ref::<IncompleteStreamError>()
        .expect("must surface IncompleteStreamError");
    assert_eq!(incomplete.message.content, "so far");
    assert!(incomplete.message.tool_calls.is_empty());
}

// M. Engine-level: incomplete turn then a successful next turn - the next
// request must not contain the partial assistant text.
#[tokio::test]
async fn next_turn_after_incomplete_excludes_partial_from_history() {
    use diet_soda::engine::Selection;
    // First reply stalls (idle timeout), second is a complete answer.
    let stalled = Reply {
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
    };
    let mut server = server(vec![
        stalled,
        Reply::sse(
            vec![
                json!({"choices":[{"delta":{"content":"final"}}]}),
                json!({"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1}}),
            ],
            true,
        ),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config
        .providers
        .get_mut("openrouter")
        .unwrap()
        .timeout_seconds = 1;
    let (engine, _events) = engine(config);
    let _ = engine
        .turn(
            "first".into(),
            Selection::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("first turn must fail incomplete");
    let result = engine
        .turn(
            "second".into(),
            Selection::default(),
            CancellationToken::new(),
        )
        .await
        .expect("second turn must succeed");
    assert_eq!(result, "final");
    let _ = server.requests.recv().await.unwrap();
    let second = server.requests.recv().await.unwrap();
    assert!(
        !second.body.contains("begin"),
        "partial text must not re-enter model history; got: {}",
        second.body
    );
    assert!(second.body.contains("first"), "user messages still present");
    assert!(second.body.contains("second"));
}

// N. Variant of I: invalid JSON AFTER [DONE] in the same chunk. The stream
// was already protocol-complete, so the trailing garbage frame must be
// ignored and the response must still succeed with the pre-[DONE] content.
#[tokio::test]
async fn invalid_frame_after_done_is_ignored() {
    let mut body = format!(
        "data: {}\r\n\r\n",
        json!({"choices":[{"delta":{"content":"ok"}}]})
    );
    body.push_str("data: [DONE]\r\n\r\n");
    body.push_str("data: {broken\r\n\r\n");
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
        .expect("trailing broken frame after [DONE] must not fail an already-complete stream");
    assert_eq!(response.message.content, "ok");
}
