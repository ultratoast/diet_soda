#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{path::Path, process::Command};
#[cfg(unix)]
use std::{
    sync::{Arc, Barrier},
    thread,
};

mod support;
use support::{answer, server, tool_call};

fn write_cli_config(path: &Path, workflows_dir: &str, skills_dir: &str) {
    std::fs::write(
        path,
        serde_json::json!({
            "workflows_dir": workflows_dir,
            "skills_dir": skills_dir,
            "providers": {"openrouter": {
                "kind": "openrouter",
                "base_url": "http://127.0.0.1:1",
                "api_key_env": null,
                "timeout_seconds": 1
            }},
            "model": {"provider": "openrouter", "model": "local/model"}
        })
        .to_string(),
    )
    .unwrap();
}

fn skill_text(name: &str) -> String {
    format!("---\nname: {name}\ndescription: A local test skill\n---\nUse this skill.")
}

fn output_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn headless_config(path: &Path, base_url: &str, extra: serde_json::Value) {
    let mut config = serde_json::json!({
        "workspace": path.parent().unwrap(),
        "sessions_dir": "sessions",
        "workflows_dir": "workflows",
        "skills_dir": "skills",
        "providers": {"openrouter": {
            "kind": "openrouter",
            "base_url": base_url,
            "api_key_env": null,
            "timeout_seconds": 5
        }},
        "model": {
            "provider": "openrouter",
            "model": "default-model",
            "max_tokens": 128,
            "reasoning": {"supported_efforts": ["low", "high"]}
        },
        "system_prompt": "test system"
    });
    if let (Some(target), Some(source)) = (config.as_object_mut(), extra.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
    std::fs::write(path, config.to_string()).unwrap();
}

fn run_cli(config: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_diet_soda"))
        .arg("--config")
        .arg(config)
        .args(args)
        .output()
        .unwrap()
}

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
    let installed_workflow = config
        .parent()
        .unwrap()
        .join("workflows/elephants_and_goldfish.json");
    assert_eq!(
        std::fs::read(&installed_workflow).unwrap(),
        include_bytes!("../examples/workflows/elephants_and_goldfish.json")
    );
    let workflow_validation = Command::new(binary)
        .args(["--config"])
        .arg(&config)
        .arg("--validate-workflow")
        .arg(&installed_workflow)
        .output()
        .unwrap();
    assert!(
        workflow_validation.status.success(),
        "{}",
        String::from_utf8_lossy(&workflow_validation.stderr)
    );
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
    let installed_workflow = directory.join("workflows/elephants_and_goldfish.json");
    assert_eq!(
        std::fs::read(&installed_workflow).unwrap(),
        include_bytes!("../examples/workflows/elephants_and_goldfish.json")
    );
    let workflow_validation = run(&["--validate-workflow", "elephants_and_goldfish"]);
    assert!(
        workflow_validation.status.success(),
        "{}",
        String::from_utf8_lossy(&workflow_validation.stderr)
    );
    let original = std::fs::read(&path).unwrap();
    let mut document: serde_json::Value = serde_json::from_slice(&original).unwrap();
    assert_eq!(
        document["builtin_timeouts"],
        serde_json::json!({"shell_timeout_seconds":120,"gh_timeout_seconds":120})
    );
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
    let workflow_path = directory.join("workflows/elephants_and_goldfish.json");
    assert_eq!(
        std::fs::read(&workflow_path).unwrap(),
        include_bytes!("../examples/workflows/elephants_and_goldfish.json")
    );
    assert_eq!(
        workflow_path.metadata().unwrap().permissions().mode() & 0o077,
        0
    );
    let workflow_validation = Command::new(binary)
        .args(["--validate-workflow", "elephants_and_goldfish"])
        .env("HOME", &home)
        .env(
            "XDG_CONFIG_HOME",
            tmp.path().join("not-the-requested-location"),
        )
        .output()
        .unwrap();
    assert!(
        workflow_validation.status.success(),
        "{}",
        String::from_utf8_lossy(&workflow_validation.stderr)
    );
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
    std::fs::create_dir_all(directory.join("workflows")).unwrap();
    // Seed the tree with a custom `AGENTS.md` and a custom prompt file.
    // Critically, `config.json` is absent so auto-init has work to do.
    let custom_agents = "# Project-wide agents\nCustom overrides here.\n";
    let agents_path = directory.join("AGENTS.md");
    std::fs::write(&agents_path, custom_agents).unwrap();
    let custom_prompt = "# Custom plan prompt\nDo this carefully.\n";
    let prompt_path = directory.join("prompts").join("plan.md");
    std::fs::write(&prompt_path, custom_prompt).unwrap();
    let custom_workflow = serde_json::json!({
        "title": "Customized workflow",
        "author": "project",
        "steps": [{
            "model": "openai/gpt-4.1-mini",
            "prompt": "Do the customized work for {{input}}.",
            "mcps": [],
            "hitl": false
        }]
    });
    let workflow_path = directory.join("workflows/customized.json");
    let custom_workflow_bytes = serde_json::to_vec(&custom_workflow).unwrap();
    std::fs::write(&workflow_path, &custom_workflow_bytes).unwrap();
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
    assert_eq!(
        std::fs::read(&workflow_path).unwrap(),
        custom_workflow_bytes
    );
    let workflow_validation = Command::new(binary)
        .args(["--validate-workflow"])
        .arg(&workflow_path)
        .env("HOME", &home)
        .env(
            "XDG_CONFIG_HOME",
            tmp.path().join("not-the-requested-location"),
        )
        .output()
        .unwrap();
    assert!(
        workflow_validation.status.success(),
        "{}",
        String::from_utf8_lossy(&workflow_validation.stderr)
    );
    let installed_workflow = directory.join("workflows/elephants_and_goldfish.json");
    assert_eq!(
        std::fs::read(&installed_workflow).unwrap(),
        include_bytes!("../examples/workflows/elephants_and_goldfish.json")
    );
    let installed_validation = Command::new(binary)
        .args(["--validate-workflow"])
        .arg(&installed_workflow)
        .env("HOME", &home)
        .env(
            "XDG_CONFIG_HOME",
            tmp.path().join("not-the-requested-location"),
        )
        .output()
        .unwrap();
    assert!(
        installed_validation.status.success(),
        "{}",
        String::from_utf8_lossy(&installed_validation.stderr)
    );
}

