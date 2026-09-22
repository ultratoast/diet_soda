mod support;

use diet_soda::{
    config::{AgentConfig, Effort, ModelConfig, ProviderKind, ReasoningConfig},
    engine::Selection,
    model::ToolCall,
    workflow::{self, Step, Workflow},
};
use serde_json::json;
use support::{answer, config, engine, server};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

#[test]
fn anthropic_rejects_none_and_minimal_reasoning_efforts() {
    for effort in [Effort::None, Effort::Minimal] {
        let reasoning = ReasoningConfig {
            supported_efforts: vec![effort],
            effort: Some(effort),
        };

        let error = reasoning.validate(&ProviderKind::Anthropic).unwrap_err();

        assert!(error
            .to_string()
            .contains("output_config.effort accepts low, medium, high, xhigh, or max"));
    }
}

#[tokio::test]
async fn workflow_effort_override_is_rejected_before_any_step_model_request() {
    let server = server(vec![answer("unexpected request")]).await;
    let tmp = tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.models.insert(
        "supported".into(),
        ModelConfig {
            reasoning: Some(ReasoningConfig {
                supported_efforts: vec![Effort::High],
                effort: None,
            }),
            ..config.model.clone()
        },
    );
    config.models.insert(
        "unsupported".into(),
        ModelConfig {
            reasoning: Some(ReasoningConfig {
                supported_efforts: vec![Effort::Low],
                effort: None,
            }),
            ..config.model.clone()
        },
    );
    let (engine, _) = engine(config);
    let workflow = Workflow {
        title: "mixed capability workflow".into(),
        author: "test".into(),
        steps: vec![
            Step {
                agent: None,
                model: "supported".into(),
                prompt: "first".into(),
                mcps: vec![],
                hitl: false,
            },
            Step {
                agent: None,
                model: "unsupported".into(),
                prompt: "second".into(),
                mcps: vec![],
                hitl: false,
            },
        ],
    };

    let error = workflow::run(
        &engine,
        workflow,
        "input".into(),
        Selection {
            effort: Some(Effort::High),
            ..Selection::default()
        },
        CancellationToken::new(),
    )
    .await
    .unwrap_err();

    assert!(format!("{error:#}").contains("Effort high is unsupported"));
    assert_eq!(server.count.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn load_skill_discovers_outside_workspace_skill_without_granting_write_access() {
    let workspace = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let skill_dir = outside.path().join("local-skill");
    std::fs::create_dir(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: local-skill\ndescription: Local instructions\n---\nUse the local instructions.",
    )
    .unwrap();

    let mut config = config("http://127.0.0.1:1", workspace.path());
    config.skills.directories.push(outside.path().into());
    config.agents.insert(
        "readonly".into(),
        AgentConfig {
            tools: Some(vec!["load_skill".into(), "write_file".into()]),
            ..AgentConfig::default()
        },
    );
    let (engine, _) = engine(config);
    let scope = engine
        .scope(
            &Selection {
                agent: Some("readonly".into()),
                ..Selection::default()
            },
            "main",
            None,
        )
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let registered = engine.available(&scope, &cancel).await.unwrap();

    assert!(!scope.can_edit);
    assert!(registered.iter().any(|tool| tool.spec.name == "load_skill"));
    assert!(!registered.iter().any(|tool| tool.spec.name == "write_file"));
    let call = ToolCall {
        id: "skill-call".into(),
        name: "load_skill".into(),
        arguments: json!({"name":"local-skill"}).to_string(),
    };
    let result = engine
        .invoke(&scope, &call, &registered, &cancel)
        .await
        .unwrap();

    assert_eq!(result["name"], "local-skill");
    assert_eq!(result["instructions"], "Use the local instructions.");
    assert_eq!(result["directory"], skill_dir.display().to_string());
}

#[tokio::test]
async fn default_agent_timeout_is_thirty_minutes() {
    let tmp = tempdir().unwrap();
    let config = config("http://127.0.0.1:1", tmp.path());
    let (engine, _) = engine(config);

    let scope = engine
        .scope(&Selection::default(), "main", None)
        .await
        .unwrap();

    assert_eq!(scope.timeout_seconds, 1_800);
}

#[tokio::test]
async fn catalog_discovery_does_not_record_model_messages_or_spend() {
    let mut server = server(vec![support::Reply::json(json!({
        "data": [{"id": "local/catalog-model"}]
    }))])
    .await;
    let tmp = tempdir().unwrap();
    let config = config(&server.url, tmp.path());
    let (engine, _) = engine(config);
    let session_path = engine.session.lock().await.path.clone();

    let models = engine
        .list_models(engine.config.read().await.providers["openrouter"].clone())
        .await
        .unwrap();

    assert_eq!(models[0].id, "local/catalog-model");
    let session = std::fs::read_to_string(session_path).unwrap();
    assert!(!session.contains("model_request"));
    assert!(!session.contains("spend"));
    assert!(server.requests.recv().await.is_some());
}
