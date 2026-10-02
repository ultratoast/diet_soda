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

fn assert_rule(
    config: &Config,
    command: &str,
    args: &[&str],
    expected_pattern: &str,
    expected_action: BashAction,
) {
    match decide(config, command, args) {
        BashDecision::Rule { pattern, action } => {
            assert_eq!(pattern, expected_pattern, "{command} {args:?}");
            assert_eq!(action, expected_action, "{command} {args:?}");
        }
        other => panic!("{command} {args:?} expected a rule, got {other:?}"),
    }
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

#[test]
fn embedded_default_bash_policy_is_allow_all_with_blacklist() {
    let tmp = tempfile::tempdir().unwrap();
    let config = default_config(tmp.path());

    assert_rule(
        &config,
        "git",
        &["push", "origin", "main"],
        "git push*",
        BashAction::Ask,
    );
    assert!(matches!(
        decide(&config, "git", &["push", "--force"]),
        BashDecision::Denied { .. }
    ));
    assert!(matches!(
        decide(&config, "rm", &["-rf", "/"]),
        BashDecision::Denied { .. }
    ));
    assert_rule(&config, "chmod", &["+x", "x"], "chmod*", BashAction::Ask);
    assert_rule(&config, "rmdir", &["d"], "rmdir*", BashAction::Ask);
    for (command, args) in [("ls", &[][..]), ("cargo", &["build"][..])] {
        assert_rule(&config, command, args, "*", BashAction::Allow);
    }
    for (command, args, pattern) in [
        ("systemctl", &["stop", "x"][..], "systemctl*"),
        ("curl", &["http://x"][..], "curl*"),
    ] {
        assert_rule(&config, command, args, pattern, BashAction::Ask);
    }
    assert!(matches!(
        decide(&config, "dd", &["if=x"]),
        BashDecision::Denied { .. }
    ));
}
