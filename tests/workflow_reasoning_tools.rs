mod support;

use diet_soda::{
    config::{
        AgentConfig, Effort, McpConfig, McpTransport, ModelConfig, ProviderKind, ReasoningConfig,
    },
    engine::Selection,
    model::ToolCall,
    workflow::{self, McpReference, Step, Workflow},
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
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
async fn workflow_mcp_uuid_mismatch_is_rejected_before_provider_or_tool_execution() {
    let server = server(vec![answer("unexpected request")]).await;
    let tmp = tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.mcp_servers.insert(
        "configured-mcp".into(),
        McpConfig {
            uuid: "configured-mcp-id".into(),
            transport: McpTransport::Stdio {
                command: "never-started".into(),
                args: vec![],
                env: BTreeMap::new(),
            },
            enabled: true,
            hitl: false,
            timeout_seconds: 5,
        },
    );
    let (engine, _) = engine(config);
    let workflow = Workflow {
        title: "mcp uuid mismatch workflow".into(),
        author: "test".into(),
        steps: vec![Step {
            agent: None,
            model: "openai/gpt-4.1-mini".into(),
            prompt: "must not run".into(),
            mcps: vec![McpReference {
                name: "configured-mcp".into(),
                uuid: "wrong-mcp-id".into(),
                enabled: true,
            }],
            hitl: false,
        }],
    };

    let error = workflow::run(
        &engine,
        workflow,
        "input".into(),
        Selection::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();

    assert!(format!("{error:#}").contains("MCP reference mismatch or duplicate"));
    assert_eq!(server.count.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn workflow_mcp_matching_name_and_uuid_passes_validation() {
    let tmp = tempdir().unwrap();
    let mut config = config("http://127.0.0.1:1", tmp.path());
    config.mcp_servers.insert(
        "configured-mcp".into(),
        McpConfig {
            uuid: "configured-mcp-id".into(),
            transport: McpTransport::Stdio {
                command: "never-started".into(),
                args: vec![],
                env: BTreeMap::new(),
            },
            enabled: true,
            hitl: false,
            timeout_seconds: 5,
        },
    );
    let workflow = Workflow {
        title: "matching mcp workflow".into(),
        author: "test".into(),
        steps: vec![Step {
            agent: None,
            model: "openai/gpt-4.1-mini".into(),
            prompt: "validation only".into(),
            mcps: vec![McpReference {
                name: "configured-mcp".into(),
                uuid: "configured-mcp-id".into(),
                enabled: true,
            }],
            hitl: false,
        }],
    };

    workflow.validate(&config).unwrap();
}

#[test]
fn workflow_duplicate_mcp_reference_is_rejected() {
    let tmp = tempdir().unwrap();
    let mut config = config("http://127.0.0.1:1", tmp.path());
    config.mcp_servers.insert(
        "configured-mcp".into(),
        McpConfig {
            uuid: "configured-mcp-id".into(),
            transport: McpTransport::Stdio {
                command: "never-started".into(),
                args: vec![],
                env: BTreeMap::new(),
            },
            enabled: true,
            hitl: false,
            timeout_seconds: 5,
        },
    );
    let workflow = Workflow {
        title: "duplicate mcp workflow".into(),
        author: "test".into(),
        steps: vec![Step {
            agent: None,
            model: "openai/gpt-4.1-mini".into(),
            prompt: "validation only".into(),
            mcps: vec![
                McpReference {
                    name: "configured-mcp".into(),
                    uuid: "configured-mcp-id".into(),
                    enabled: true,
                },
                McpReference {
                    name: "configured-mcp".into(),
                    uuid: "configured-mcp-id".into(),
                    enabled: true,
                },
            ],
            hitl: false,
        }],
    };

    let error = workflow.validate(&config).unwrap_err();

    assert!(error
        .to_string()
        .contains("MCP reference mismatch or duplicate"));
}

#[tokio::test]
async fn workflow_steps_use_selected_agent_scopes_and_outer_agent_fallback() {
    let mut server = server(vec![answer("alpha result"), answer("beta result")]).await;
    let tmp = tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.models.insert(
        "alpha-model".into(),
        ModelConfig {
            model: "provider/alpha-model".into(),
            ..config.model.clone()
        },
    );
    config.models.insert(
        "beta-model".into(),
        ModelConfig {
            model: "provider/beta-model".into(),
            ..config.model.clone()
        },
    );
    config.mcp_servers.insert(
        "alpha-mcp".into(),
        McpConfig {
            uuid: "alpha-mcp-id".into(),
            transport: McpTransport::Stdio {
                command: if cfg!(windows) { "python" } else { "python3" }.into(),
                args: vec![format!(
                    "{}/tests/fixtures/mcp_server.py",
                    env!("CARGO_MANIFEST_DIR")
                )],
                env: BTreeMap::new(),
            },
            enabled: true,
            hitl: false,
            timeout_seconds: 5,
        },
    );
    config.mcp_servers.insert(
        "beta-mcp".into(),
        McpConfig {
            uuid: "beta-mcp-id".into(),
            transport: McpTransport::Stdio {
                command: if cfg!(windows) { "python" } else { "python3" }.into(),
                args: vec![format!(
                    "{}/tests/fixtures/mcp_server.py",
                    env!("CARGO_MANIFEST_DIR")
                )],
                env: BTreeMap::new(),
            },
            enabled: true,
            hitl: false,
            timeout_seconds: 5,
        },
    );
    config.agents.insert(
        "alpha".into(),
        AgentConfig {
            model: Some("alpha-model".into()),
            system_prompt: Some("ALPHA SYSTEM PROMPT".into()),
            tools: Some(vec!["read_file".into(), "write_file".into()]),
            mcp_servers: Some(vec!["alpha-mcp-id".into()]),
            can_edit: true,
            ..AgentConfig::default()
        },
    );
    config.agents.insert(
        "beta".into(),
        AgentConfig {
            model: Some("beta-model".into()),
            system_prompt: Some("BETA SYSTEM PROMPT".into()),
            tools: Some(vec!["load_skill".into(), "write_file".into()]),
            mcp_servers: Some(vec!["beta-mcp-id".into()]),
            can_edit: false,
            ..AgentConfig::default()
        },
    );
    let (engine, _) = engine(config);
    let workflow = Workflow {
        title: "agent scope workflow".into(),
        author: "test".into(),
        steps: vec![
            Step {
                agent: Some("alpha".into()),
                model: "alpha-model".into(),
                prompt: "alpha step".into(),
                mcps: vec![diet_soda::workflow::McpReference {
                    name: "alpha-mcp".into(),
                    uuid: "alpha-mcp-id".into(),
                    enabled: true,
                }],
                hitl: false,
            },
            Step {
                agent: None,
                model: "beta-model".into(),
                prompt: "beta step".into(),
                mcps: vec![diet_soda::workflow::McpReference {
                    name: "beta-mcp".into(),
                    uuid: "beta-mcp-id".into(),
                    enabled: true,
                }],
                hitl: false,
            },
        ],
    };

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        workflow::run(
            &engine,
            workflow,
            "workflow input".into(),
            Selection {
                agent: Some("beta".into()),
                ..Selection::default()
            },
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result, "beta result");

    let first: Value = serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let second: Value = serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    assert_eq!(first["model"], "provider/alpha-model");
    assert_eq!(second["model"], "provider/beta-model");
    assert!(first["messages"][0]["content"]
        .as_str()
        .unwrap()
        .contains("ALPHA SYSTEM PROMPT"));
    assert!(second["messages"][0]["content"]
        .as_str()
        .unwrap()
        .contains("BETA SYSTEM PROMPT"));
    let first_tools = first["tools"].to_string();
    let second_tools = second["tools"].to_string();
    assert!(first_tools.contains("read_file"));
    assert!(
        first_tools.contains("mcp_alpha-mcp__echo"),
        "first tools: {first_tools}"
    );
    assert!(first_tools.contains("write_file"));
    assert!(second_tools.contains("load_skill"));
    assert!(
        second_tools.contains("mcp_beta-mcp__echo"),
        "second tools: {second_tools}"
    );
    assert!(
        !second_tools.contains("mcp_alpha-mcp__echo"),
        "second tools: {second_tools}"
    );
    assert!(!second_tools.contains("write_file"));
}

#[tokio::test]
async fn workflow_unknown_step_agent_fails_before_step_one_request() {
    let server = server(vec![answer("unexpected request")]).await;
    let tmp = tempdir().unwrap();
    let (engine, _) = engine(config(&server.url, tmp.path()));
    let workflow = Workflow {
        title: "unknown agent workflow".into(),
        author: "test".into(),
        steps: vec![Step {
            agent: Some("does-not-exist".into()),
            model: "openai/gpt-4.1-mini".into(),
            prompt: "must not run".into(),
            mcps: vec![],
            hitl: false,
        }],
    };

    let error = workflow::run(
        &engine,
        workflow,
        "input".into(),
        Selection::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("unknown agent"));
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
