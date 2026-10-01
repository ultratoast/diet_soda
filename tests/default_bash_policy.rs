//! The embedded default bash policy (examples/bash-permissions.json, compiled in as
//! `tools::DEFAULT_BASH_PERMISSIONS`) is what `--init` persists and what is used when
//! no bash-permissions.json exists. These tests pin selected default rules.
use diet_soda::{
    config::Config,
    tools::{self, BashAction, BashDecision},
};

fn default_config(dir: &std::path::Path) -> Config {
    Config {
        workspace: dir.into(),
        config_dir: dir.into(),
        ..Config::default()
    }
}

fn decide(config: &Config, command: &str, args: &[&str]) -> BashDecision {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    tools::evaluate_bash_permissions(config, command, &args).expect("embedded default must load")
}

#[test]
fn embedded_default_bash_policy_auto_allows_make() {
    let tmp = tempfile::tempdir().unwrap();
    let config = default_config(tmp.path());
    for args in [
        &[][..],
        &["release"][..],
        &["setup"][..],
        &["-n", "setup"][..],
    ] {
        match decide(&config, "make", args) {
            BashDecision::Rule {
                action: BashAction::Allow,
                ..
            } => {}
            other => panic!("make {args:?} must auto-allow, got {other:?}"),
        }
    }
}

#[test]
fn embedded_default_bash_policy_keeps_destructive_commands_blocked() {
    let tmp = tempfile::tempdir().unwrap();
    let config = default_config(tmp.path());
    assert!(matches!(
        decide(&config, "rm", &["-rf", "/"]),
        BashDecision::Denied { .. }
    ));
    assert!(matches!(
        decide(&config, "git", &["push", "--force"]),
        BashDecision::Denied { .. }
    ));
}
