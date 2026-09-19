use std::process::Command;

#[test]
fn cli_initializes_validates_examples_and_refuses_overwrite() {
    let binary = env!("CARGO_BIN_EXE_diet-harness");
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.json");
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
fn tui_pseudo_terminal_restores_terminal_after_help_and_quit() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.json");
    std::fs::write(&config, "{}").unwrap();
    let output = Command::new("python3")
        .arg(format!(
            "{}/tests/fixtures/tui_smoke.py",
            env!("CARGO_MANIFEST_DIR")
        ))
        .arg(env!("CARGO_BIN_EXE_diet-harness"))
        .arg(config)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