#[test]
#[cfg(unix)]
fn auto_init_creates_private_tree_and_preserves_existing_broad_modes() {
    let binary = env!("CARGO_BIN_EXE_diet_soda");
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let directory = home.join(".config/diet_soda");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
    let seeded = directory.join("AGENTS.md");
    std::fs::write(&seeded, "seeded\n").unwrap();
    std::fs::set_permissions(&seeded, std::fs::Permissions::from_mode(0o644)).unwrap();

    let output = Command::new(binary)
        .arg("--validate-config")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", tmp.path().join("ignored"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let private_file = |path: std::path::PathBuf| {
        assert_eq!(
            path.metadata().unwrap().permissions().mode() & 0o077,
            0,
            "{}",
            path.display()
        );
    };
    let private_dir = |path: std::path::PathBuf| {
        assert_eq!(
            path.metadata().unwrap().permissions().mode() & 0o077,
            0,
            "{}",
            path.display()
        );
    };
    private_file(directory.join("config.json"));
    private_file(directory.join(".diet_soda-init.lock"));
    for name in [
        "AGENTS.md",
        "theme.json",
        "bash-permissions.json",
        "CONFIGURATION.md",
        "QUEUE_AND_ACCESS.md",
    ] {
        if name != "AGENTS.md" {
            private_file(directory.join(name));
        }
    }
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
        private_file(directory.join("prompts").join(name));
    }
    for name in ["workflows", "skills", "prompts", "sessions", "exports"] {
        private_dir(directory.join(name));
    }
    assert_eq!(
        seeded.metadata().unwrap().permissions().mode() & 0o077,
        0o044
    );
    assert_eq!(
        directory.metadata().unwrap().permissions().mode() & 0o077,
        0o055
    );
    assert_eq!(
        directory
            .join("prompts")
            .metadata()
            .unwrap()
            .permissions()
            .mode()
            & 0o077,
        0
    );
}

