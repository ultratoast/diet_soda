mod support;
use diet_harness::{
    config::{ProviderConfig, ProviderKind},
    provider::RemoteProvider,
};
use serde_json::json;
use support::{server, Reply};

#[tokio::test]
async fn catalogs_use_configured_endpoints_and_provider_authentication() {
    let env = "DIET_TEST_CATALOG_TOKEN";
    std::env::set_var(env, "local-fixture-token");
    for kind in [
        ProviderKind::Openrouter,
        ProviderKind::Openai,
        ProviderKind::Litellm,
        ProviderKind::Anthropic,
    ] {
        let mut server = server(vec![Reply::json(json!({"data":[
            {"id":"vendor/model-one", "name":"Model One"},
            {"id":"model-two", "display_name":"Model Two"}
        ]}))])
        .await;
        let provider = RemoteProvider::new(ProviderConfig {
            kind: kind.clone(),
            base_url: format!("{}/v1/", server.url),
            api_key_env: Some(env.into()),
            timeout_seconds: 5,
        })
        .unwrap();
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 2);
        assert!(models
            .iter()
            .any(|m| m.id == "vendor/model-one" && m.name == "Model One"));
        assert!(models.iter().any(|m| m.name == "Model Two"));
        let request = server.requests.recv().await.unwrap();
        assert!(request.headers.starts_with("GET /v1/models HTTP/1.1"));
        assert!(request.body.is_empty());
        let headers = request.headers.to_lowercase();
        if kind == ProviderKind::Anthropic {
            assert!(headers.contains("x-api-key: local-fixture-token"));
            assert!(headers.contains("anthropic-version: 2023-06-01"));
        } else {
            assert!(headers.contains("authorization: bearer local-fixture-token"));
        }
    }
    std::env::remove_var(env);
}

#[tokio::test]
async fn catalog_pagination_deduplicates_models_and_rejects_broken_responses() {
    let mut server = server(vec![
        Reply::json(json!({"data":[{"id":"first"}],"has_more":true,"last_id":"first"})),
        Reply::json(json!({"data":[{"id":"first"},{"id":"second"}],"has_more":false})),
        Reply::json(json!({"error":"unavailable"})),
        Reply::json(json!({"data":[],"has_more":true,"last_id":"stuck"})),
        Reply::json(json!({"data":[],"has_more":true,"last_id":"stuck"})),
        Reply {
            status: 401,
            ..Reply::json(json!({}))
        },
    ])
    .await;
    let provider = RemoteProvider::new(ProviderConfig {
        kind: ProviderKind::Anthropic,
        base_url: server.url.clone(),
        api_key_env: None,
        timeout_seconds: 5,
    })
    .unwrap();
    let models = provider.list_models().await.unwrap();
    assert_eq!(
        models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["first", "second"]
    );
    server.requests.recv().await.unwrap();
    assert!(server
        .requests
        .recv()
        .await
        .unwrap()
        .headers
        .starts_with("GET /models?after_id=first HTTP/1.1"));
    assert!(provider
        .list_models()
        .await
        .unwrap_err()
        .to_string()
        .contains("data array"));
    assert!(provider
        .list_models()
        .await
        .unwrap_err()
        .to_string()
        .contains("did not advance"));
    assert!(provider
        .list_models()
        .await
        .unwrap_err()
        .to_string()
        .contains("401"));
}
