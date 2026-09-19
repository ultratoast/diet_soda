use diet_soda::{
    config::{Config, ToolConfig},
    engine::{intersect, Selection},
    model::{Message, Spend, ToolCall, Usage},
    session::Session,
    template, tools,
    workflow::Workflow,
};
use serde_json::json;
use std::io::Write;

#[test]
fn model_ids_preserve_slashes_and_support_provider_colon_and_alias() {
    let mut c = Config::default();
    c.models.insert("fast".into(), c.model.clone());
    assert_eq!(
        c.resolve_model("anthropic/claude-sonnet-4").unwrap().model,
        "anthropic/claude-sonnet-4"
    );
    assert_eq!(
        c.resolve_model("openrouter:anthropic/claude-sonnet-4")
            .unwrap()
            .provider,
        "openrouter"
    );
    assert_eq!(c.resolve_model("fast").unwrap().model, c.model.model);
    assert!(c.resolve_model("missing:model").is_err());
}
#[test]
fn custom_config_roundtrips_and_rejects_unknown_fields() {
    let value = json!({"description":"test","type":"command","command":"echo","args":["{{value}}"],"hitl":false,"destructive":false,"input_schema":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}});
    let tool: ToolConfig = serde_json::from_value(value).unwrap();
    let mut config = Config::default();
    config.tools.insert("echo".into(), tool);
    config.validate().unwrap();
    let serialized = serde_json::to_value(config).unwrap();
    serde_json::from_value::<Config>(serialized.clone())
        .unwrap()
        .validate()
        .unwrap();
    let mut wrong = serialized;
    wrong["unknown"] = json!(1);
    assert!(serde_json::from_value::<Config>(wrong).is_err());
    assert!(serde_json::from_value::<ToolConfig>(
        json!({"description":"test","type":"command","command":"echo","argz":[]})
    )
    .is_err());
}

#[test]
fn editable_named_sections_serialize_as_arrays_with_names() {
    let mut config = Config::default();
    config.models.insert("fast".into(), config.model.clone());
    config.agents.insert(
        "researcher".into(),
        serde_json::from_value(json!({"can_edit":false})).unwrap(),
    );
    config.modes.insert(
        "research".into(),
        serde_json::from_value(json!({"agent":"researcher"})).unwrap(),
    );
    config.tools.insert(
        "run_tests".into(),
        serde_json::from_value(json!({
            "type":"command", "description":"Run tests", "command":"cargo", "args":["test"]
        }))
        .unwrap(),
    );
    let value = serde_json::to_value(&config).unwrap();
    for section in ["models", "agents", "modes", "tools"] {
        assert!(value[section].is_array(), "{section} must be an array");
        assert!(
            value[section][0]["name"].is_string(),
            "{section} entries need names"
        );
    }
    let roundtrip: Config = serde_json::from_value(value).unwrap();
    assert!(roundtrip.models.contains_key("fast"));
    assert!(roundtrip.agents.contains_key("researcher"));
    assert!(roundtrip.modes.contains_key("research"));
    assert!(roundtrip.tools.contains_key("run_tests"));
}
#[test]
fn configuration_paths_are_relative_to_config_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.json");
    std::fs::write(&path, "{}").unwrap();
    let config = Config::load(&path).unwrap();
    let base = tmp.path().canonicalize().unwrap();
    assert_eq!(config.workspace, std::env::current_dir().unwrap());
    assert_eq!(config.sessions_dir, base.join("sessions"));
    assert_eq!(config.skills_dir, base.join("skills"));
    assert_eq!(config.workflows_dir, base.join("workflows"));
    assert_eq!(config.exports_dir, base.join("exports"));
    std::fs::write(&path, r#"{"workspace":"project","sessions_dir":"old-sessions","skills":{"directories":["extra-skills"]}}"#).unwrap();
    let config = Config::load(&path).unwrap();
    assert_eq!(config.workspace, base.join("project"));
    assert_eq!(config.sessions_dir, base.join("old-sessions"));
    assert_eq!(config.skills.directories, [base.join("extra-skills")]);
}

