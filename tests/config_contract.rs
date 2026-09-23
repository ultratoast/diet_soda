use diet_soda::{
    config::{AgentConfig, Config, ProviderConfig, ProviderKind},
    engine::{Engine, Selection},
    model::{Message, UiEvent},
    provider::{ModelProvider, ModelRequest, RemoteProvider},
    session::Session,
};
use serde_json::json;
use std::fs;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn test_engine(mut config: Config, root: &std::path::Path) -> Engine {
    config.workspace = root.to_path_buf();
    config.sessions_dir = root.join("sessions");
    config.skills_dir = root.join("skills");
    config.config_dir = root.to_path_buf();
    let session = Session::open(&config.sessions_dir, None).unwrap();
    let (events, _receiver) = mpsc::unbounded_channel::<UiEvent>();
    Engine::new(config, session, events)
}

fn write_skill(root: &std::path::Path, name: &str, instructions: &str) {
    let directory = root.join(name);
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        directory.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {name} fixture\n---\n{instructions}\n"),
    )
    .unwrap();
}

#[tokio::test]
async fn disabled_tools_are_not_advertised_or_executed() {
    let root = tempfile::tempdir().unwrap();
    let config = Config {
        disabled_tools: vec!["read_file".into()],
        ..Config::default()
    };
    let engine = test_engine(config.clone(), root.path());
    let scope = engine
        .scope(&Selection::default(), "main", None)
        .await
        .unwrap();

    let advertised = engine
        .available(&scope, &CancellationToken::new())
        .await
        .unwrap();
    assert!(!advertised.iter().any(|tool| tool.spec.name == "read_file"));

    let mut enabled_config = config;
    enabled_config.disabled_tools.clear();
    engine.replace_config(enabled_config, true).await;
    let stale_advertisement = engine
        .available(&scope, &CancellationToken::new())
        .await
        .unwrap();
    engine
        .replace_config(
            Config {
                disabled_tools: vec!["read_file".into()],
                ..Config::default()
            },
            true,
        )
        .await;

    let error = engine
        .invoke(
            &scope,
            &diet_soda::model::ToolCall {
                id: "stale-read".into(),
                name: "read_file".into(),
                arguments: json!({"path": "missing.txt"}).to_string(),
            },
            &stale_advertisement,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Tool is disabled: read_file"));
}

#[tokio::test]
async fn missing_provider_key_is_reported_only_when_provider_is_used() {
    let variable = format!("DIET_SODA_MISSING_KEY_{}", uuid::Uuid::new_v4().simple());
    std::env::remove_var(&variable);
    let provider = RemoteProvider::new(ProviderConfig {
        kind: ProviderKind::Openai,
        base_url: "http://127.0.0.1:1".into(),
        api_key_env: Some(variable.clone()),
        headers: std::collections::BTreeMap::new(),
        timeout_seconds: 1,
    })
    .unwrap();

    let (events, _receiver) = mpsc::unbounded_channel();
    let request = ModelRequest {
        model: Config::default().model,
        system: "test".into(),
        messages: vec![Message {
            role: "user".into(),
            content: "test".into(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning: None,
            reasoning_details: vec![],
            native_content: vec![],
            incomplete: None,
        }],
        tools: vec![],
        context: "test".into(),
    };
    let error = match provider
        .stream(request, &events, &CancellationToken::new())
        .await
    {
        Ok(_) => panic!("provider use must require the configured API key"),
        Err(error) => error,
    };
    assert!(error
        .to_string()
        .contains(&format!("Missing environment variable {variable}")));
}

#[test]
fn multiple_default_agents_are_rejected() {
    let mut config = Config::default();
    config.agents.insert(
        "first".into(),
        AgentConfig {
            default: true,
            ..Default::default()
        },
    );
    config.agents.insert(
        "second".into(),
        AgentConfig {
            default: true,
            ..Default::default()
        },
    );

    let error = config.validate().unwrap_err();
    assert!(error
        .to_string()
        .contains("Only one agent may be marked default"));
}

#[test]
fn prompt_reference_rejects_over_one_mib_and_loads_exact_boundary() {
    let root = tempfile::tempdir().unwrap();
    let config_path = root.path().join("config.json");
    let prompt_path = root.path().join("prompt.md");
    fs::write(&config_path, r#"{"system_prompt":"./prompt.md"}"#).unwrap();

    fs::write(&prompt_path, vec![b'x'; 1_000_000]).unwrap();
    let loaded = Config::load(&config_path).unwrap();
    assert_eq!(loaded.system_prompt.len(), 1_000_000);

    fs::write(&prompt_path, vec![b'x'; 1_000_001]).unwrap();
    let error = Config::load(&config_path).unwrap_err();
    assert!(format!("{error:#}").contains("Prompt file exceeds 1 MB"));
}

#[tokio::test]
async fn agent_skills_override_global_skills_without_widening_parent_permissions() {
    let root = tempfile::tempdir().unwrap();
    write_skill(&root.path().join("skills"), "global", "global instructions");
    write_skill(&root.path().join("skills"), "agent", "agent instructions");

    let mut config = Config::default();
    config.skills.enabled = vec!["global".into()];
    config.agents.insert(
        "parent".into(),
        AgentConfig {
            tools: Some(vec!["read_file".into()]),
            ..Default::default()
        },
    );
    config.agents.insert(
        "child".into(),
        AgentConfig {
            tools: Some(vec!["read_file".into(), "write_file".into()]),
            skills: Some(vec!["agent".into()]),
            can_edit: true,
            ..Default::default()
        },
    );
    let engine = test_engine(config, root.path());
    let parent = engine
        .scope(
            &Selection {
                agent: Some("parent".into()),
                ..Default::default()
            },
            "parent",
            None,
        )
        .await
        .unwrap();
    let child = engine
        .scope(
            &Selection {
                agent: Some("child".into()),
                ..Default::default()
            },
            "child",
            Some(&parent),
        )
        .await
        .unwrap();

    assert_eq!(child.tools, Some(vec!["read_file".into()]));
    assert!(!child.can_edit);
    assert!(child.system.contains("agent instructions"));
    assert!(!child.system.contains("global instructions"));
}
