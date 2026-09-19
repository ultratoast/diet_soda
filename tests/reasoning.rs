mod support;
use diet_harness::{
    config::{Effort, ProviderKind, ReasoningConfig},
    engine::Selection,
    provider::{openai_messages, ModelProvider, ModelRequest, RemoteProvider},
};
use serde_json::{json, Value};
use support::*;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn effort_uses_each_providers_documented_field() {
    for kind in [
        ProviderKind::Openrouter,
        ProviderKind::Openai,
        ProviderKind::Litellm,
        ProviderKind::Anthropic,
    ] {
        let reply = if kind == ProviderKind::Anthropic {
            Reply::sse(
                vec![
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"answer"}}),
                    json!({"type":"message_stop"}),
                ],
                false,
            )
        } else {
            answer("answer")
        };
        let mut server = server(vec![reply]).await;
        let tmp = tempfile::tempdir().unwrap();
        let mut config = config(&server.url, tmp.path());
        config.providers.get_mut("openrouter").unwrap().kind = kind.clone();
        config.model.reasoning = Some(ReasoningConfig {
            supported_efforts: vec![Effort::Low, Effort::High],
            effort: Some(Effort::Low),
        });
        let (engine, _) = engine(config);
        engine
            .turn(
                "question".into(),
                Selection {
                    effort: Some(Effort::High),
                    ..Selection::default()
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let body: Value =
            serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
        let path = match kind {
            ProviderKind::Openrouter => "/reasoning/effort",
            ProviderKind::Anthropic => "/output_config/effort",
            _ => "/reasoning_effort",
        };
        assert_eq!(body.pointer(path).unwrap(), "high");
    }
}

#[tokio::test]
async fn signed_anthropic_thinking_blocks_survive_tool_continuation() {
    let first = Reply::sse(
        vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"provider thinking"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"opaque-signature"}}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call1","name":"read_file","input":{}}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"input.txt\"}"}}),
            json!({"type":"message_stop"}),
        ],
        false,
    );
    let last = Reply::sse(
        vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"done"}}),
            json!({"type":"message_stop"}),
        ],
        false,
    );
    let mut server = server(vec![first, last]).await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("input.txt"), "file contents").unwrap();
    let mut config = config(&server.url, tmp.path());
    config.providers.get_mut("openrouter").unwrap().kind = ProviderKind::Anthropic;
    let (engine, _) = engine(config);
    assert_eq!(
        engine
            .turn(
                "read".into(),
                Selection::default(),
                CancellationToken::new()
            )
            .await
            .unwrap(),
        "done"
    );
    server.requests.recv().await.unwrap();
    let followup: Value =
        serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    assert_eq!(
        followup["messages"][1]["content"][0],
        json!({"type":"thinking","thinking":"provider thinking","signature":"opaque-signature"})
    );
    assert_eq!(
        followup["messages"][1]["content"][1]["input"],
        json!({"path":"input.txt"})
    );
}

#[tokio::test]
async fn openrouter_reasoning_deltas_are_preserved_for_the_next_request() {
    let server = server(vec![Reply::sse(vec![
        json!({"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","index":0,"id":"r1","text":"first ","signature":null}]}}]}),
        json!({"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","index":0,"id":"r1","text":"second","signature":"sig"}],"content":"answer"}}]})
    ],true)]).await;
    let tmp = tempfile::tempdir().unwrap();
    let config = config(&server.url, tmp.path());
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
    let next = openai_messages("system", &[response.message]);
    assert_eq!(next[1]["reasoning_details"][0]["text"], "first second");
    assert_eq!(next[1]["reasoning_details"][0]["signature"], "sig");
}
