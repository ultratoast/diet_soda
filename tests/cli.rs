use std::process::Command;

#[test]
fn cli_initializes_validates_examples_and_refuses_overwrite() {
    let binary = env!("CARGO_BIN_EXE_diet_soda");
    let version = Command::new(binary).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap().trim(),
        concat!("diet_soda ", env!("CARGO_PKG_VERSION"))
    );
    let help = Command::new(binary).arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("Usage: diet_soda"));
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("nested/config.json");
    let result = Command::new(binary)
        .args(["--init", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!Command::new(binary)
        .args(["--init", "--config"])
        .arg(&config)
        .output()
        .unwrap()
        .status
        .success());
    assert!(Command::new(binary)
        .args(["--validate-config", "--config"])
        .arg(&config)
        .output()
        .unwrap()
        .status
        .success());
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for name in ["research-report", "mcp-demo"] {
        let output = Command::new(binary)
            .args(["--config"])
            .arg(root.join("examples/config.json"))
            .arg("--validate-workflow")
            .arg(root.join(format!("examples/workflows/{name}.json")))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
#[cfg(unix)]
fn default_config_is_user_scoped_and_every_start_reads_the_latest_files() {
    let binary = env!("CARGO_BIN_EXE_diet_soda");
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let workspace = tmp.path().join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    // A project-local file must not silently shadow the user's shared settings.
    std::fs::write(workspace.join("config.json"), "not valid json").unwrap();
    let run = |args: &[&str]| {
        Command::new(binary)
            .args(args)
            .env("HOME", &home)
            .env(
                "XDG_CONFIG_HOME",
                tmp.path().join("not-the-requested-location"),
            )
            .current_dir(&workspace)
            .output()
            .unwrap()
    };
    let directory = home.join(".config/diet_soda");
    let path = directory.join("config.json");
    let missing = run(&["--validate-config"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains(".config/diet_soda/config.json"));
    assert!(run(&["--init"]).status.success());
    assert!(directory.join("workflows").is_dir());
    assert!(directory.join("skills").is_dir());
    let original = std::fs::read(&path).unwrap();
    let mut document: serde_json::Value = serde_json::from_slice(&original).unwrap();
    assert_eq!(document["workspace"], "");
    assert_eq!(document["workflows_dir"], "workflows");
    assert_eq!(document["skills_dir"], "skills");
    assert!(run(&["--validate-config"]).status.success());
    assert!(!run(&["--init"]).status.success());
    assert_eq!(std::fs::read(&path).unwrap(), original);

    let workflow = directory.join("workflows/report.json");
    let mut definition = serde_json::json!({"title":"Original report", "author":"test", "steps":[
        {"model":"openai/gpt-4.1-mini","prompt":"Write a report","mcps":[],"hitl":false}
    ]});
    std::fs::write(&workflow, definition.to_string()).unwrap();
    let first = run(&["--validate-workflow", "report"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(String::from_utf8_lossy(&first.stdout).contains("Original report"));
    definition["title"] = serde_json::json!("Edited report");
    std::fs::write(&workflow, definition.to_string()).unwrap();
    let second = run(&["--validate-workflow", "report"]);
    assert!(second.status.success());
    assert!(String::from_utf8_lossy(&second.stdout).contains("Edited report"));

    document["theme"] = serde_json::json!("invalid-theme-from-disk");
    std::fs::write(&path, document.to_string()).unwrap();
    assert!(!run(&["--validate-config"]).status.success());
    document["theme"] = serde_json::json!("diet_soda");
    std::fs::write(&path, document.to_string()).unwrap();
    assert!(run(&["--validate-config"]).status.success());
    assert!(run(&[
        "--config",
        "~/.config/diet_soda/config.json",
        "--validate-config"
    ])
    .status
    .success());
    assert_eq!(
        std::fs::read_to_string(workspace.join("config.json")).unwrap(),
        "not valid json"
    );
}

#[test]
#[cfg(unix)]
fn tui_pseudo_terminal_handles_modes_model_picker_and_restores_terminal() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.json");
    std::fs::write(
        &config,
        serde_json::json!({
            "providers": {"openrouter": {
                "kind":"openrouter", "base_url":"http://127.0.0.1:1",
                "api_key_env":null, "timeout_seconds":1
            }},
            "models": {"browse-target": {"model":"vendor/dialog-model"}},
            "agents": [{"name":"plan","hidden":false,"prompt":"Planning."}],
            "mcp_servers": {"browser": {"uuid":"test-browser", "transport":"stdio", "command":"never-started", "enabled":false}}
        })
        .to_string(),
    )
    .unwrap();
    let output = Command::new("python3")
        .arg(format!(
            "{}/tests/fixtures/tui_smoke.py",
            env!("CARGO_MANIFEST_DIR")
        ))
        .arg(env!("CARGO_BIN_EXE_diet_soda"))
        .arg(config)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