#[test]
fn loading_again_uses_current_disk_settings_instead_of_compiled_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.json");
    std::fs::write(
        &path,
        r#"{"theme":"haxx0r","model":{"model":"first/model"}}"#,
    )
    .unwrap();
    let before = Config::load(&path).unwrap();
    std::fs::write(&path, serde_json::to_vec(&json!({
        "theme":{"background":"#121212", "foreground":"#abcdef"},
        "model":{"model":"second/model"}, "system_prompt":"New prompt from disk",
        "agents":{"reviewer":{"prompt":"New agent instructions"}},
        "modes":{"review":{"agent":"reviewer"}},
        "mcp_servers":{"local":{"uuid":"local-id","transport":"stdio","command":"never-started","enabled":false}}
    })).unwrap()).unwrap();
    let after = Config::load(&path).unwrap();
    assert_eq!(before.model.model, "first/model");
    assert_eq!(after.model.model, "second/model");
    assert_eq!(after.theme.foreground, "#abcdef");
    assert_eq!(after.system_prompt, "New prompt from disk");
    assert_eq!(
        after.agents["reviewer"].prompt.as_deref(),
        Some("New agent instructions")
    );
    assert_eq!(after.modes["review"].agent.as_deref(), Some("reviewer"));
    assert!(!after.mcp_servers["local"].enabled);
}

#[test]
fn prompt_file_references_load_beside_the_config_and_directories_remain_config_relative() {
    let tmp = tempfile::tempdir().unwrap();
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(config_dir.join("workflows")).unwrap();
    std::fs::write(
        config_dir.join("AGENTS.md"),
        "Shared instructions from disk",
    )
    .unwrap();
    std::fs::write(config_dir.join("review.md"), "Review carefully.").unwrap();
    let config_path = config_dir.join("config.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "system_prompt":"./AGENTS.md",
            "agents":{"reviewer":{
                "prompt":"./review.md",
                "system_prompt":"Inline system addition",
                "modes":{"strict":{"prompt":"./AGENTS.md"}}
            }},
            "workflows_dir":"./workflows"
        })
        .to_string(),
    )
    .unwrap();
    let config = Config::load(&config_path).unwrap();
    assert_eq!(config.system_prompt, "Shared instructions from disk");
    assert_eq!(
        config.agents["reviewer"].prompt.as_deref(),
        Some("Review carefully.")
    );
    assert_eq!(
        config.agents["reviewer"].system_prompt.as_deref(),
        Some("Inline system addition")
    );
    assert_eq!(
        config.agents["reviewer"].modes["strict"].prompt.as_deref(),
        Some("Shared instructions from disk")
    );
    assert_eq!(config.workflows_dir, config.config_dir.join("workflows"));
    assert_eq!(config.config_dir, config_dir.canonicalize().unwrap());

    std::fs::write(&config_path, r#"{"system_prompt":"./missing.md"}"#).unwrap();
    let error = format!("{:#}", Config::load(&config_path).unwrap_err());
    assert!(error.contains("Loading system_prompt"));
    assert!(error.contains("missing.md"));
}
#[test]
fn exact_workflow_shape_is_enforced() {
    let value = json!({"title":"test","author":"user","steps":[{"model":"openai/gpt-4.1-mini","prompt":"{{input}} {{previous_result}}","mcps":[],"hitl":true}]});
    let workflow: Workflow = serde_json::from_value(value.clone()).unwrap();
    workflow.validate(&Config::default()).unwrap();
    let mut wrong = value.clone();
    wrong["steps"][0]["unsupported"] = json!("extra");
    assert!(serde_json::from_value::<Workflow>(wrong).is_err());
    let mut wrong = value;
    wrong["steps"][0].as_object_mut().unwrap().remove("hitl");
    assert!(serde_json::from_value::<Workflow>(wrong).is_err());
}
#[test]
fn template_substitution_preserves_json_types_and_never_recurses() {
    let vars = json!({"value":{"x":"\"quoted\""},"instruction":"{{missing}}","n":3});
    assert_eq!(
        template::render("A {{instruction}}", &vars).unwrap(),
        "A {{missing}}"
    );
    assert_eq!(
        template::render_json(&json!({"body":"{{value}}","n":"{{n}}"}), &vars).unwrap(),
        json!({"body":{"x":"\"quoted\""},"n":3})
    );
    assert!(template::render("{{unknown}}", &vars).is_err());
    assert!(template::render("{{unclosed", &vars).is_err());
}
#[test]
fn tool_arguments_follow_json_schema() {
    let spec = tools::builtins()
        .into_iter()
        .find(|s| s.name == "web_fetch")
        .unwrap();
    assert!(tools::validate_arguments(&spec, &json!({"url":"https://example.test"})).is_ok());
    assert!(tools::validate_arguments(&spec, &json!({"url":123})).is_err());
    assert!(tools::validate_arguments(&spec, &json!({"url":"x","extra":true})).is_err());
}
#[test]
fn session_recovery_is_append_only_and_repairs_interrupted_tools() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = Session::open(tmp.path(), Some("recover")).unwrap();
    let mut assistant = Message::new("assistant", "");
    assistant.tool_calls.push(ToolCall {
        id: "pending".into(),
        name: "example".into(),
        arguments: "{}".into(),
    });
    s.record_message("main", Message::new("user", "hello"))
        .unwrap();
    s.record_message("main", assistant).unwrap();
    s.usage(
        "child",
        &Usage {
            cost_microusd: Some(15),
            ..Usage::default()
        },
    )
    .unwrap();
    assert!(Session::open(tmp.path(), Some("recover")).is_err());
    let path = s.path.clone();
    drop(s);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"type\":")
        .unwrap();
    let original = std::fs::read(&path).unwrap();
    let s = Session::open(tmp.path(), Some("recover")).unwrap();
    assert_eq!(s.spend.microusd, 15);
    assert_eq!(
        s.messages.last().unwrap().tool_call_id.as_deref(),
        Some("pending")
    );
    drop(s);
    assert!(std::fs::read(&path).unwrap().starts_with(&original));
    let s = Session::open(tmp.path(), Some("recover")).unwrap();
    assert_eq!(s.messages.len(), 3);
}
#[test]
fn persisted_records_redact_resolved_secrets() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = Session::open(tmp.path(), None).unwrap();
    s.add_redactions(vec!["private-token".into(), "token-with-\"quote".into()]);
    s.record_message("main", Message::new("tool", "Bearer private-token"))
        .unwrap();
    s.append("plugin", "main", json!({"nested":["private-token"]}))
        .unwrap();
    s.record_message(
        "main",
        Message::new("tool", json!({"value":"token-with-\"quote"}).to_string()),
    )
    .unwrap();
    let text = std::fs::read_to_string(&s.path).unwrap();
    assert!(!text.contains("private-token"));
    assert!(text.contains("[REDACTED]"));
    assert!(!text.contains("token-with-"));
}

