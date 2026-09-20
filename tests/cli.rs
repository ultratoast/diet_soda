use std::{
    process::Command,
    sync::{Arc, Barrier},
    thread,
};

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
    // The implicit default path auto-initializes on first launch when the
    // config is missing. Status goes to stderr so scripted `--prompt` output
    // stays clean.
    let auto = run(&["--validate-config"]);
    assert!(
        auto.status.success(),
        "{}",
        String::from_utf8_lossy(&auto.stderr)
    );
    // The auto-init announcement must not be on stdout, even though
    // `--validate-config` itself prints a confirmation there.
    assert!(
        !String::from_utf8_lossy(&auto.stdout).contains("Initialized default configuration at"),
        "auto-init status leaked to stdout: {:?}",
        String::from_utf8_lossy(&auto.stdout)
    );
    let auto_stderr = String::from_utf8_lossy(&auto.stderr);
    assert!(
        auto_stderr.contains("Initialized default configuration at"),
        "auto-init should announce on stderr: {auto_stderr}"
    );
    assert!(path.exists());
    assert!(directory.join("workflows").is_dir());
    assert!(directory.join("skills").is_dir());
    assert!(directory.join("prompts").is_dir());
    assert!(directory.join("prompts/plan.md").is_file());
    assert!(directory.join("AGENTS.md").is_file());
    assert!(directory.join("theme.json").is_file());
    assert!(directory.join("bash-permissions.json").is_file());
    assert!(directory.join("CONFIGURATION.md").is_file());
    assert!(directory.join("QUEUE_AND_ACCESS.md").is_file());
    let original = std::fs::read(&path).unwrap();
    let mut document: serde_json::Value = serde_json::from_slice(&original).unwrap();
    assert_eq!(document["workspace"], "");
    assert_eq!(document["workflows_dir"], "workflows");
    assert_eq!(document["skills_dir"], "skills");
    // `--init` refuses to overwrite an existing config.
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
fn explicit_missing_config_fails_but_implicit_default_auto_initializes() {
    let binary = env!("CARGO_BIN_EXE_diet_soda");
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let workspace = tmp.path().join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
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
    // Explicit missing --config must still fail.
    let missing = run(&[
        "--config",
        "/definitely/never/exists.json",
        "--validate-config",
    ]);
    assert!(!missing.status.success());
    let missing_text = String::from_utf8_lossy(&missing.stderr);
    assert!(
        missing_text.contains("/definitely/never/exists.json"),
        "stderr did not reference the explicit path: {missing_text}"
    );
    assert!(!path.exists());
    // Implicit default path auto-inits and proceeds.
    let auto = run(&["--validate-config"]);
    assert!(
        auto.status.success(),
        "{}",
        String::from_utf8_lossy(&auto.stderr)
    );
    assert!(path.exists());
}

#[test]
#[cfg(unix)]
fn auto_init_status_goes_to_stderr_not_stdout() {
    let binary = env!("CARGO_BIN_EXE_diet_soda");
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let output = Command::new(binary)
        .arg("--validate-config")
        .env("HOME", &home)
        .env(
            "XDG_CONFIG_HOME",
            tmp.path().join("not-the-requested-location"),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // The auto-init announcement must not appear on stdout; validate-config
    // confirmation does, so the check is on the absence of the announcement.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("Initialized default configuration at"),
        "auto-init status leaked to stdout: {stdout:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Initialized default configuration at"),
        "stderr should report auto-init: {stderr}"
    );
}

