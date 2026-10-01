//! Phase 2 pre-prompt unified bash policy dispatch tests. These exercise the
//! engine approval flow (not just the policy evaluator): a `deny` rule fails
//! before any approval prompt, `ask` forces one with the matched glob in the
//! detail, `allow` suppresses only the ordinary-risk heuristic, and outside
//! workspace / legacy blocks remain independent.
mod support;
use diet_soda::{
    config::{Config, ToolConfig},
    engine::{Engine, Selection},
    model::{ActivityEvent, ActivityKind, ActivityPhase, ActivityStatus, Decision, UiEvent},
};
use serde_json::{json, Value};
use support::*;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn write_policy(dir: &std::path::Path, policy: &str) {
    std::fs::write(dir.join("bash-permissions.json"), policy).unwrap();
}

fn reader_config(url: &str, dir: &std::path::Path) -> Config {
    let mut config = config(url, dir);
    config.agents.insert(
        "reader".into(),
        serde_json::from_value(json!({"tools":["shell"]})).unwrap(),
    );
    config
}

fn reader() -> Selection {
    Selection {
        agent: Some("reader".into()),
        ..Selection::default()
    }
}

fn echo_tool(hitl: bool) -> ToolConfig {
    serde_json::from_value(json!({
        "type":"command",
        "command":"/bin/echo",
        "args":["{{value}}"],
        "description":"echo probe",
        "hitl":hitl,
        "destructive":false,
        "input_schema":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}
    }))
    .unwrap()
}

fn echo_tool_in(hitl: bool, cwd: &std::path::Path) -> ToolConfig {
    let mut value = json!({
        "type":"command",
        "command":"/bin/echo",
        "args":["{{value}}"],
        "description":"echo probe",
        "hitl":hitl,
        "destructive":false,
        "input_schema":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}
    });
    value["cwd"] = json!(cwd);
    serde_json::from_value(value).unwrap()
}

fn sheller_config(url: &str, dir: &std::path::Path, allow_outside: bool) -> Config {
    let mut config = config(url, dir);
    config.agents.insert(
        "sheller".into(),
        serde_json::from_value(json!({
            "can_edit": true,
            "allow_outside_workspace": allow_outside,
            "tools": ["shell"],
        }))
        .unwrap(),
    );
    config
}

fn sheller() -> Selection {
    Selection {
        agent: Some("sheller".into()),
        ..Selection::default()
    }
}

#[derive(Default)]
struct RunOutcome {
    approvals: usize,
    details: Vec<String>,
    persist_allowed: Vec<bool>,
    activities: Vec<ActivityEvent>,
}

fn tool_end_status(outcome: &RunOutcome) -> Option<ActivityStatus> {
    outcome
        .activities
        .iter()
        .rev()
        .find(|a| a.kind == ActivityKind::Tool && a.phase == ActivityPhase::End)
        .and_then(|a| a.status)
}

async fn drive_turn(
    engine: Engine,
    selection: Selection,
    mut events: mpsc::UnboundedReceiver<UiEvent>,
    on_approval: impl Fn(usize) -> Decision,
) -> RunOutcome {
    let runner = engine.clone();
    let mut task = tokio::spawn(async move {
        runner
            .turn("go".into(), selection, CancellationToken::new())
            .await
    });
    let mut outcome = RunOutcome::default();
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(UiEvent::Approval { reply, detail, persist_allowed, .. }) => {
                    outcome.approvals += 1;
                    outcome.details.push(detail);
                    outcome.persist_allowed.push(persist_allowed);
                    reply.send(on_approval(outcome.approvals)).unwrap();
                }
                Some(UiEvent::Activity(activity)) => outcome.activities.push(activity),
                Some(_) => {}
                None => break,
            },
            result = &mut task => {
                result.unwrap().unwrap();
                break;
            }
        }
    }
    outcome
}

