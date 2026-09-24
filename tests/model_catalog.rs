mod support;
use diet_soda::{
    config::{ProviderConfig, ProviderKind},
    provider::RemoteProvider,
};
use serde_json::json;
use support::{server, Reply};

#[tokio::test]
async fn catalogs_use_configured_endpoints_and_provider_authentication() {
    // Unique variable name (no fixed-name clobbering) but we still save
    // and restore so an outer test or developer shell that happens to set
    // the same name does not lose its value mid-suite.
    let env = "DIET_TEST_CATALOG_TOKEN";
    let prev = std::env::var(env).ok();
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
            headers: std::collections::BTreeMap::new(),
            timeout_seconds: 5,
            allow_private_networks: true,
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
    match prev {
        Some(value) => std::env::set_var(env, value),
        None => std::env::remove_var(env),
    }
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
        headers: std::collections::BTreeMap::new(),
        timeout_seconds: 5,
        allow_private_networks: true,
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

#[tokio::test]
async fn catalog_model_ids_reject_unsafe_chars_and_preserve_joiners() {
    let server = server(vec![
        Reply::json(json!({
            "data": [{
                "id": "vendor/mi\u{202e}ni\u{200b}\u{2028}",
                "name": "unsafe"
            }]
        })),
        Reply::json(json!({
            "data": [{
                "id": "vendor/👨‍👩‍👧‍👦-می\u{200c}رود",
                "name": "joiners"
            }]
        })),
    ])
    .await;
    let provider = RemoteProvider::new(ProviderConfig {
        kind: ProviderKind::Openai,
        base_url: server.url.clone(),
        api_key_env: None,
        headers: std::collections::BTreeMap::new(),
        timeout_seconds: 5,
        allow_private_networks: true,
    })
    .unwrap();

    let error = provider.list_models().await.unwrap_err();

    assert_eq!(error.to_string(), "Model list contains an invalid id");

    let models = provider.list_models().await.unwrap();

    assert_eq!(models[0].id, "vendor/👨‍👩‍👧‍👦-می\u{200c}رود");
}

#[tokio::test]
async fn catalog_applies_custom_provider_headers_and_expands_environment_values() {
    let env = "DIET_TEST_PROVIDER_HEADER";
    let previous = std::env::var(env).ok();
    std::env::set_var(env, "expanded-value");
    let mut headers = std::collections::BTreeMap::new();
    headers.insert("X-Custom-Provider".into(), "static-value".into());
    headers.insert("X-Env-Provider".into(), format!("${{{env}}}"));
    headers.insert("Authorization".into(), "Custom scheme-token".into());
    let mut server = server(vec![Reply::json(json!({"data":[]}))]).await;
    let provider = RemoteProvider::new(ProviderConfig {
        kind: ProviderKind::Openai,
        base_url: server.url.clone(),
        api_key_env: None,
        headers,
        timeout_seconds: 5,
        allow_private_networks: true,
    })
    .unwrap();

    provider.list_models().await.unwrap();
    let request = server.requests.recv().await.unwrap();
    let headers = request.headers.to_lowercase();
    assert!(headers.contains("x-custom-provider: static-value"));
    assert!(headers.contains("x-env-provider: expanded-value"));
    assert!(headers.contains("authorization: custom scheme-token"));

    match previous {
        Some(value) => std::env::set_var(env, value),
        None => std::env::remove_var(env),
    }
}

#[tokio::test]
async fn catalog_rejects_private_provider_addresses_without_explicit_opt_in() {
    let mut server = server(vec![Reply::json(json!({"data":[]}))]).await;
    let provider = RemoteProvider::new(ProviderConfig {
        kind: ProviderKind::Openai,
        base_url: server.url.clone(),
        api_key_env: None,
        headers: std::collections::BTreeMap::new(),
        timeout_seconds: 5,
        allow_private_networks: false,
    })
    .unwrap();

    let error = provider.list_models().await.unwrap_err();
    assert!(error
        .to_string()
        .contains("Refusing to connect to non-public address"));
    assert!(
        server.requests.try_recv().is_err(),
        "request reached private endpoint"
    );
}