#[test]
#[cfg(unix)]
fn auto_init_does_not_overwrite_existing_config_or_companion_files() {
    let binary = env!("CARGO_BIN_EXE_diet_soda");
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let directory = home.join(".config/diet_soda");
    std::fs::create_dir_all(&directory).unwrap();
    // Pre-populate the tree as if a prior run already published everything.
    let config_path = directory.join("config.json");
    let original_config = serde_json::json!({
        "providers": {"openrouter": {
            "kind":"openrouter", "base_url":"http://127.0.0.1:1",
            "api_key_env":null, "timeout_seconds":1
        }},
        "model":{"provider":"openrouter","model":"openai/gpt-4.1-mini","max_tokens":4096},
        "agents":[{"name":"plan","prompt":"Stay.","default":true,"hidden":false}],
        "system_prompt":"Hi"
    });
    std::fs::write(&config_path, serde_json::to_vec(&original_config).unwrap()).unwrap();
    let agents_path = directory.join("AGENTS.md");
    let original_agents = "# Custom\n";
    std::fs::write(&agents_path, original_agents).unwrap();
    let output = Command::new(binary)
        .arg("--validate-config")
        .env("HOME", &home)
        .env(
            "XDG_CONFIG_HOME",
            tmp.path().join("not-the-requested-location"),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Existing config and companion file untouched.
    assert_eq!(
        std::fs::read(&config_path).unwrap(),
        serde_json::to_vec(&original_config).unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(&agents_path).unwrap(),
        original_agents
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("Initialized default configuration at"),
        "auto-init should not announce when the tree already exists: {stderr}"
    );
}

#[test]
#[cfg(unix)]
fn init_short_circuits_auto_initialization() {
    // `--init` must run even when no implicit default is needed; verify by
    // invoking `--init` against a fresh, isolated home and then a second time
    // to confirm no-overwrite behavior, without ever reaching the auto-init
    // branch.
    let binary = env!("CARGO_BIN_EXE_diet_soda");
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let run = |args: &[&str]| {
        Command::new(binary)
            .args(args)
            .env("HOME", &home)
            .env(
                "XDG_CONFIG_HOME",
                tmp.path().join("not-the-requested-location"),
            )
            .output()
            .unwrap()
    };
    let path = home.join(".config/diet_soda/config.json");
    let first = run(&["--init"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    // `--init` writes its success status to stdout.
    let stdout = String::from_utf8_lossy(&first.stdout);
    assert!(
        stdout.contains("Created"),
        "stdout should announce init: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&first.stderr);
    assert!(
        !stderr.contains("Initialized default configuration at"),
        "--init must not invoke the auto-init branch: {stderr}"
    );
    // Second run: refuses to overwrite and still surfaces an error.
    let second = run(&["--init"]);
    assert!(!second.status.success());
    assert!(path.exists());
}

#[test]
#[cfg(unix)]
fn concurrent_first_run_auto_initializes_exactly_once() {
    // Multiple processes racing on a fresh default config must end with a
    // complete tree, a valid config, and no partial files. The lock file in
    // `init` serializes writers; companion files use `create_new(true)` so
    // any concurrent loser cannot clobber an already-published file.
    let binary = env!("CARGO_BIN_EXE_diet_soda");
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let workers = 6;
    let barrier = Arc::new(Barrier::new(workers));
    let mut handles = Vec::new();
    for _ in 0..workers {
        let binary = binary.to_string();
        let home = home.clone();
        let xdg = tmp.path().join("not-the-requested-location");
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            Command::new(&binary)
                .arg("--validate-config")
                .env("HOME", &home)
                .env("XDG_CONFIG_HOME", &xdg)
                .output()
                .unwrap()
        }));
    }
    let mut successes = 0;
    let mut auto_announced = 0;
    for handle in handles {
        let output = handle.join().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        successes += 1;
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("Initialized default configuration at") {
            auto_announced += 1;
        }
    }
    assert_eq!(successes, workers);
    // Exactly one process announces: the lock serializes writers and both
    // branches of `auto_initialize` short-circuit when the published file is
    // already on disk. The early `path.exists()` check returns `Ok(())`
    // without writing to `sink`, and the post-`initialize` error path
    // (`path exists` from `hard_link`'s `AlreadyExists` or the upfront
    // bail) also returns `Ok(())` without announcing. `Config::load` then
    // finds the freshly-published file for every other process, so they all
    // succeed.
    assert_eq!(
        auto_announced, 1,
        "{workers} processes ran, {auto_announced} announced"
    );
    let directory = home.join(".config/diet_soda");
    // Config is valid JSON once.
    let path = directory.join("config.json");
    let document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(document["workflows_dir"], "workflows");
    // All companion files are present and parse as expected.
    assert!(directory.join("AGENTS.md").is_file());
    assert!(serde_json::from_slice::<serde_json::Value>(
        &std::fs::read(directory.join("theme.json")).unwrap()
    )
    .is_ok());
    assert!(directory.join("bash-permissions.json").is_file());
    assert!(directory.join("CONFIGURATION.md").is_file());
    assert!(directory.join("QUEUE_AND_ACCESS.md").is_file());
    for name in [
        "plan.md",
        "build.md",
        "code-review.md",
        "plan-review.md",
        "debug.md",
        "research.md",
        "explore.md",
        "test-runner.md",
        "test-writer.md",
        "general-purpose.md",
        "converse.md",
        "elephant.md",
    ] {
        let prompt_path = directory.join("prompts").join(name);
        assert!(prompt_path.is_file(), "missing prompt {name}");
        // Companion files were never truncated or written partial.
        let body = std::fs::read(&prompt_path).unwrap();
        assert!(!body.is_empty());
        assert!(body.ends_with(b"\n"));
    }
}

#[test]
#[cfg(unix)]
fn auto_init_heals_partial_tree_and_preserves_seeded_companion_files() {
    // Crash-recovery / self-healing regression: a partial tree left behind
    // by a mid-init crash (or by a user who placed companion files before
    // the first launch) must not stop auto-init from publishing
    // `config.json`, and the seeded companion files must be left exactly as
    // they were. `create_new(true)` is what guarantees the latter.
    let binary = env!("CARGO_BIN_EXE_diet_soda");
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let directory = home.join(".config/diet_soda");
    std::fs::create_dir_all(directory.join("prompts")).unwrap();
    // Seed the tree with a custom `AGENTS.md` and a custom prompt file.
    // Critically, `config.json` is absent so auto-init has work to do.
    let custom_agents = "# Project-wide agents\nCustom overrides here.\n";
    let agents_path = directory.join("AGENTS.md");
    std::fs::write(&agents_path, custom_agents).unwrap();
    let custom_prompt = "# Custom plan prompt\nDo this carefully.\n";
    let prompt_path = directory.join("prompts").join("plan.md");
    std::fs::write(&prompt_path, custom_prompt).unwrap();
    let output = Command::new(binary)
        .arg("--validate-config")
        .env("HOME", &home)
        .env(
            "XDG_CONFIG_HOME",
            tmp.path().join("not-the-requested-location"),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Initialized default configuration at"),
        "expected auto-init announcement on stderr: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("Initialized default configuration at"),
        "auto-init leaked to stdout: {stdout:?}"
    );
    // The implicit default config path was self-healed: `config.json` now
    // exists and parses as valid JSON.
    let config_path = directory.join("config.json");
    let config_bytes = std::fs::read(&config_path).unwrap();
    let document: serde_json::Value = serde_json::from_slice(&config_bytes).unwrap();
    assert_eq!(document["workflows_dir"], "workflows");
    assert_eq!(document["skills_dir"], "skills");
    // The seeded companion files were left exactly as they were on disk.
    assert_eq!(
        std::fs::read_to_string(&agents_path).unwrap(),
        custom_agents
    );
    assert_eq!(
        std::fs::read_to_string(&prompt_path).unwrap(),
        custom_prompt
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