#[test]
fn interrupted_utf8_tail_can_be_recovered_repeatedly() {
    let tmp = tempfile::tempdir().unwrap();
    let s = Session::open(tmp.path(), Some("utf8")).unwrap();
    let path = s.path.clone();
    drop(s);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"data\":\"\xf0\x9f")
        .unwrap();
    drop(Session::open(tmp.path(), Some("utf8")).unwrap());
    Session::open(tmp.path(), Some("utf8")).unwrap();
}
#[test]
fn spend_tracks_unknown_and_estimated_separately() {
    let mut spend = Spend::default();
    for _ in 0..1000 {
        spend.add(&Usage {
            cost_microusd: Some(1),
            estimated: true,
            ..Usage::default()
        });
    }
    spend.add(&Usage::default());
    assert_eq!(spend.microusd, 1000);
    assert_eq!(spend.display(), "~$0.0010 + unknown");
}
#[test]
fn scope_intersection_never_widens_parent_permissions() {
    assert_eq!(
        intersect(
            Some(vec!["read".into()]),
            Some(vec!["read".into(), "write".into()])
        ),
        Some(vec!["read".into()])
    );
    assert_eq!(intersect(None, Some(vec![])), Some(vec![]));
    assert_eq!(Selection::default().agent, None);
}
#[test]
fn html_extraction_omits_scripts_and_navigation() {
    let (title,text) = tools::extract_html("<title>Example</title><nav>menu</nav><main><h1>Hello</h1><script>evil()</script><p>Read &amp; learn</p></main><footer>noise</footer>");
    assert_eq!(title, "Example");
    assert!(text.contains("Read & learn"));
    assert!(!text.contains("evil"));
    assert!(!text.contains("menu"));
}
#[test]
fn workspace_paths_reject_parent_and_symlink_escapes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(tmp.path().join("secret"), "secret").unwrap();
    assert!(tools::workspace_path(&root, "../secret", false).is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(tmp.path().join("secret"), root.join("link")).unwrap();
        assert!(tools::workspace_path(&root, "link", true).is_err());
    }
}

