mod support;
use diet_soda::{
    model::{Spend, Usage},
    provider::{ModelProvider, ModelRequest, RemoteProvider},
};
use serde_json::{json, Value};
use support::*;
use tokio_util::sync::CancellationToken;

async fn stream_once(model: &str, session: Option<&str>, reply: Reply) -> (Value, Usage) {
    let mut server = server(vec![reply]).await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.model.model = model.into();
    let mut provider = RemoteProvider::new(config.providers["openrouter"].clone()).unwrap();
    if let Some(id) = session {
        provider = provider.with_session_id(id);
    }
    let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
    let response = provider
        .stream(
            ModelRequest {
                model: config.model.clone(),
                system: "SYS".into(),
                messages: vec![],
                tools: vec![],
                context: "test".into(),
            },
            &events,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let request = server.requests.recv().await.unwrap();
    (serde_json::from_str(&request.body).unwrap(), response.usage)
}

#[tokio::test]
async fn anthropic_model_sends_session_id_and_cache_markers() {
    let (body, _) = stream_once("anthropic/claude-sonnet-4", Some("sess-123"), answer("ok")).await;
    assert_eq!(body["session_id"], "sess-123");
    assert_eq!(body["cache_control"]["type"], "ephemeral");
    assert_eq!(body["messages"][0]["content"][0]["text"], "SYS");
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
}

#[tokio::test]
async fn non_anthropic_model_gets_session_id_but_no_cache_markers() {
    let (body, _) = stream_once("openai/gpt-4.1-mini", Some("sess-123"), answer("ok")).await;
    assert_eq!(body["session_id"], "sess-123");
    assert!(body.get("cache_control").is_none());
    assert!(body["messages"][0]["content"].is_string());
}

#[tokio::test]
async fn no_session_id_means_no_field() {
    let (body, _) = stream_once("anthropic/claude-sonnet-4", None, answer("ok")).await;
    assert!(body.get("session_id").is_none());
}

#[tokio::test]
async fn session_id_is_truncated_to_256_chars() {
    let long = "a".repeat(300);
    let (body, _) = stream_once("openai/gpt-4.1-mini", Some(&long), answer("ok")).await;
    assert_eq!(body["session_id"].as_str().unwrap().len(), 256);
}

#[tokio::test]
async fn usage_reads_cache_read_and_write_tokens() {
    let reply = Reply::sse(
        vec![
            json!({"choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}]}),
            json!({"usage":{"prompt_tokens":1000,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":800,"cache_write_tokens":150}}}),
        ],
        true,
    );
    let (_, usage) = stream_once("anthropic/claude-sonnet-4", None, reply).await;
    assert_eq!(usage.cached_tokens, 800);
    assert_eq!(usage.cache_write_tokens, 150);
}

#[test]
fn spend_accumulates_cache_tokens() {
    let mut spend = Spend::default();
    for (read, write) in [(10, 1), (20, 2)] {
        spend.add(&Usage {
            cached_tokens: read,
            cache_write_tokens: write,
            ..Usage::default()
        });
    }
    assert_eq!(spend.cached_tokens, 30);
    assert_eq!(spend.cache_write_tokens, 3);
}