#[test]
#[cfg(unix)]
fn runtime_log_is_owner_only_when_created() {
    let binary = env!("CARGO_BIN_EXE_diet_soda");
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.json");
    std::fs::write(&config, serde_json::json!({
        "providers":{"openrouter":{"kind":"openrouter","base_url":"http://127.0.0.1:1","api_key_env":null,"timeout_seconds":1}},
        "model":{"provider":"openrouter","model":"local/model"}
    }).to_string()).unwrap();
    let _ = Command::new(binary)
        .args(["--config", config.to_str().unwrap(), "--prompt", "test"])
        .output()
        .unwrap();
    let sessions = tmp.path().join("sessions");
    assert_eq!(sessions.metadata().unwrap().permissions().mode() & 0o077, 0);
    assert_eq!(
        sessions
            .join("diet_soda.log")
            .metadata()
            .unwrap()
            .permissions()
            .mode()
            & 0o077,
        0
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

#[test]
#[cfg(unix)]
fn tui_activity_accordion_expands_and_collapses_with_keyboard_and_sgr_mouse() {
    let tmp = tempfile::tempdir().unwrap();
    let output = Command::new("python3")
        .arg(format!(
            "{}/tests/fixtures/activity_accordion.py",
            env!("CARGO_MANIFEST_DIR")
        ))
        .arg(env!("CARGO_BIN_EXE_diet_soda"))
        .arg(tmp.path())
        .env("ACTIVITY_TEST_KEY", "dummy")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[cfg(unix)]
fn tui_geometry_survives_controlling_pty_resize() {
    let tmp = tempfile::tempdir().unwrap();
    let output = Command::new("python3")
        .arg(format!(
            "{}/tests/fixtures/tui_geometry.py",
            env!("CARGO_MANIFEST_DIR")
        ))
        .arg(env!("CARGO_BIN_EXE_diet_soda"))
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[cfg(unix)]
fn headless_terminal_safety_differs_from_exact_piped_model_output() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.json");
    let output = Command::new("python3")
        .arg(format!(
            "{}/tests/fixtures/headless_text_safety.py",
            env!("CARGO_MANIFEST_DIR")
        ))
        .arg(env!("CARGO_BIN_EXE_diet_soda"))
        .arg(&config)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn list_workflows_uses_the_configured_workflows_directory() {
    let binary = env!("CARGO_BIN_EXE_diet_soda");
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.json");
    let workflows = tmp.path().join("configured-workflows");
    std::fs::create_dir_all(&workflows).unwrap();
    std::fs::write(workflows.join("zeta.json"), "{}").unwrap();
    std::fs::write(workflows.join("alpha.json"), "{}").unwrap();
    std::fs::write(workflows.join("not-a-workflow.txt"), "ignored").unwrap();
    write_cli_config(&config, "configured-workflows", "skills");

    let output = Command::new(binary)
        .args(["--config", config.to_str().unwrap(), "--list-workflows"])
        .output()
        .unwrap();

    assert!(output.status.success(), "{}", output_text(&output.stderr));
    assert_eq!(
        output_text(&output.stdout),
        format!(
            "{}\n{}\n",
            workflows.join("alpha.json").display(),
            workflows.join("zeta.json").display()
        )
    );
    assert!(!output_text(&output.stdout).contains("not-a-workflow"));
}

#[test]
fn list_skills_prints_discovered_skill_names_from_the_configured_directory() {
    let binary = env!("CARGO_BIN_EXE_diet_soda");
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.json");
    let skills = tmp.path().join("configured-skills");
    std::fs::create_dir_all(skills.join("bravo")).unwrap();
    std::fs::create_dir_all(skills.join("alpha")).unwrap();
    std::fs::write(skills.join("bravo/SKILL.md"), skill_text("bravo")).unwrap();
    std::fs::write(skills.join("alpha/SKILL.md"), skill_text("alpha")).unwrap();
    write_cli_config(&config, "workflows", "configured-skills");

    let output = Command::new(binary)
        .args(["--config", config.to_str().unwrap(), "--list-skills"])
        .output()
        .unwrap();

    assert!(output.status.success(), "{}", output_text(&output.stderr));
    let stdout = output_text(&output.stdout);
    assert!(stdout.contains("alpha — A local test skill"), "{stdout}");
    assert!(stdout.contains("bravo — A local test skill"), "{stdout}");
    assert!(stdout.find("alpha").unwrap() < stdout.find("bravo").unwrap());
}

#[test]
fn install_skill_accepts_local_file_directory_and_gzip_archive_and_reports_destinations() {
    let binary = env!("CARGO_BIN_EXE_diet_soda");
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.json");
    let destination = tmp.path().join("installed-skills");
    write_cli_config(&config, "workflows", "installed-skills");

    let standalone = tmp.path().join("standalone.md");
    std::fs::write(&standalone, skill_text("standalone-skill")).unwrap();
    let directory = tmp.path().join("directory-skill");
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(directory.join("SKILL.md"), skill_text("directory-skill")).unwrap();

    let archive = tmp.path().join("archive.tar.gz");
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    let body = skill_text("archive-skill");
    let mut header = tar::Header::new_gnu();
    header.set_path("archive-skill/SKILL.md").unwrap();
    header.set_size(body.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append(&header, body.as_bytes()).unwrap();
    let bytes = builder.into_inner().unwrap().finish().unwrap();
    std::fs::write(&archive, bytes).unwrap();

    for (source, name) in [
        (&standalone, "standalone-skill"),
        (&directory, "directory-skill"),
        (&archive, "archive-skill"),
    ] {
        let output = Command::new(binary)
            .args(["--config", config.to_str().unwrap(), "--install-skill"])
            .arg(source)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", output_text(&output.stderr));
        assert!(
            output_text(&output.stdout)
                .contains(&format!("Installed {}", destination.join(name).display())),
            "{}",
            output_text(&output.stdout)
        );
        assert!(destination.join(name).join("SKILL.md").is_file());
    }
}

#[test]
fn invalid_or_missing_cli_skill_inputs_fail_with_clear_stderr() {
    let binary = env!("CARGO_BIN_EXE_diet_soda");
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.json");
    write_cli_config(&config, "workflows", "skills");

    let missing_source = tmp.path().join("does-not-exist.md");
    let output = Command::new(binary)
        .args(["--config", config.to_str().unwrap(), "--install-skill"])
        .arg(&missing_source)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = output_text(&output.stderr);
    assert!(stderr.contains("Error:"), "{stderr}");
    assert!(stderr.contains("No such file or directory"), "{stderr}");

    let missing_config = tmp.path().join("missing-config.json");
    let output = Command::new(binary)
        .args([
            "--config",
            missing_config.to_str().unwrap(),
            "--list-skills",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = output_text(&output.stderr);
    assert!(stderr.contains("Use --init --config"), "{stderr}");
    assert!(stderr.contains("missing-config.json"), "{stderr}");
}

#[test]
fn explicit_config_override_prevents_default_auto_init_side_effects() {
    let binary = env!("CARGO_BIN_EXE_diet_soda");
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let config_dir = tmp.path().join("explicit");
    std::fs::create_dir_all(&config_dir).unwrap();
    let config = config_dir.join("config.json");
    write_cli_config(&config, "workflows", "skills");

    let output = Command::new(binary)
        .args(["--config", config.to_str().unwrap(), "--list-workflows"])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", tmp.path().join("ignored-xdg"))
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{}", output_text(&output.stderr));
    assert!(output_text(&output.stderr).is_empty());
    assert!(!home.join(".config/diet_soda").exists());
    assert!(!tmp.path().join("ignored-xdg").exists());
    assert!(!tmp.path().join("workflows").exists());
    assert!(!tmp.path().join("skills").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_streams_assistant_to_stdout_and_status_to_stderr() {
    let mut mock = server(vec![answer("local streamed answer")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.json");
    headless_config(&config, &mock.url, serde_json::json!({}));

    let output = run_cli(&config, &["--prompt", "Say hello"]);
    assert!(output.status.success(), "{}", output_text(&output.stderr));
    assert_eq!(output_text(&output.stdout), "local streamed answer\n");
    let stderr = output_text(&output.stderr);
    assert!(stderr.contains("Connecting"), "{stderr}");
    assert!(stderr.contains("Waiting"), "{stderr}");
    assert!(stderr.contains("Streaming"), "{stderr}");
    assert!(mock.requests.recv().await.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_selection_uses_agent_model_and_native_effort_in_request() {
    let mut mock = server(vec![answer("selected")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.json");
    headless_config(
        &config,
        &mock.url,
        serde_json::json!({
            "models": [{"name":"named-model","provider":"openrouter","model":"catalog-model","max_tokens":256,"reasoning":{"supported_efforts":["low","high"]}}],
            "agents": [{"name":"reviewer","prompt":"Agent marker","model":"named-model"}]
        }),
    );

    let output = run_cli(
        &config,
        &[
            "--prompt",
            "Inspect this",
            "--agent",
            "reviewer",
            "--model",
            "named-model",
            "--effort",
            "high",
        ],
    );
    assert!(output.status.success(), "{}", output_text(&output.stderr));
    let request = mock.requests.recv().await.unwrap();
    let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["model"], "catalog-model");
    assert_eq!(body["reasoning"]["effort"], "high");
    let system = body["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("Agent marker"), "{system}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_resume_sends_only_complete_main_history() {
    let mut mock = server(vec![answer("resumed")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.json");
    headless_config(&config, &mock.url, serde_json::json!({}));
    let session_dir = tmp.path().join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let session = session_dir.join("resume-check.jsonl");
    let message = |context: &str, data: serde_json::Value| {
        serde_json::json!({"type":"message","context":context,"data":data}).to_string()
    };
    let mut incomplete = serde_json::json!({"role":"assistant","content":"partial","incomplete":{"reason":"cut off"}});
    let lines = [
        message(
            "main",
            serde_json::json!({"role":"user","content":"old question"}),
        ),
        message(
            "child:one",
            serde_json::json!({"role":"assistant","content":"child secret"}),
        ),
        message("main", std::mem::take(&mut incomplete)),
    ];
    std::fs::write(&session, format!("{}\n", lines.join("\n"))).unwrap();

    let output = run_cli(
        &config,
        &["--session", "resume-check", "--prompt", "continue"],
    );
    assert!(output.status.success(), "{}", output_text(&output.stderr));
    let request: serde_json::Value =
        serde_json::from_str(&mock.requests.recv().await.unwrap().body).unwrap();
    let messages = request["messages"].as_array().unwrap();
    assert!(messages.iter().any(|m| m["content"] == "old question"));
    assert!(messages.iter().any(|m| m["content"] == "continue"));
    assert!(!messages.iter().any(|m| m["content"] == "child secret"));
    assert!(!messages.iter().any(|m| m["content"] == "partial"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_input_runs_two_local_steps_and_missing_workflow_fails() {
    let mut mock = server(vec![answer("step one"), answer("final step")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.json");
    headless_config(&config, &mock.url, serde_json::json!({}));
    let workflows = tmp.path().join("workflows");
    std::fs::create_dir_all(&workflows).unwrap();
    std::fs::write(
        workflows.join("two-step.json"),
        serde_json::json!({"title":"Two step","author":"test","steps":[
            {"model":"default-model","prompt":"First {{input}}","mcps":[],"hitl":false},
            {"model":"default-model","prompt":"Second","mcps":[],"hitl":false}
        ]})
        .to_string(),
    )
    .unwrap();

    let output = run_cli(&config, &["--workflow", "two-step", "--input", "seed"]);
    assert!(output.status.success(), "{}", output_text(&output.stderr));
    assert!(output_text(&output.stdout).contains("final step"));
    assert!(mock.requests.recv().await.is_some());
    assert!(mock.requests.recv().await.is_some());

    let missing = run_cli(
        &config,
        &["--workflow", "does-not-exist", "--input", "seed"],
    );
    assert!(!missing.status.success());
    assert!(
        !output_text(&missing.stderr).is_empty(),
        "missing workflow error was not reported"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_tty_approval_aborts_write_file_without_executing_it() {
    let mut mock = server(vec![tool_call(
        "write_file",
        serde_json::json!({"path":"should-not-exist.txt","content":"must not write"}),
    )])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.json");
    headless_config(
        &config,
        &mock.url,
        serde_json::json!({
            "agents": [{"name":"editor","default":true,"can_edit":true}],
            "builtins": ["write_file"]
        }),
    );

    let output = run_cli(&config, &["--prompt", "Write the file"]);
    assert!(!output.status.success());
    assert!(!tmp.path().join("should-not-exist.txt").exists());
    assert!(
        !output_text(&output.stderr).is_empty(),
        "approval failure was not reported"
    );
    assert!(mock.requests.recv().await.is_some());
}

#[test]
fn non_tty_stdout_without_prompt_or_workflow_fails_clearly() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.json");
    headless_config(&config, "http://127.0.0.1:1", serde_json::json!({}));
    let output = run_cli(&config, &[]);
    assert!(!output.status.success());
    assert!(output_text(&output.stderr).contains("use --prompt or --workflow"));
}
