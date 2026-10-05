mod support;
use diet_soda::{
    config::{ProviderConfig, ProviderKind},
    model::UiEvent,
    provider::RemoteProvider,
};
use serde_json::json;
use support::{config, engine, server, Reply};
use std::sync::atomic::Ordering;

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

#[tokio::test]
async fn prefetch_limits_emits_status_only_for_the_default_provider_failure() {
    // Both providers fail catalog discovery with HTTP 500, but only the
    // default model's provider may surface a user-visible Status event;
    // the non-default provider's failure stays warn-only.
    let primary = server(vec![Reply {
        status: 500,
        ..Reply::json(json!({}))
    }])
    .await;
    let backup = server(vec![Reply {
        status: 500,
        ..Reply::json(json!({}))
    }])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&primary.url, tmp.path());
    config.discover_model_limits = true;
    // The support helper leaves api_key_env unset; do not set one or the
    // missing-key pre-check would skip discovery and pass vacuously.
    config.providers.insert(
        "backup".into(),
        ProviderConfig {
            kind: ProviderKind::Openai,
            base_url: backup.url.clone(),
            api_key_env: None,
            headers: std::collections::BTreeMap::new(),
            timeout_seconds: 5,
            allow_private_networks: true,
        },
    );
    // The default model's provider is the `openrouter` key the helper built
    // against `primary`.
    assert_eq!(config.model.provider, "openrouter");
    let (engine, mut events) = engine(config);

    engine.prefetch_limits().await;

    // Both catalogs were actually queried, so both really failed.
    assert_eq!(primary.count.load(Ordering::SeqCst), 1);
    assert_eq!(backup.count.load(Ordering::SeqCst), 1);

    let mut statuses = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let UiEvent::Status { context, text } = event {
            statuses.push((context, text));
        }
    }
    assert_eq!(
        statuses.len(),
        1,
        "expected exactly one Status event, got: {statuses:?}"
    );
    assert_eq!(statuses[0].0, "main");
    assert!(
        statuses[0].1.contains("openrouter"),
        "status should name the default provider: {}",
        statuses[0].1
    );
    assert!(
        !statuses[0].1.contains("backup"),
        "non-default provider must not be named: {}",
        statuses[0].1
    );
}