#[test]
fn outside_reads_are_detected_for_approval_without_widening_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("inside.txt"), "inside").unwrap();
    std::fs::write(tmp.path().join("secret.txt"), "secret").unwrap();
    let config = Config {
        workspace: root,
        ..Config::default()
    };
    assert!(!tools::read_requires_approval(&config, "inside.txt").unwrap());
    assert!(tools::read_requires_approval(&config, "../secret.txt").unwrap());
}

#[test]
fn outside_shell_arguments_require_approval_but_inside_ones_do_not() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let config = Config {
        workspace: root.clone(),
        ..Config::default()
    };
    assert!(!tools::outside_path_args(&config, &["test".into(), "--locked".into()]).unwrap());
    assert!(tools::outside_path_args(&config, &["../secret".into()]).unwrap());
    assert!(tools::outside_path_args(
        &config,
        &[tmp.path().join("x").to_string_lossy().into_owned()]
    )
    .unwrap());
    assert!(!tools::shell_requires_approval(&config, "cargo", &["test".into()], false).unwrap());
    assert!(tools::shell_requires_approval(&config, "ls", &["/etc".into()], false).unwrap());
    assert!(
        tools::shell_requires_approval(&config, "rm", &["-rf".into(), "build".into()], false)
            .unwrap()
    );
    // The standing grant covers non-destructive outside work but never
    // destructive commands.
    let outside_arg = tmp.path().join("x").to_string_lossy().into_owned();
    assert!(!tools::shell_requires_approval(
        &config,
        "cat",
        std::slice::from_ref(&outside_arg),
        true
    )
    .unwrap());
    assert!(
        tools::shell_requires_approval(&config, "rm", &["-rf".into(), outside_arg], true).unwrap()
    );
    assert!(tools::command_cwd_outside(&config, tmp.path()));
    assert!(!tools::command_cwd_outside(&config, &root));
}

#[test]
fn tool_call_summaries_stay_human_readable() {
    assert_eq!(
        tools::describe_call("read_file", &json!({"path":"src/main.rs"})),
        "Read `src/main.rs`"
    );
    assert_eq!(
        tools::describe_call(
            "shell",
            &json!({"command":"cargo","args":["test","--locked"]})
        ),
        "Run `cargo test --locked`"
    );
    assert_eq!(
        tools::describe_call("write_file", &json!({"path":"a.txt","content":"hello"})),
        "Write 5 bytes to `a.txt`"
    );
    assert!(tools::describe_call("custom", &json!({"a":1})).contains("\"a\""));
}

#[test]
fn session_restores_the_latest_main_context_size() {
    let tmp = tempfile::tempdir().unwrap();
    let mut session = Session::open(tmp.path(), None).unwrap();
    session
        .usage(
            "main",
            &Usage {
                input_tokens: 100,
                output_tokens: 20,
                ..Usage::default()
            },
        )
        .unwrap();
    session
        .usage(
            "subagent:x",
            &Usage {
                input_tokens: 999,
                output_tokens: 1,
                ..Usage::default()
            },
        )
        .unwrap();
    let id = session.id.clone();
    drop(session);
    let reopened = Session::open(tmp.path(), Some(&id)).unwrap();
    assert_eq!(reopened.context_tokens, 120);
    assert_eq!(reopened.spend.input_tokens, 1099);
}

#[test]
fn export_has_the_requested_timestamp_and_keeps_child_contexts_and_redaction() {
    use chrono::TimeZone;
    let tmp = tempfile::tempdir().unwrap();
    let mut session = Session::open(tmp.path(), None).unwrap();
    session.add_redactions(vec!["export-secret".into()]);
    session
        .record_message("main", Message::new("user", "Question"))
        .unwrap();
    session
        .record_message(
            "subagent:worker",
            Message::new("assistant", "Result export-secret"),
        )
        .unwrap();
    let timestamp = chrono::Local
        .with_ymd_and_hms(2026, 9, 18, 16, 5, 2)
        .unwrap();
    let directory = tmp.path().join("exports");
    let first = session.export_at(&directory, timestamp).unwrap();
    let second = session.export_at(&directory, timestamp).unwrap();
    assert_ne!(first, second);
    #[cfg(not(windows))]
    assert_eq!(first.file_name().unwrap(), "09:18:2026-16:05:02.txt");
    let text = std::fs::read_to_string(first).unwrap();
    assert!(text.starts_with("09:18:2026-16:05:02"));
    assert!(text.contains("subagent:worker"));
    assert!(text.contains("[REDACTED]"));
    assert!(!text.contains("export-secret"));
}
