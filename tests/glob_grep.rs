use diet_soda::{
    config::Config,
    tools::{self, BUILTIN_NAMES},
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

fn config_for(workspace: &std::path::Path) -> Config {
    Config {
        workspace: workspace.into(),
        ..Config::default()
    }
}

async fn builtin(name: &str, args: Value, config: &Config) -> anyhow::Result<Value> {
    tools::builtin(name, &args, config, &CancellationToken::new(), false).await
}

#[test]
fn glob_and_grep_are_registered_with_closed_schemas() {
    let specs = tools::builtins();
    for name in ["glob", "grep"] {
        let spec = specs.iter().find(|spec| spec.name == name).unwrap();
        assert_eq!(spec.input_schema["additionalProperties"], false);
        assert_eq!(spec.input_schema["required"], json!(["pattern"]));
        assert!(BUILTIN_NAMES.contains(&name));
    }
}

#[tokio::test]
async fn glob_returns_sorted_workspace_matches_and_reports_truncation() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(workspace.path().join("src/a.rs"), "a").unwrap();
    std::fs::write(workspace.path().join("src/b.rs"), "b").unwrap();
    std::fs::write(workspace.path().join("readme.md"), "readme").unwrap();
    let config = config_for(workspace.path());

    let result = builtin("glob", json!({"pattern":"**/*.rs"}), &config)
        .await
        .unwrap();
    let paths: Vec<&str> = result["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|matched| matched["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["src/a.rs", "src/b.rs"]);
    assert_eq!(result["count"], 2);
    assert_eq!(result["truncated"], false);

    let limited = builtin("glob", json!({"pattern":"**/*.rs", "limit":1}), &config)
        .await
        .unwrap();
    assert_eq!(limited["matches"].as_array().unwrap().len(), 1);
    assert_eq!(limited["count"], 2);
    assert_eq!(limited["truncated"], true);
}

#[tokio::test]
async fn glob_rejects_patterns_that_can_escape_workspace() {
    let workspace = tempfile::tempdir().unwrap();
    let config = config_for(workspace.path());

    assert!(builtin("glob", json!({"pattern":"../*"}), &config)
        .await
        .is_err());
    assert!(builtin("glob", json!({"pattern":"/etc/*"}), &config)
        .await
        .is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn glob_does_not_follow_symlinks() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.rs"), "secret").unwrap();
    std::os::unix::fs::symlink(outside.path(), workspace.path().join("link")).unwrap();
    let config = config_for(workspace.path());

    let result = builtin("glob", json!({"pattern":"**/*.rs"}), &config)
        .await
        .unwrap();
    let paths: Vec<&str> = result["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|matched| matched["path"].as_str().unwrap())
        .collect();
    assert!(!paths.iter().any(|path| path.starts_with("link/")));
}

#[tokio::test]
async fn grep_returns_literal_match_location_and_supports_case_insensitivity() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("lib.rs"),
        "fn alpha() {}\nlet beta = 1;\n",
    )
    .unwrap();
    let config = config_for(workspace.path());

    let result = builtin("grep", json!({"pattern":"beta"}), &config)
        .await
        .unwrap();
    let matches = result["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0]["file"], "lib.rs");
    assert_eq!(matches[0]["line"], 2);
    assert_eq!(matches[0]["column"], 5);

    let insensitive = builtin(
        "grep",
        json!({"pattern":"BETA", "ignore_case":true}),
        &config,
    )
    .await
    .unwrap();
    assert_eq!(insensitive["matches"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn grep_rejects_paths_outside_the_workspace() {
    let workspace = tempfile::tempdir().unwrap();
    let config = config_for(workspace.path());

    assert!(
        builtin("grep", json!({"path":"/etc", "pattern":"x"}), &config)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn grep_glob_filter_limits_matches_to_selected_files() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("a.rs"), "needle\n").unwrap();
    std::fs::write(workspace.path().join("b.txt"), "needle\n").unwrap();
    let config = config_for(workspace.path());

    let result = builtin("grep", json!({"pattern":"needle", "glob":"*.rs"}), &config)
        .await
        .unwrap();
    let matches = result["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1);
    assert!(matches[0]["file"].as_str().unwrap().ends_with(".rs"));
}
