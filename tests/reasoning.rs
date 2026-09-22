mod support;
use diet_soda::{
    config::{Effort, ProviderKind, ReasoningConfig},
    engine::Selection,
    model::{Message, ToolCall},
    provider::{
        openai_messages, IncompleteStreamError, ModelProvider, ModelRequest, RemoteProvider,
    },
    session::Session,
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
    let (engine, mut events) = engine(config);
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
    let visible_deltas: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            diet_soda::model::UiEvent::Delta { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(visible_deltas, vec!["done"]);
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

#[tokio::test]
async fn reopened_session_preserves_reasoning_metadata_for_the_next_tool_continuation() {
    let mut server = server(vec![answer("resumed")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.providers.get_mut("openrouter").unwrap().kind = ProviderKind::Openrouter;

    let mut session = Session::open(&config.sessions_dir, Some("reasoning-resume")).unwrap();
    session
        .record_message("main", Message::new("user", "Use the tool"))
        .unwrap();
    let mut assistant = Message::new("assistant", "");
    assistant.tool_calls.push(ToolCall {
        id: "call-resume".into(),
        name: "lookup".into(),
        arguments: r#"{"key":"status"}"#.into(),
    });
    assistant.reasoning = Some("signed plan".into());
    assistant.reasoning_details = vec![json!({
        "type": "reasoning.text",
        "id": "reason-1",
        "text": "signed plan",
        "signature": "native-signature"
    })];
    session.record_message("main", assistant).unwrap();
    session
        .record_message("main", Message::tool("call-resume", "ready"))
        .unwrap();
    session.checkpoint().unwrap();
    drop(session);

    let reopened = Session::open(&config.sessions_dir, Some("reasoning-resume")).unwrap();
    assert_eq!(reopened.messages.len(), 3);
    assert_eq!(
        reopened.messages[1].reasoning.as_deref(),
        Some("signed plan")
    );
    assert_eq!(
        reopened.messages[1].reasoning_details[0]["signature"],
        "native-signature"
    );

    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let engine = diet_soda::engine::Engine::new(config, reopened, events);
    engine
        .turn(
            "Continue after the tool result".into(),
            Selection::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let request: Value = serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let assistant_request = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "assistant")
        .unwrap();
    assert_eq!(assistant_request["reasoning"], "signed plan");
    assert_eq!(
        assistant_request["reasoning_details"][0]["signature"],
        "native-signature"
    );
    assert_eq!(assistant_request["tool_calls"][0]["id"], "call-resume");
}

#[tokio::test]
async fn incomplete_reasoning_stream_drops_continuation_metadata_before_session_reopen() {
    let server = server(vec![Reply::sse(
        vec![
            json!({"choices":[{"delta":{"reasoning":"private partial plan","reasoning_details":[{"type":"reasoning.text","signature":"unsafe-signature"}],"content":"visible"}}]}),
        ],
        false,
    )])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let config = config(&server.url, tmp.path());
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let result = RemoteProvider::new(config.providers["openrouter"].clone())
        .unwrap()
        .stream(
            ModelRequest {
                model: config.model.clone(),
                system: "system".into(),
                messages: vec![],
                tools: vec![],
                context: "test".into(),
            },
            &events,
            &CancellationToken::new(),
        )
        .await;
    let error = match result {
        Ok(_) => panic!("incomplete stream unexpectedly completed"),
        Err(error) => error,
    };
    let partial = error.downcast_ref::<IncompleteStreamError>().unwrap();
    assert_eq!(partial.message.content, "visible");
    assert!(partial.message.reasoning.is_none());
    assert!(partial.message.reasoning_details.is_empty());
    assert!(partial.message.native_content.is_empty());
    assert!(partial.message.tool_calls.is_empty());

    let mut session = Session::open(&config.sessions_dir, Some("incomplete-reasoning")).unwrap();
    session
        .record_message("main", partial.message.clone())
        .unwrap();
    session.checkpoint().unwrap();
    drop(session);
    let reopened = Session::open(&config.sessions_dir, Some("incomplete-reasoning")).unwrap();
    assert!(reopened.messages.is_empty());
    let displayed = reopened
        .display_events
        .iter()
        .filter_map(|event| match event {
            diet_soda::session::DisplayEvent::Message(entry) => Some(&entry.message),
            diet_soda::session::DisplayEvent::Activity(_) => None,
        })
        .find(|message| message.incomplete.is_some())
        .unwrap();
    assert_eq!(displayed.content, "visible");
    assert!(displayed.reasoning.is_none());
    assert!(displayed.reasoning_details.is_empty());
    assert!(displayed.native_content.is_empty());
}

#[tokio::test]
async fn reopened_session_preserves_signed_anthropic_content_for_tool_continuation() {
    let mut server = server(vec![Reply::sse(
        vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"resumed"}}),
            json!({"type":"message_stop"}),
        ],
        false,
    )])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.providers.get_mut("openrouter").unwrap().kind = ProviderKind::Anthropic;

    let mut session = Session::open(&config.sessions_dir, Some("anthropic-resume")).unwrap();
    session
        .record_message("main", Message::new("user", "Use the tool"))
        .unwrap();
    let mut assistant = Message::new("assistant", "");
    assistant.tool_calls.push(ToolCall {
        id: "call-anthropic-resume".into(),
        name: "lookup".into(),
        arguments: r#"{"key":"status"}"#.into(),
    });
    assistant.native_content = vec![
        json!({
            "type": "thinking",
            "thinking": "private signed plan",
            "signature": "anthropic-signature"
        }),
        json!({
            "type": "tool_use",
            "id": "call-anthropic-resume",
            "name": "lookup",
            "input": {"key": "status"}
        }),
    ];
    session.record_message("main", assistant).unwrap();
    session
        .record_message("main", Message::tool("call-anthropic-resume", "ready"))
        .unwrap();
    session.checkpoint().unwrap();
    drop(session);

    let reopened = Session::open(&config.sessions_dir, Some("anthropic-resume")).unwrap();
    assert_eq!(
        reopened.messages[1].native_content[0]["signature"],
        "anthropic-signature"
    );
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let engine = diet_soda::engine::Engine::new(config, reopened, events);
    engine
        .turn(
            "Continue after the tool result".into(),
            Selection::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let request: Value = serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let assistant_request = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "assistant")
        .unwrap();
    assert_eq!(
        assistant_request["content"][0],
        json!({
            "type": "thinking",
            "thinking": "private signed plan",
            "signature": "anthropic-signature"
        })
    );
    assert_eq!(
        assistant_request["content"][1]["id"],
        "call-anthropic-resume"
    );
}