#[test]
fn ordinary_risk_classification_is_independent_of_outside_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/target")).unwrap();
    let outside_file = outside.path().join("secret.txt");
    std::fs::write(&outside_file, "data").unwrap();
    let outside_path = outside_file.to_string_lossy().into_owned();
    let config = Config {
        workspace: tmp.path().into(),
        config_dir: tmp.path().into(),
        ..Config::default()
    };
    // `cat` on an outside path is not ordinary-risk, but the bundled
    // classifier still flags the outside escape. The two signals are
    // independent, which is exactly what an `allow` rule must not suppress.
    assert!(!diet_soda::tools::shell_ordinary_risk_approval(
        "/bin/cat",
        std::slice::from_ref(&outside_path)
    ));
    assert!(diet_soda::tools::shell_requires_approval(
        &config,
        "/bin/cat",
        std::slice::from_ref(&outside_path),
        false
    )
    .unwrap());
    // A mutating command is ordinary-risk regardless of paths.
    assert!(diet_soda::tools::shell_ordinary_risk_approval(
        "/bin/mkdir",
        &["created".into()]
    ));
}

fn last_tool_message(body: &Value) -> Value {
    body["messages"].as_array().unwrap().last().unwrap().clone()
}

#[tokio::test]
async fn deny_rule_on_shell_fails_before_prompt() {
    let mut server = server(vec![
        tool_call("shell", json!({"command":"/bin/rm","args":["-rf","build"]})),
        answer("done"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut test_config = reader_config(&server.url, tmp.path());
    write_policy(
        tmp.path(),
        r#"{"blocked_commands":[],"blocked_patterns":[],"bash":{"rm *":"deny"}}"#,
    );
    test_config.config_dir = tmp.path().into();
    let (engine, events) = engine(test_config);
    let outcome = drive_turn(engine, reader(), events, |_| Decision::Reject).await;

    assert_eq!(outcome.approvals, 0, "deny must not prompt");
    assert_eq!(
        tool_end_status(&outcome),
        Some(ActivityStatus::Error),
        "deny is a tool error, not a user rejection"
    );
    server.requests.recv().await.unwrap();
    let followup: Value =
        serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let tool_message = last_tool_message(&followup);
    let content = tool_message["content"].as_str().unwrap();
    // The reason is embedded in a serialized JSON tool result, so the glob's
    // quotes are escaped; assert on the unquoted fragments.
    assert!(
        content.contains("Blocked by bash permission rule"),
        "{content}"
    );
    assert!(content.contains("rm *"), "{content}");
}

#[tokio::test]
async fn deny_rule_on_custom_command_fails_before_prompt() {
    let server = server(vec![
        tool_call("echo_probe", json!({"value":"hello"})),
        answer("done"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut test_config = config(&server.url, tmp.path());
    write_policy(
        tmp.path(),
        r#"{"blocked_commands":[],"blocked_patterns":[],"bash":{"echo *":"deny"}}"#,
    );
    test_config.config_dir = tmp.path().into();
    // `hitl` would normally force a prompt; the deny must preempt it.
    test_config
        .tools
        .insert("echo_probe".into(), echo_tool(true));
    let (engine, events) = engine(test_config);
    let outcome = drive_turn(engine, Selection::default(), events, |_| Decision::Reject).await;

    assert_eq!(outcome.approvals, 0, "deny must preempt the hitl prompt");
    assert_eq!(tool_end_status(&outcome), Some(ActivityStatus::Error));
}

#[tokio::test]
async fn custom_command_preflight_matches_rendered_executor_subject() {
    // The deny glob matches only the template-RENDERED argument. If the
    // preflight evaluated the raw `{{value}}` template instead of the rendered
    // argv, no rule would match and the hitl prompt would appear.
    let server = server(vec![
        tool_call("echo_probe", json!({"value":"danger"})),
        answer("done"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut test_config = config(&server.url, tmp.path());
    write_policy(
        tmp.path(),
        r#"{"blocked_commands":[],"blocked_patterns":[],"bash":{"echo danger":"deny"}}"#,
    );
    test_config.config_dir = tmp.path().into();
    test_config
        .tools
        .insert("echo_probe".into(), echo_tool(true));
    let (engine, events) = engine(test_config);
    let outcome = drive_turn(engine, Selection::default(), events, |_| Decision::Reject).await;

    assert_eq!(
        outcome.approvals, 0,
        "rendered subject must match the deny rule pre-prompt"
    );
    assert_eq!(tool_end_status(&outcome), Some(ActivityStatus::Error));
}

#[tokio::test]
async fn ask_rule_prompts_with_glob_and_persist_grants_family() {
    let server = server(vec![
        tool_call("shell", json!({"command":"/bin/echo","args":["hello"]})),
        tool_call("shell", json!({"command":"/bin/echo","args":["hello"]})),
        answer("done"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut test_config = reader_config(&server.url, tmp.path());
    write_policy(
        tmp.path(),
        r#"{"blocked_commands":[],"blocked_patterns":[],"bash":{"echo *":"ask"}}"#,
    );
    test_config.config_dir = tmp.path().into();
    let (engine, events) = engine(test_config);
    let outcome = drive_turn(engine, reader(), events, |_| Decision::ApprovePersist).await;

    assert_eq!(
        outcome.approvals, 1,
        "persistent family grant must suppress the repeat prompt"
    );
    assert!(
        outcome.details[0].contains("bash permission rule \"echo *\" requires approval"),
        "approval detail must name the matched glob, got: {}",
        outcome.details[0]
    );
    assert_eq!(outcome.persist_allowed, vec![true]);
    assert_eq!(tool_end_status(&outcome), Some(ActivityStatus::Success));
}

#[tokio::test]
async fn ask_rule_with_outside_path_prompts_but_suppresses_persist() {
    #[cfg(not(unix))]
    return;
    #[cfg(unix)]
    {
        let outside = tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/target")).unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "outside-data").unwrap();
        let secret = secret.to_string_lossy().into_owned();
        let server = server(vec![
            tool_call(
                "shell",
                json!({"command":"/bin/cat","args":[secret.clone()]}),
            ),
            tool_call(
                "shell",
                json!({"command":"/bin/cat","args":[secret.clone()]}),
            ),
            answer("done"),
        ])
        .await;
        let tmp = tempfile::tempdir().unwrap();
        let mut test_config = reader_config(&server.url, tmp.path());
        write_policy(
            tmp.path(),
            r#"{"blocked_commands":[],"blocked_patterns":[],"bash":{"cat *":"ask"}}"#,
        );
        test_config.config_dir = tmp.path().into();
        let (engine, events) = engine(test_config);
        let outcome = drive_turn(engine, reader(), events, |_| Decision::Approve).await;

        assert_eq!(
            outcome.approvals, 2,
            "outside path must keep prompting; no family grant may be written"
        );
        assert!(
            outcome.persist_allowed.iter().all(|allowed| !allowed),
            "outside path must suppress the persistent grant"
        );
    }
}

#[tokio::test]
async fn allow_rule_suppresses_ordinary_risk_without_prompt() {
    // `mkdir` is a mutating command (ordinary risk) that is not legacy-blocked.
    let server = server(vec![
        tool_call("shell", json!({"command":"/bin/mkdir","args":["created"]})),
        answer("done"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut test_config = reader_config(&server.url, tmp.path());
    write_policy(
        tmp.path(),
        r#"{"blocked_commands":[],"blocked_patterns":[],"bash":{"mkdir *":"allow"}}"#,
    );
    test_config.config_dir = tmp.path().into();
    let (engine, events) = engine(test_config);
    let outcome = drive_turn(engine, reader(), events, |_| Decision::Reject).await;

    assert_eq!(outcome.approvals, 0, "allow rule must suppress the prompt");
    assert_eq!(tool_end_status(&outcome), Some(ActivityStatus::Success));
    assert!(tmp.path().join("created").is_dir());
}

#[tokio::test]
async fn allow_rule_does_not_bypass_outside_workspace_prompt() {
    #[cfg(not(unix))]
    return;
    #[cfg(unix)]
    {
        let outside = tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/target")).unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "outside-data").unwrap();
        let secret = secret.to_string_lossy().into_owned();
        let server = server(vec![
            tool_call("shell", json!({"command":"/bin/cat","args":[secret]})),
            answer("done"),
        ])
        .await;
        let tmp = tempfile::tempdir().unwrap();
        let mut test_config = reader_config(&server.url, tmp.path());
        write_policy(
            tmp.path(),
            r#"{"blocked_commands":[],"blocked_patterns":[],"bash":{"cat *":"allow"}}"#,
        );
        test_config.config_dir = tmp.path().into();
        let (engine, events) = engine(test_config);
        let outcome = drive_turn(engine, reader(), events, |_| Decision::Approve).await;

        assert_eq!(
            outcome.approvals, 1,
            "allow must not suppress the outside-workspace gate"
        );
        assert_eq!(outcome.persist_allowed, vec![false]);
        assert_eq!(tool_end_status(&outcome), Some(ActivityStatus::Success));
    }
}

#[tokio::test]
async fn allow_rule_does_not_bypass_legacy_block() {
    let server = server(vec![
        tool_call("shell", json!({"command":"/bin/mkdir","args":["created"]})),
        answer("done"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut test_config = reader_config(&server.url, tmp.path());
    write_policy(
        tmp.path(),
        r#"{"blocked_commands":["mkdir"],"blocked_patterns":[],"bash":{"*":"allow"}}"#,
    );
    test_config.config_dir = tmp.path().into();
    let (engine, events) = engine(test_config);
    let outcome = drive_turn(engine, reader(), events, |_| Decision::Reject).await;

    assert_eq!(outcome.approvals, 0, "legacy block must deny pre-prompt");
    assert_eq!(tool_end_status(&outcome), Some(ActivityStatus::Error));
    assert!(!tmp.path().join("created").exists());
}

#[tokio::test]
async fn malformed_policy_fails_closed_before_prompt() {
    let mut server = server(vec![
        tool_call("shell", json!({"command":"/bin/echo","args":["hi"]})),
        answer("done"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut test_config = reader_config(&server.url, tmp.path());
    write_policy(
        tmp.path(),
        r#"{"blocked_commands":[],"blocked_patterns":[],"bash":{"echo *":"explode"}}"#,
    );
    test_config.config_dir = tmp.path().into();
    let (engine, events) = engine(test_config);
    let outcome = drive_turn(engine, reader(), events, |_| Decision::Reject).await;

    assert_eq!(outcome.approvals, 0, "malformed policy must not prompt");
    assert_eq!(tool_end_status(&outcome), Some(ActivityStatus::Error));
    server.requests.recv().await.unwrap();
    let followup: Value =
        serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let tool_message = last_tool_message(&followup);
    assert!(
        tool_message["content"]
            .as_str()
            .unwrap()
            .contains("bash-permissions.json"),
        "error must name the policy file, got: {}",
        tool_message["content"]
    );
}

#[tokio::test]
async fn allow_rule_does_not_bypass_custom_hitl() {
    let server = server(vec![
        tool_call("echo_probe", json!({"value":"hi"})),
        answer("done"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut test_config = config(&server.url, tmp.path());
    write_policy(
        tmp.path(),
        r#"{"blocked_commands":[],"blocked_patterns":[],"bash":{"echo *":"allow"}}"#,
    );
    test_config.config_dir = tmp.path().into();
    test_config
        .tools
        .insert("echo_probe".into(), echo_tool(true));
    let (engine, events) = engine(test_config);
    let outcome = drive_turn(engine, Selection::default(), events, |_| Decision::Approve).await;

    assert_eq!(
        outcome.approvals, 1,
        "allow must not suppress a custom-tool HITL gate"
    );
    assert_eq!(outcome.persist_allowed, vec![false]);
    assert_eq!(tool_end_status(&outcome), Some(ActivityStatus::Success));
}

#[tokio::test]
async fn allow_rule_does_not_bypass_custom_outside_cwd() {
    #[cfg(not(unix))]
    return;
    #[cfg(unix)]
    {
        let outside = tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/target")).unwrap();
        let server = server(vec![
            tool_call("echo_probe", json!({"value":"hi"})),
            answer("done"),
        ])
        .await;
        let tmp = tempfile::tempdir().unwrap();
        let mut test_config = config(&server.url, tmp.path());
        write_policy(
            tmp.path(),
            r#"{"blocked_commands":[],"blocked_patterns":[],"bash":{"echo *":"allow"}}"#,
        );
        test_config.config_dir = tmp.path().into();
        test_config
            .tools
            .insert("echo_probe".into(), echo_tool_in(false, outside.path()));
        let (engine, events) = engine(test_config);
        let outcome = drive_turn(engine, Selection::default(), events, |_| Decision::Approve).await;

        assert_eq!(
            outcome.approvals, 1,
            "allow must not suppress the outside-cwd gate"
        );
        assert_eq!(outcome.persist_allowed, vec![false]);
        assert_eq!(tool_end_status(&outcome), Some(ActivityStatus::Success));
    }
}

#[tokio::test]
async fn ask_rule_with_custom_outside_prompts_without_family_grant() {
    #[cfg(not(unix))]
    return;
    #[cfg(unix)]
    {
        let outside = tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/target")).unwrap();
        let server = server(vec![
            tool_call("echo_probe", json!({"value":"hi"})),
            tool_call("echo_probe", json!({"value":"hi"})),
            answer("done"),
        ])
        .await;
        let tmp = tempfile::tempdir().unwrap();
        let mut test_config = config(&server.url, tmp.path());
        write_policy(
            tmp.path(),
            r#"{"blocked_commands":[],"blocked_patterns":[],"bash":{"echo *":"ask"}}"#,
        );
        test_config.config_dir = tmp.path().into();
        test_config
            .tools
            .insert("echo_probe".into(), echo_tool_in(false, outside.path()));
        let (engine, events) = engine(test_config);
        let outcome = drive_turn(engine, Selection::default(), events, |_| Decision::Approve).await;

        assert_eq!(
            outcome.approvals, 2,
            "the outside-cwd gate must suppress the family grant"
        );
        assert!(outcome.persist_allowed.iter().all(|allowed| !allowed));
        assert_eq!(tool_end_status(&outcome), Some(ActivityStatus::Success));
    }
}

#[tokio::test]
async fn allow_rule_with_standing_outside_grant_auto_runs_risky_command() {
    #[cfg(not(unix))]
    return;
    #[cfg(unix)]
    {
        let outside = tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/target")).unwrap();
        let target = outside.path().join("created-by-policy");
        let target_arg = target.to_string_lossy().into_owned();
        let server = server(vec![
            tool_call("shell", json!({"command":"/bin/mkdir","args":[target_arg]})),
            answer("done"),
        ])
        .await;
        let tmp = tempfile::tempdir().unwrap();
        let mut test_config = sheller_config(&server.url, tmp.path(), true);
        write_policy(
            tmp.path(),
            r#"{"blocked_commands":[],"blocked_patterns":[],"bash":{"mkdir *":"allow"}}"#,
        );
        test_config.config_dir = tmp.path().into();
        let (engine, events) = engine(test_config);
        let outcome = drive_turn(engine, sheller(), events, |_| Decision::Reject).await;

        assert_eq!(
            outcome.approvals, 0,
            "standing outside grant plus allow must auto-run"
        );
        assert_eq!(tool_end_status(&outcome), Some(ActivityStatus::Success));
        assert!(target.is_dir());
    }
}

/// Drive one shell invocation stuffed with metacharacters and return the
/// outcome plus the serialized tool result content. The fake model issues a
/// single shell tool call and then a final answer.
async fn metachar_case(command: &str) -> (RunOutcome, String) {
    let mut server = server(vec![
        tool_call("shell", json!({"command":command,"args":[]})),
        answer("done"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let test_config = reader_config(&server.url, tmp.path());
    let (engine, events) = engine(test_config);
    let outcome = drive_turn(engine, reader(), events, |_| Decision::Reject).await;
    server.requests.recv().await.unwrap();
    let followup: Value =
        serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let content = last_tool_message(&followup)["content"]
        .as_str()
        .unwrap()
        .to_owned();
    (outcome, content)
}

#[tokio::test]
async fn shell_command_metacharacters_fail_pre_prompt_with_guidance() {
    // A `command` token containing pipes, redirects, or chaining can never
    // execute (the harness runs argv without a shell). It must fail as a tool
    // error before any approval prompt, with guidance to move arguments into
    // `args` and split pipelines into multiple calls.
    for command in [
        "find . | head -80",
        "file * .* 2>/dev/null",
        "foo > bar",
        "ls; rm -rf build",
    ] {
        let (outcome, content) = metachar_case(command).await;
        assert_eq!(outcome.approvals, 0, "{command}: must not prompt");
        assert_eq!(
            tool_end_status(&outcome),
            Some(ActivityStatus::Error),
            "{command}: must be a tool error"
        );
        assert!(
            content.contains("without a shell"),
            "{command}: guidance missing from {content}"
        );
        assert!(
            content.contains("split pipelines into multiple calls"),
            "{command}: guidance missing from {content}"
        );
    }
}

#[tokio::test]
async fn shell_metacharacter_error_precedes_allow_rule() {
    // Even a catch-all `allow` rule must not reach the approval machinery for
    // an invocation that cannot execute: validation precedes policy.
    let server = server(vec![
        tool_call("shell", json!({"command":"find . | head -80","args":[]})),
        answer("done"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut test_config = reader_config(&server.url, tmp.path());
    write_policy(
        tmp.path(),
        r#"{"blocked_commands":[],"blocked_patterns":[],"bash":{"*":"allow"}}"#,
    );
    test_config.config_dir = tmp.path().into();
    let (engine, events) = engine(test_config);
    let outcome = drive_turn(engine, reader(), events, |_| Decision::Reject).await;

    assert_eq!(outcome.approvals, 0, "validation must precede policy");
    assert_eq!(tool_end_status(&outcome), Some(ActivityStatus::Error));
}

#[test]
fn shell_command_validation_allows_paths_and_metacharacter_arguments() {
    // Paths and plain program names must pass; only shell metacharacters in the
    // program token are rejected. Argument values are handled by the caller and
    // are deliberately not inspected here.
    for command in ["ls", "./scripts/test.sh", "/usr/bin/rg", "grep", "dd"] {
        assert!(
            diet_soda::tools::validate_shell_command(command).is_ok(),
            "{command} must be accepted"
        );
    }
    for command in [
        "find . | head -80",
        "foo > bar",
        "a && b",
        "x; y",
        "`id`",
        "$(id)",
        "a < b",
        "a\nb",
    ] {
        assert!(
            diet_soda::tools::validate_shell_command(command).is_err(),
            "{command} must be rejected"
        );
    }
}

#[tokio::test]
async fn shell_argument_may_contain_shell_metacharacters() {
    // A metacharacter in an `args` value (a grep pattern here) must not be
    // rejected; only the `command` token is validated. The command auto-runs
    // and reaches the process layer rather than failing validation.
    let mut server = server(vec![
        tool_call("shell", json!({"command":"grep","args":["a|b","file"]})),
        answer("done"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let test_config = reader_config(&server.url, tmp.path());
    let (engine, events) = engine(test_config);
    let outcome = drive_turn(engine, reader(), events, |_| Decision::Reject).await;

    assert_eq!(outcome.approvals, 0, "safe grep must auto-run");
    server.requests.recv().await.unwrap();
    let followup: Value =
        serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let content = last_tool_message(&followup)["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        !content.contains("without a shell"),
        "argument metacharacters must not trip validation: {content}"
    );
}

/// Run one shell tool call against the shipped policy. `config()` stages a
/// policy containing only its legacy lists for older tests, so remove that
/// file here to exercise the embedded policy just as a fresh config dir does.
async fn embedded_policy_shell_case(
    command: &str,
    args: &[&str],
    can_edit: bool,
    file: Option<(&str, &str)>,
    on_approval: impl Fn(usize) -> Decision,
) -> RunOutcome {
    let server = server(vec![
        tool_call(
            "shell",
            json!({"command":command,"args":args}),
        ),
        answer("done"),
    ])
    .await;
    let workspace = tempfile::tempdir().unwrap();
    if let Some((path, contents)) = file {
        std::fs::write(workspace.path().join(path), contents).unwrap();
    }
    let mut test_config = config(&server.url, workspace.path());
    std::fs::remove_file(workspace.path().join("bash-permissions.json")).unwrap();
    test_config.agents.insert(
        "policy-test".into(),
        serde_json::from_value(json!({
            "can_edit": can_edit,
            "tools": ["shell"],
        }))
        .unwrap(),
    );
    let (engine, events) = engine(test_config);
    drive_turn(
        engine,
        Selection {
            agent: Some("policy-test".into()),
            ..Selection::default()
        },
        events,
        on_approval,
    )
    .await
}

fn fail_if_approval(_: usize) -> Decision {
    panic!("unexpected approval prompt")
}

#[tokio::test]
async fn policy_allows_python_and_cargo_without_approval() {
    for (command, args) in [
        ("cargo", vec!["--version"]),
        ("python3", vec!["--version"]),
        ("python3", vec!["-c", "print('a b')"]),
        ("pwd", vec![]),
        ("which", vec!["cargo"]),
        ("/bin/ls", vec!["-la"]),
    ] {
        let outcome =
            embedded_policy_shell_case(command, &args, false, None, fail_if_approval).await;
        assert_eq!(outcome.approvals, 0, "{command} {args:?} must auto-run");
    }
}

#[tokio::test]
async fn wrapped_scripts_run_per_command_without_approval() {
    for (command, args) in [
        ("bash", vec!["-c", "cargo --version && python3 --version"]),
        ("bash", vec!["-c", "cargo --version | head -1"]),
        ("sh", vec!["-c", "pwd && ls"]),
        ("/bin/bash", vec!["-c", "/bin/ls && /bin/ls -la"]),
    ] {
        let outcome =
            embedded_policy_shell_case(command, &args, false, None, fail_if_approval).await;
        assert_eq!(outcome.approvals, 0, "{command} {args:?} must auto-run");
    }
}

#[tokio::test]
async fn sed_auto_runs_for_editors_only() {
    let editor = embedded_policy_shell_case(
        "sed",
        &["-n", "1,2p", "f.txt"],
        true,
        Some(("f.txt", "one\ntwo\n")),
        fail_if_approval,
    )
    .await;
    assert_eq!(editor.approvals, 0, "editor sed should auto-run");

    let reader = embedded_policy_shell_case(
        "sed",
        &["-n", "1,2p", "f.txt"],
        false,
        Some(("f.txt", "one\ntwo\n")),
        |_| Decision::Reject,
    )
    .await;
    assert_eq!(reader.approvals, 1, "non-editor sed should prompt");
}

#[tokio::test]
async fn wrapped_and_path_forms_still_prompt() {
    let cases = [
        ("cargo", vec!["publish", "--dry-run"], false),
        (
            "bash",
            vec!["-c", "cargo --version; npm install x"],
            false,
        ),
        ("bash", vec!["-c", "cargo --version $(whoami)"], false),
        ("bash", vec!["-c", "cargo --version > out.txt"], false),
        ("bash", vec!["-c", "cd src && ls"], false),
        ("bash", vec!["-c", "ls *"], false),
        ("bash", vec!["script.sh"], false),
        ("bash", vec!["python3", "x.py"], false),
        ("./ls", vec![], false),
        ("./pwd", vec![], false),
        ("/tmp/y/bash", vec!["-c", "ls"], false),
        ("sed", vec!["s/a/b/e", "f.txt"], true),
    ];
    for (command, args, can_edit) in cases {
        let outcome = embedded_policy_shell_case(
            command,
            &args,
            can_edit,
            Some(("f.txt", "a\n")),
            |_| Decision::Reject,
        )
        .await;
        assert_eq!(
            outcome.approvals, 1,
            "{command} {args:?} should require approval"
        );
        if command == "bash" && args.get(1) == Some(&"cargo --version; npm install x") {
            assert!(
                outcome.details[0].contains("npm install"),
                "approval detail must identify the npm segment: {}",
                outcome.details[0]
            );
        }
    }
}

#[tokio::test]
async fn deny_in_wrapped_script_fails_without_approval() {
    let outcome = embedded_policy_shell_case(
        "bash",
        &["-c", "cargo --version && rm -rf x"],
        false,
        None,
        fail_if_approval,
    )
    .await;
    assert_eq!(outcome.approvals, 0, "deny must not prompt");
    assert_eq!(
        tool_end_status(&outcome),
        Some(ActivityStatus::Error),
        "a denied inner command should return a tool error"
    );
}

#[tokio::test]
async fn session_grant_only_offered_for_normalized_paths() {
    let normalized = embedded_policy_shell_case(
        "cargo",
        &["publish", "--dry-run"],
        false,
        None,
        |_| Decision::Reject,
    )
    .await;
    assert_eq!(normalized.approvals, 1);
    assert!(
        normalized.details[0].contains("Press p to allow"),
        "normalized command should offer a session grant: {}",
        normalized.details[0]
    );

    let relative = embedded_policy_shell_case(
        "./cargo",
        &["publish", "--dry-run"],
        false,
        None,
        |_| Decision::Reject,
    )
    .await;
    assert_eq!(relative.approvals, 1);
    assert!(
        !relative.details[0].contains("Press p to allow"),
        "non-normalized command must not offer a session grant: {}",
        relative.details[0]
    );
}

async fn embedded_find_case(
    command: &str,
    args: &[&str],
    on_approval: impl Fn(usize) -> Decision,
) -> RunOutcome {
    let server = server(vec![
        tool_call("shell", json!({"command":command,"args":args})),
        answer("done"),
    ])
    .await;
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let mut test_config = config(&server.url, workspace.path());
    // The config helper stages its default policy in the workspace; use a
    // separate empty config directory to exercise the embedded policy instead.
    std::fs::remove_file(workspace.path().join("bash-permissions.json")).unwrap();
    test_config.config_dir = config_dir.path().into();
    test_config.agents.insert(
        "find-policy-test".into(),
        serde_json::from_value(json!({
            "can_edit": false,
            "tools": ["shell"],
        }))
        .unwrap(),
    );
    let (engine, events) = engine(test_config);
    drive_turn(
        engine,
        Selection {
            agent: Some("find-policy-test".into()),
            ..Selection::default()
        },
        events,
        on_approval,
    )
    .await
}

#[tokio::test]
async fn find_read_only_forms_run_without_approval() {
    let outcome = embedded_find_case("find", &[".", "-name", "x"], fail_if_approval).await;
    assert_eq!(outcome.approvals, 0, "read-only find must auto-run");
}

#[tokio::test]
async fn find_dangerous_actions_still_prompt() {
    let outcome = embedded_find_case("find", &[".", "-delete"], |_| Decision::Reject).await;
    assert_eq!(outcome.approvals, 1, "find -delete must prompt");
    assert!(
        outcome.details[0].contains("bash permission rule \"find * -delete*\" requires approval"),
        "approval detail must name the matched infix rule, got: {}",
        outcome.details[0]
    );
}

#[tokio::test]
async fn wrapped_find_script_runs_per_command() {
    let outcome =
        embedded_find_case("bash", &["-c", "find . -name x && ls"], fail_if_approval).await;
    assert_eq!(outcome.approvals, 0, "read-only wrapped find must auto-run");
}

#[tokio::test]
async fn wrapped_find_ask_segment_prompts() {
    let outcome = embedded_find_case(
        "bash",
        &["-c", "find . -name x && find . -delete"],
        |_| Decision::Reject,
    )
    .await;
    assert_eq!(outcome.approvals, 1, "wrapped find -delete must prompt");
    assert!(
        outcome.details[0].contains(
            "find . -delete — rule \"find * -delete*\" requires approval"
        ),
        "approval detail must name the asking segment and matched rule, got: {}",
        outcome.details[0]
    );
}
