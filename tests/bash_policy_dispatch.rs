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

/// Editor-capable twin of `reader`. Under the new approval semantics an
/// explicit `ask` rule (pattern != "*") only PROMPTS edit-capable agents and
/// hard-denies read-only agents, so the ask-rule/persist tests below use this
/// agent to keep pinning the prompt mechanics (rule i).
fn editor_config(url: &str, dir: &std::path::Path) -> Config {
    let mut config = reader_config(url, dir);
    config.agents.insert(
        "editor".into(),
        serde_json::from_value(json!({"can_edit": true, "tools": ["shell"]})).unwrap(),
    );
    config
}

fn editor() -> Selection {
    Selection {
        agent: Some("editor".into()),
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
    // Rule (i): this test pins ASK-rule prompt + persist-family mechanics,
    // which now only apply to edit-capable agents (read-only agents are
    // hard-denied for explicit ask rules). All assertions are unchanged.
    let mut test_config = editor_config(&server.url, tmp.path());
    write_policy(
        tmp.path(),
        r#"{"blocked_commands":[],"blocked_patterns":[],"bash":{"echo *":"ask"}}"#,
    );
    test_config.config_dir = tmp.path().into();
    let (engine, events) = engine(test_config);
    let outcome = drive_turn(engine, editor(), events, |_| Decision::ApprovePersist).await;

    assert_eq!(
        outcome.approvals, 1,
        "persistent family grant must suppress the repeat prompt"
    );
    // The reason string format changed with the unified decision table: the
    // prompt cause is now `rule "<glob>" requires approval` (the legacy
    // `bash permission rule ...` prefix is the DENY wording). The assertion's
    // intent — the detail must name the matched glob — is unchanged.
    assert!(
        outcome.details[0].contains("rule \"echo *\" requires approval"),
        "approval detail must name the matched glob, got: {}",
        outcome.details[0]
    );
    assert_eq!(outcome.persist_allowed, vec![true]);
    assert_eq!(tool_end_status(&outcome), Some(ActivityStatus::Success));
}

#[tokio::test]
async fn ask_rule_with_outside_path_prompts_and_offers_directory_grant() {
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
        // Explicit ask rules prompt edit-capable agents; the outside path
        // independently offers a session-long directory grant.
        let mut test_config = editor_config(&server.url, tmp.path());
        write_policy(
            tmp.path(),
            r#"{"blocked_commands":[],"blocked_patterns":[],"bash":{"cat *":"ask"}}"#,
        );
        test_config.config_dir = tmp.path().into();
        let (engine, events) = engine(test_config);
        let outcome = drive_turn(engine, editor(), events, |_| Decision::Approve).await;

        assert_eq!(
            outcome.approvals, 2,
            "without choosing the directory grant, each outside call prompts"
        );
        assert_eq!(
            outcome.persist_allowed,
            vec![true, true],
            "grantable outside paths must offer the session directory grant"
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
        assert_eq!(outcome.persist_allowed, vec![true]);
        assert_eq!(tool_end_status(&outcome), Some(ActivityStatus::Success));
    }
}

#[tokio::test]
async fn outside_directory_persist_grant_suppresses_repeat_prompt() {
    #[cfg(not(unix))]
    return;
    #[cfg(unix)]
    {
        let outside = tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/target")).unwrap();
        let first_file = outside.path().join("first.txt");
        let second_file = outside.path().join("second.txt");
        std::fs::write(&first_file, "first outside file").unwrap();
        std::fs::write(&second_file, "second outside file").unwrap();
        let first_path = first_file.to_string_lossy().into_owned();
        let second_path = second_file.to_string_lossy().into_owned();
        let server = server(vec![
            tool_call("shell", json!({"command":"/bin/cat","args":[first_path]})),
            tool_call("shell", json!({"command":"/bin/cat","args":[second_path]})),
            answer("done"),
        ])
        .await;
        let tmp = tempfile::tempdir().unwrap();
        let test_config = editor_config(&server.url, tmp.path());
        let (engine, events) = engine(test_config);
        let outcome = drive_turn(engine, editor(), events, |approval| {
            assert_eq!(approval, 1, "only the initial outside call should prompt");
            Decision::ApprovePersist
        })
        .await;

        assert_eq!(
            outcome.approvals, 1,
            "the grant must suppress the approval for the second file in the same directory"
        );
        assert_eq!(outcome.persist_allowed, vec![true]);
        assert_eq!(outcome.details.len(), 1);
        assert!(
            outcome.details[0].contains("outside the configured workspace"),
            "initial approval must identify the outside-workspace gate: {}",
            outcome.details[0]
        );
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
        tool_call("shell", json!({"command":command,"args":args})),
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
async fn query_tier_forms_run_without_approval_for_read_only_agents() {
    for (command, args, expected_output, files) in [
        ("cargo", vec!["--version"], vec!["cargo"], vec![]),
        ("python3", vec!["--version"], vec!["Python"], vec![]),
        (
            "bash",
            vec!["-c", "cargo --version && python3 --version"],
            vec!["cargo", "Python"],
            vec![],
        ),
        (
            "/bin/ls",
            vec!["-la"],
            vec!["visible.txt"],
            vec![("visible.txt", "present\n")],
        ),
        ("pwd", vec![], vec!["/"], vec![]),
        ("which", vec!["cargo"], vec!["cargo"], vec![]),
    ] {
        let (outcome, content) =
            contract_case(command, &args, false, &files, fail_if_approval).await;
        assert_eq!(outcome.approvals, 0, "{command} {args:?} must not prompt");
        assert_eq!(
            tool_end_status(&outcome),
            Some(ActivityStatus::Success),
            "{command} {args:?} must execute successfully: {content}"
        );
        for expected in expected_output {
            assert!(
                content.contains(expected),
                "{command} {args:?} must return {expected:?}: {content}"
            );
        }
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
async fn sed_auto_runs_for_editors_and_denies_for_read_only() {
    // Editor phase unchanged: sed without an executable script auto-runs via
    // the editor override (can_edit => allow), so no approval event.
    let editor = embedded_policy_shell_case(
        "sed",
        &["-n", "1,2p", "f.txt"],
        true,
        Some(("f.txt", "one\ntwo\n")),
        fail_if_approval,
    )
    .await;
    assert_eq!(editor.approvals, 0, "editor sed should auto-run");

    // Read-only phase (special verification, probed against the wiring):
    // sed is interpreter-class, so the new contract is a HARD DENY — no
    // approval event (it neither auto-runs nor prompts), the turn completes,
    // and the tool result carries the "read-only agent" message.
    let (reader, content) = contract_case(
        "sed",
        &["-n", "1,2p", "f.txt"],
        false,
        &[("f.txt", "one\ntwo\n")],
        fail_if_approval,
    )
    .await;
    assert_eq!(
        reader.approvals, 0,
        "read-only sed must hard-deny without prompting"
    );
    assert_eq!(
        tool_end_status(&reader),
        Some(ActivityStatus::Error),
        "read-only sed is a tool error, not an execution"
    );
    assert!(
        content.contains("read-only agent"),
        "deny must carry the read-only contract message: {content}"
    );
}

#[tokio::test]
async fn wrapped_and_path_forms_still_prompt() {
    // These vectors pin the classifier/tier prompt contract, so use the old
    // catch-all-ask semantics explicitly rather than coupling them to the
    // embedded allow-all policy. The agent is can_edit=true so the prompt
    // contract stays under test; read-only denies are covered separately.
    let ask_policy = old_ask_policy(r#""cargo --version*":"allow""#);
    let prompt_cases = [
        // Catch-all `*` plus the classifier prompts editors for this command.
        ("cargo", vec!["publish", "--dry-run"]),
        // Wrapped script with an ask-rule segment; the detail must name it.
        ("bash", vec!["-c", "cargo --version; npm install x"]),
        // The leading-cd script case moved to
        // wrapped_cd_inside_runs_and_nonexistent_denies: it is now modeled
        // and runs without approval (a cd-feature change, not a policy flip).
        // Unparseable substitution falls back to whole-invocation approval.
        ("bash", vec!["-c", "cargo --version $(whoami)"]),
        // Unparseable redirect falls back to whole-invocation approval.
        ("bash", vec!["-c", "cargo --version > out.txt"]),
        // Non-normalized wrapper path stays script-driven (always approved).
        ("/tmp/y/bash", vec!["-c", "ls"]),
        // A shell invoked without `-c` still executes a script file (the
        // interpreter gate covers it) — prompts editors.
        ("bash", vec!["script.sh"]),
        ("bash", vec!["python3", "x.py"]),
        // sed with a script-execution flag stays gated for editors too.
        ("sed", vec!["s/a/b/e", "f.txt"]),
        // Non-normalized path whose basename the classifier cannot clear
        // (replaces the old ./ls / ./pwd prompt vectors, which are now
        // classifier-safe reads — see the run_cases loop below).
        ("./some-unknown-tool", vec![]),
        ("./mkdir", vec!["made-dir"]),
    ];
    for (command, args) in prompt_cases {
        let (outcome, _) =
            contract_case_with_policy(command, &args, true, &[("f.txt", "a\n")], Some(&ask_policy), |_| {
                Decision::Reject
            })
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
    // Rule (iii) else-branch (expectation flip, documented): these forms used
    // to prompt, but the new contract parses wrapped globs and classifies by
    // basename, so `ls`/`pwd`-equivalent invocations are classifier-safe reads
    // that run for every agent without approval.
    let run_cases = [
        // Globs now parse inside -c scripts; the `ls` segment is safe.
        ("bash", vec!["-c", "ls *"]),
        // Basename classification: `./ls` is judged as `ls` (safe read).
        ("./ls", vec![]),
        ("./pwd", vec![]),
    ];
    for (command, args) in run_cases {
        let (outcome, content) =
            contract_case(command, &args, true, &[("f.txt", "a\n")], fail_if_approval).await;
        assert_eq!(
            outcome.approvals, 0,
            "{command} {args:?} must run without approval"
        );
        assert!(
            !content.contains("not a permitted read operation"),
            "{command} {args:?} must not be denied: {content}"
        );
    }

    // The shipped allow-all policy still asks for package publication even
    // when cargo is invoked through a non-normalized path: basename-specific
    // ask rules remain applicable to that command.
    let shipped_path = embedded_policy_shell_case(
        "/tmp/y/cargo",
        &["publish", "--dry-run"],
        true,
        None,
        |_| Decision::Reject,
    )
    .await;
    assert_eq!(shipped_path.approvals, 1, "non-normalized cargo publish must prompt");
}

#[tokio::test]
async fn wrapped_cd_inside_runs_and_nonexistent_denies() {
    let (inside, inside_content) = contract_case(
        "bash",
        &["-c", "cd src && ls"],
        false,
        &[("src/note.txt", "present\n")],
        fail_if_approval,
    )
    .await;
    assert_eq!(inside.approvals, 0, "inside-workspace cd must not prompt");
    assert_eq!(
        tool_end_status(&inside),
        Some(ActivityStatus::Success),
        "inside-workspace cd should run: {inside_content}"
    );

    let (missing, missing_content) = contract_case(
        "bash",
        &["-c", "cd nosuchdir && ls"],
        false,
        &[],
        fail_if_approval,
    )
    .await;
    assert_eq!(missing.approvals, 0, "invalid cd must deny without prompting");
    assert_eq!(
        tool_end_status(&missing),
        Some(ActivityStatus::Error),
        "invalid cd should be a tool error: {missing_content}"
    );
    assert!(
        missing_content.contains("cd target does not exist"),
        "denial should explain the missing cd target: {missing_content}"
    );
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
    // Rule (i): subject is which prompting commands are offered the `Press p`
    // session-family grant; explicit ask rules and classifier prompts now only
    // apply to edit-capable agents (read-only agents are hard-denied before
    // any prompt). Both phases move to can_edit=true; every assertion is
    // unchanged.
    let normalized =
        embedded_policy_shell_case("cargo", &["publish", "--dry-run"], true, None, |_| {
            Decision::Reject
        })
        .await;
    assert_eq!(normalized.approvals, 1);
    assert!(
        normalized.details[0].contains("Press p to allow"),
        "normalized command should offer a session grant: {}",
        normalized.details[0]
    );

    let relative =
        embedded_policy_shell_case("./cargo", &["publish", "--dry-run"], true, None, |_| {
            Decision::Reject
        })
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
    can_edit: bool,
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
            // Read-only for the auto-run vectors; edit-capable for the
            // ask-rule vectors (explicit ask rules now only prompt
            // edit-capable agents — hard-deny for read-only, rule i).
            "can_edit": can_edit,
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
    let outcome = embedded_find_case("find", &[".", "-name", "x"], false, fail_if_approval).await;
    assert_eq!(outcome.approvals, 0, "read-only find must auto-run");
}

#[tokio::test]
async fn find_dangerous_actions_still_prompt() {
    // Rule (i): subject is the infix ask-rule prompt mechanics (matched glob
    // named in the detail); explicit ask rules now prompt only edit-capable
    // agents. Every assertion is unchanged.
    let outcome = embedded_find_case("find", &[".", "-delete"], true, |_| Decision::Reject).await;
    assert_eq!(outcome.approvals, 1, "find -delete must prompt");
    // Reason format renamed under the unified decision table (`rule "..."
    // requires approval` instead of the legacy `bash permission rule ...`
    // prefix); the intent — naming the matched infix rule — is unchanged.
    assert!(
        outcome.details[0].contains("rule \"find * -delete*\" requires approval"),
        "approval detail must name the matched infix rule, got: {}",
        outcome.details[0]
    );
}

#[tokio::test]
async fn wrapped_find_script_runs_per_command() {
    let outcome = embedded_find_case(
        "bash",
        &["-c", "find . -name x && ls"],
        false,
        fail_if_approval,
    )
    .await;
    assert_eq!(outcome.approvals, 0, "read-only wrapped find must auto-run");
}

#[tokio::test]
async fn wrapped_find_ask_segment_prompts() {
    // Rule (i): subject is the per-segment ask evaluation inside a wrapped
    // script; ask rules now prompt only edit-capable agents. Every assertion
    // (segment named, matched rule named) is unchanged.
    let outcome = embedded_find_case(
        "bash",
        &["-c", "find . -name x && find . -delete"],
        true,
        |_| Decision::Reject,
    )
    .await;
    assert_eq!(outcome.approvals, 1, "wrapped find -delete must prompt");
    assert!(
        outcome.details[0].contains("find . -delete — rule \"find * -delete*\" requires approval"),
        "approval detail must name the asking segment and matched rule, got: {}",
        outcome.details[0]
    );
}

// ---------------------------------------------------------------------------
// Mission 2: engine tests pinning the new approval contract.
//
// Conventions: a system-temp `tempfile::tempdir()` workspace (never the repo
// or target dir), an EMPTY config_dir tempdir so the embedded
// bash-permissions policy applies, mock-server tool_call/answer fixtures,
// the approval-panic `drive_turn` guard for no-approval cases, and
// Approval-then-Reject for prompt cases. Workspace files are pre-created so
// classifier-safe reads actually execute.
// ---------------------------------------------------------------------------

/// Drive one shell tool call against the embedded policy and return the
/// engine outcome plus the serialized tool-result text the model sees in the
/// follow-up request. The config_dir is a separate EMPTY tempdir so the
/// embedded default policy (catch-all ask + safe-read allow rules) governs,
/// exactly as a fresh install would see. `files` entries may contain
/// subdirectories (`sub/note.txt`); parent directories are created.
async fn contract_case(
    command: &str,
    args: &[&str],
    can_edit: bool,
    files: &[(&str, &str)],
    on_approval: impl Fn(usize) -> Decision,
) -> (RunOutcome, String) {
    contract_case_with_policy(command, args, can_edit, files, None, on_approval).await
}

/// Run a contract case with an explicit policy in the otherwise-empty config
/// directory. This keeps tests of classifier/tier behavior independent from
/// changes to the embedded shipped policy.
async fn contract_case_with_policy(
    command: &str,
    args: &[&str],
    can_edit: bool,
    files: &[(&str, &str)],
    policy: Option<&str>,
    on_approval: impl Fn(usize) -> Decision,
) -> (RunOutcome, String) {
    let mut server = server(vec![
        tool_call("shell", json!({"command":command,"args":args})),
        answer("done"),
    ])
    .await;
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    for (path, contents) in files {
        let full = workspace.path().join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&full, contents).unwrap();
    }
    let mut test_config = config(&server.url, workspace.path());
    std::fs::remove_file(workspace.path().join("bash-permissions.json")).unwrap();
    test_config.config_dir = config_dir.path().into();
    if let Some(policy) = policy {
        write_policy(config_dir.path(), policy);
    }
    test_config.agents.insert(
        "contract-agent".into(),
        serde_json::from_value(json!({"can_edit": can_edit, "tools": ["shell"]})).unwrap(),
    );
    let (engine, events) = engine(test_config);
    let outcome = drive_turn(
        engine,
        Selection {
            agent: Some("contract-agent".into()),
            ..Selection::default()
        },
        events,
        on_approval,
    )
    .await;
    server.requests.recv().await.unwrap();
    let followup: Value =
        serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let content = last_tool_message(&followup)["content"]
        .as_str()
        .unwrap()
        .to_owned();
    (outcome, content)
}

const OLD_ASK_POLICY_PREFIX: &str = r#"{"blocked_commands":["rm","rmdir","shred","mkfs","fdisk","diskutil","dd","shutdown","poweroff","reboot","halt","kill","pkill","killall","chmod","chown","mount","umount","iptables","pfctl","gcloud","az","terraform","kubectl","helm"],"blocked_patterns":["rm -rf","rm -fr","docker system prune","docker volume rm","docker rm -f","curl | sh","curl | bash","wget | sh","wget | bash","> /dev/","2>/dev/",":(){ :|:& };:","base64 -d | sh","terraform destroy","kubectl delete","kubectl apply","kubectl replace","helm uninstall"],"bash":{"*":"ask","npm install*":"ask","cargo publish*":"ask","sort -o*":"ask""#;

const OLD_ASK_POLICY_SUFFIX: &str = "}}";

fn old_ask_policy(extra_rules: &str) -> String {
    let separator = if extra_rules.is_empty() { "" } else { "," };
    format!("{OLD_ASK_POLICY_PREFIX}{separator}{extra_rules}{OLD_ASK_POLICY_SUFFIX}")
}

#[tokio::test]
async fn read_only_agent_denials_carry_contract_message() {
    // Pin the legacy catch-all-ask denial wording/rule cause independently
    // from the new embedded allow-all message shape.
    let ask_policy = old_ask_policy("");
    let (sed, sed_content) = contract_case_with_policy(
        "sed",
        &["-n", "1p", "f.txt"],
        false,
        &[("f.txt", "one\n")],
        Some(&ask_policy),
        fail_if_approval,
    )
    .await;
    assert_eq!(sed.approvals, 0, "read-only sed must not prompt");
    assert_eq!(
        tool_end_status(&sed),
        Some(ActivityStatus::Error),
        "the denial is a tool error"
    );
    assert!(
        sed_content.contains("read-only agent"),
        "deny message missing: {sed_content}"
    );

    // Network/credential CLI excluded from the classifier fallback: even
    // though `npm install x` matches an explicit ask rule, a read-only agent
    // is denied pre-prompt.
    let (npm, npm_content) = contract_case_with_policy(
        "npm",
        &["install", "x"],
        false,
        &[],
        Some(&ask_policy),
        fail_if_approval,
    )
    .await;
    assert_eq!(npm.approvals, 0, "read-only npm install must not prompt");
    assert!(
        npm_content.contains("read-only agent") && npm_content.contains("npm install*"),
        "deny must carry the contract message and name the rule: {npm_content}"
    );

    // Classifier-unsafe `sort` form (`-o` writes its output file): denied
    // for read-only agents even though plain `sort` runs for everyone.
    let (sort, sort_content) = contract_case_with_policy(
        "sort",
        &["-o", "out", "f"],
        false,
        &[("f", "2\n1\n")],
        Some(&ask_policy),
        fail_if_approval,
    )
    .await;
    assert_eq!(sort.approvals, 0, "read-only sort -o must not prompt");
    assert_eq!(tool_end_status(&sort), Some(ActivityStatus::Error));
    // The explicit ask policy reproduces the pre-flip catch-all semantics (the
    // last-matching rule), not the bare classifier wording. The serialized
    // tool content escapes quotes, so assert on quote-free fragments: the
    // contract message plus the cause suffix naming the rule.
    assert!(
        sort_content.contains("read-only agent") && sort_content.contains("requires approval"),
        "deny must carry the contract message and the cause: {sort_content}"
    );

    // Under the shipped allow-all default there is no matching rule to quote;
    // the classifier denial instead identifies the unsafe form itself.
    let (embedded, embedded_content) =
        contract_case("sort", &["-o", "out", "f"], false, &[], fail_if_approval).await;
    assert_eq!(embedded.approvals, 0, "embedded policy must still deny sort -o");
    assert_eq!(tool_end_status(&embedded), Some(ActivityStatus::Error));
    assert!(
        embedded_content.contains("not classifier-safe"),
        "allow-all denial must explain the classifier cause: {embedded_content}"
    );
}

#[tokio::test]
async fn read_only_agent_runs_classifier_safe_reads() {
    // Every vector here is a classifier-safe local read: no approval event
    // for read-only agents, for any of them. `git grep` may fail at exec
    // (no repo staged) — the decision precedes execution, so the approval
    // behavior is the assertion.
    for (command, args, files) in [
        ("git", vec!["grep", "foo"], Vec::new()),
        (
            "sort",
            vec!["-rn", "nums.txt"],
            vec![("nums.txt", "3\n1\n2\n")],
        ),
        ("echo", vec!["hi"], Vec::new()),
        ("du", vec!["-sh", "."], Vec::new()),
    ] {
        let (outcome, _content) =
            contract_case(command, &args, false, &files, fail_if_approval).await;
        assert_eq!(
            outcome.approvals, 0,
            "{command} {args:?} must run without approval for read-only agents"
        );
    }
    // The pure-read vectors that exec cleanly must reach Success, proving
    // the decision was Run (not a silent deny).
    for (command, args, files) in [
        ("echo", vec!["hi"], Vec::new()),
        (
            "sort",
            vec!["-rn", "nums.txt"],
            vec![("nums.txt", "3\n1\n2\n")],
        ),
        ("du", vec!["-sh", "."], Vec::new()),
    ] {
        let (outcome, content) =
            contract_case(command, &args, false, &files, fail_if_approval).await;
        assert_eq!(
            tool_end_status(&outcome),
            Some(ActivityStatus::Success),
            "{command} {args:?} must execute: {content}"
        );
    }
    let (_, echo_content) = contract_case("echo", &["hi"], false, &[], fail_if_approval).await;
    assert!(
        echo_content.contains("hi"),
        "echo output must reach the model: {echo_content}"
    );
}

#[tokio::test]
async fn user_pipeline_scripts_run_for_read_only_agents() {
    // Preserve the old shipped policy's allow rules for these script segments:
    // head/grep/wc/ls were explicitly allowed even where the newer strict
    // read-only classifier rejects a particular flag form.
    let allow_policy = old_ask_policy(
        r#""head*":"allow","grep*":"allow","wc*":"allow","ls*":"allow""#,
    );
    let files = [
        ("a.rs", "fn a() {}\nfn b() {}\nfn c() {}\n"),
        ("b.rs", "fn d() {}\n"),
    ];
    let (pipeline, pipeline_content) = contract_case_with_policy(
        "sh",
        &["-c", "wc -l *.rs | sort -rn | head -40"],
        false,
        &files,
        Some(&allow_policy),
        fail_if_approval,
    )
    .await;
    assert_eq!(
        pipeline.approvals, 0,
        "read-only pipeline over workspace files must not prompt"
    );
    assert!(
        pipeline_content.contains("a.rs") && pipeline_content.contains("b.rs"),
        "pipeline must actually execute over the files: {pipeline_content}"
    );

    // The production read-only script shape (parse is exercised per segment;
    // every segment must run for read-only agents).
    let f1 = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20\nl21\nl22\nl23\nl24\nl25\nl26\nl27\nl28\nl29\nl30\nl31\n";
    let f2 = "fn usage\" marker\ninclude_usage marker\nother\n";
    let (production, production_content) = contract_case_with_policy(
        "sh",
        &[
            "-c",
            "head -30 f1; echo ---; ls sub; echo ---; grep -rn 'usage\"' f2 | head -3; grep -rn 'include_usage' f2",
        ],
        false,
        &[("f1", f1), ("f2", f2), ("sub/note.txt", "notes\n")],
        Some(&allow_policy),
        fail_if_approval,
    )
    .await;
    assert_eq!(
        production.approvals, 0,
        "the production read-only script shape must not prompt"
    );
    assert!(
        production_content.contains("include_usage") && production_content.contains("note.txt"),
        "the production script must actually execute (ls sub lists note.txt): {production_content}"
    );
}

#[tokio::test]
async fn editors_keep_prompting_for_unsafe_and_run_safe() {
    // Edit-capable agents keep the prompt for classifier-unsafe commands...
    let (npm, npm_content) =
        contract_case("npm", &["install", "x"], true, &[], |_| Decision::Reject).await;
    assert_eq!(npm.approvals, 1, "editor npm install must prompt");
    assert!(
        npm_content.contains("Tool rejected by user"),
        "rejection must surface to the model: {npm_content}"
    );
    assert_eq!(
        tool_end_status(&npm),
        Some(ActivityStatus::Denied),
        "a rejected prompt is a user denial, not an error"
    );

    // ...while classifier-safe reads (sort without -o) still auto-run...
    let (sort, sort_content) = contract_case(
        "sort",
        &["-rn", "nums.txt"],
        true,
        &[("nums.txt", "3\n1\n2\n")],
        fail_if_approval,
    )
    .await;
    assert_eq!(sort.approvals, 0, "editor sort must not prompt");
    assert_eq!(
        tool_end_status(&sort),
        Some(ActivityStatus::Success),
        "editor sort must execute: {sort_content}"
    );

    // ...and the sed editor override keeps auto-running non-executing sed.
    let (sed, sed_content) = contract_case(
        "sed",
        &["-n", "1p", "f.txt"],
        true,
        &[("f.txt", "one\n")],
        fail_if_approval,
    )
    .await;
    assert_eq!(
        sed.approvals, 0,
        "editor sed override must suppress the prompt"
    );
    assert_eq!(
        tool_end_status(&sed),
        Some(ActivityStatus::Success),
        "editor sed must execute: {sed_content}"
    );
    assert!(
        sed_content.contains("one"),
        "sed output must reach the model: {sed_content}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn outside_reads_still_prompt_for_read_only() {
    // The outside-workspace gate is a READ gate that survives the deny
    // semantics: it prompts for read-only agents too (then the rejection
    // prevents the read).
    #[cfg(not(unix))]
    return;
    #[cfg(unix)]
    {
        let outside = tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/target")).unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "outside-data").unwrap();
        let secret = secret.to_string_lossy().into_owned();
        let (outcome, content) =
            contract_case("ls", &[&secret], false, &[], |_| Decision::Reject).await;
        assert_eq!(
            outcome.approvals, 1,
            "outside reads must prompt even for read-only agents"
        );
        assert!(
            outcome.details[0].contains("outside the configured workspace"),
            "outside banner must be present: {}",
            outcome.details[0]
        );
        assert_eq!(
            outcome.persist_allowed,
            vec![true],
            "grantable outside reads must offer the session directory grant"
        );
        assert!(
            content.contains("Tool rejected by user"),
            "rejection must surface to the model: {content}"
        );
    }
}

#[tokio::test]
async fn multiword_command_fails_fast_without_prompt() {
    // `command` is executed as argv without a shell, so a multi-word value
    // ("ls -l") can never run. It must fail as a tool error before any
    // approval prompt — here probed with a read-only agent to show the
    // validation precedes even the deny table.
    let (outcome, content) = contract_case("ls -l", &[], false, &[], fail_if_approval).await;
    assert_eq!(outcome.approvals, 0, "multi-word command must not prompt");
    assert_eq!(
        tool_end_status(&outcome),
        Some(ActivityStatus::Error),
        "the validation failure is a tool error"
    );
    assert!(
        content.contains("single executable"),
        "error must explain the single-executable contract: {content}"
    );
}

#[tokio::test]
async fn aws_credential_calls_never_auto_run() {
    // Read-only agent: aws (a network/credential CLI) is excluded from the
    // classifier fallback, so even its `get-token` form is a hard deny.
    let (denied, denied_content) =
        contract_case("aws", &["eks", "get-token"], false, &[], fail_if_approval).await;
    assert_eq!(denied.approvals, 0, "read-only aws must not prompt");
    assert_eq!(
        tool_end_status(&denied),
        Some(ActivityStatus::Error),
        "the denial is a tool error (decisions precede execution)"
    );
    assert!(
        denied_content.contains("read-only agent"),
        "deny must carry the contract message: {denied_content}"
    );

    // Edit-capable agent: the same call prompts, and the rejection prevents
    // the call from ever running (aws need not exist — Reject stops it).
    let (prompted, prompted_content) =
        contract_case("aws", &["eks", "get-token"], true, &[], |_| {
            Decision::Reject
        })
        .await;
    assert_eq!(prompted.approvals, 1, "editor aws must prompt");
    assert!(
        prompted_content.contains("Tool rejected by user"),
        "rejection must surface to the model: {prompted_content}"
    );
}

#[tokio::test]
async fn wrapped_script_read_only_deny_names_offending_segment() {
    // A wrapped script whose deny comes from ONE segment fails the whole
    // call before any approval event, and the message names the segment.
    let (outcome, content) = contract_case(
        "bash",
        &["-c", "wc -l f && npm install x"],
        false,
        &[("f", "1\n2\n")],
        fail_if_approval,
    )
    .await;
    assert_eq!(outcome.approvals, 0, "wrapped deny must not prompt");
    assert_eq!(
        tool_end_status(&outcome),
        Some(ActivityStatus::Error),
        "the wrapped deny is a tool error"
    );
    assert!(
        content.contains("read-only agent"),
        "deny must carry the contract message: {content}"
    );
    assert!(
        content.contains("npm install"),
        "deny must name the offending npm segment: {content}"
    );
}

#[tokio::test]
async fn user_case_rustfmt_check_runs_for_everyone() {
    let args = ["--edition", "2021", "--check", "src/x.rs"];
    let (editor, _) = contract_case("rustfmt", &args, true, &[], fail_if_approval).await;
    assert_eq!(
        editor.approvals, 0,
        "editor rustfmt --check must not prompt"
    );

    // rustfmt may not be installed in every test environment. The policy
    // decision happens before execution, so pin no prompt and no read-only
    // denial rather than requiring a successful process exit.
    let (reader, content) = contract_case("rustfmt", &args, false, &[], fail_if_approval).await;
    assert_eq!(
        reader.approvals, 0,
        "read-only rustfmt --check must not prompt"
    );
    assert!(
        !content.contains("read-only agent"),
        "rustfmt --check must not be denied: {content}"
    );
}

#[tokio::test]
async fn user_case_login_shell_cargo_check_runs_for_editor_denies_read_only() {
    // `-l` sources the real user profile via the passed-through HOME. These
    // commands are harmless; assertions pin the policy decision, not build
    // output or the presence of a Cargo.toml.
    let args = ["-lc", "cargo check --locked --lib 2>&1 | tail -30"];
    let (editor, _) = contract_case("bash", &args, true, &[], fail_if_approval).await;
    assert_eq!(
        editor.approvals, 0,
        "editor cargo check pipeline must not prompt"
    );

    let (reader, content) = contract_case("bash", &args, false, &[], fail_if_approval).await;
    assert_eq!(reader.approvals, 0, "read-only cargo check must not prompt");
    assert_eq!(tool_end_status(&reader), Some(ActivityStatus::Error));
    assert!(
        content.contains("read-only agent"),
        "read-only cargo check must be denied: {content}"
    );
}

#[tokio::test]
async fn user_case_blocked_script_text_denies_without_prompt() {
    let args = [
        "-lc",
        "rm -rf /tmp/ds-scratch && mkdir -p /tmp/ds-scratch && cd /tmp/ds-scratch && cargo test --quiet 2>&1 | tail -25",
    ];
    let (editor, editor_content) = contract_case("bash", &args, true, &[], fail_if_approval).await;
    assert_eq!(
        editor.approvals, 0,
        "blocked script text must not prompt an editor"
    );
    assert_eq!(tool_end_status(&editor), Some(ActivityStatus::Error));
    assert!(
        editor_content.contains("Blocked"),
        "blocked script scan must report Blocked: {editor_content}"
    );

    let (reader, reader_content) = contract_case("bash", &args, false, &[], fail_if_approval).await;
    assert_eq!(
        reader.approvals, 0,
        "blocked script text must not prompt a reader"
    );
    assert_eq!(tool_end_status(&reader), Some(ActivityStatus::Error));
    assert!(
        reader_content.contains("Blocked"),
        "blocked script scan must report Blocked before read-only denial: {reader_content}"
    );
}

#[tokio::test]
async fn user_case_login_shell_glob_pipeline_runs() {
    let args = [
        "-lc",
        "ls *.txt 2>/dev/null | head; ls sub/*.md 2>/dev/null | head",
    ];
    let files = [("a.txt", "a\n"), ("b.txt", "b\n"), ("sub/c.md", "c\n")];
    for can_edit in [true, false] {
        let (outcome, content) =
            contract_case("bash", &args, can_edit, &files, fail_if_approval).await;
        assert_eq!(
            outcome.approvals, 0,
            "login-shell glob pipeline must not prompt"
        );
        assert_eq!(
            tool_end_status(&outcome),
            Some(ActivityStatus::Success),
            "login-shell glob pipeline must execute: {content}"
        );
        assert!(
            content.contains("a.txt") && content.contains("c.md"),
            "glob pipeline must list the pre-created files: {content}"
        );
    }
}

#[tokio::test]
async fn tier_pins_editor() {
    // The decision is asserted independently from whether the command can
    // complete in a minimal temporary workspace (for example, cargo test has
    // no manifest here). Explicit rules reproduce the old catch-all-ask tier
    // distinctions rather than inheriting the embedded allow-all default.
    let tier_policy = old_ask_policy(
        r#""cargo test*":"allow","python3 -m pytest*":"allow","cargo --version*":"allow","python3 --version*":"allow","cargo run*":"ask","python3 bench.py*":"ask","make*":"ask","awk*":"ask""#,
    );
    for (command, args, files) in [
        ("cargo", vec!["test"], Vec::new()),
        ("python3", vec!["-m", "pytest"], Vec::new()),
        ("cargo", vec!["--version"], Vec::new()),
        ("python3", vec!["--version"], Vec::new()),
    ] {
        let (outcome, content) = contract_case_with_policy(
            command,
            &args,
            true,
            &files,
            Some(&tier_policy),
            fail_if_approval,
        )
        .await;
        assert_eq!(
            outcome.approvals, 0,
            "editor {command} {args:?} must run: {content}"
        );
        assert!(
            !content.contains("read-only agent"),
            "editor {command} {args:?} must not be denied: {content}"
        );
    }

    for (command, args, files) in [
        ("cargo", vec!["run"], Vec::new()),
        ("python3", vec!["bench.py"], Vec::new()),
        ("make", Vec::new(), Vec::new()),
        ("awk", vec!["NR>=1{print}", "f"], vec![("f", "line\n")]),
    ] {
        let (outcome, content) = contract_case_with_policy(
            command,
            &args,
            true,
            &files,
            Some(&tier_policy),
            |_| Decision::Reject,
        )
        .await;
        assert_eq!(
            outcome.approvals, 1,
            "editor {command} {args:?} must prompt"
        );
        assert_eq!(tool_end_status(&outcome), Some(ActivityStatus::Denied));
        assert!(
            content.contains("Tool rejected by user"),
            "prompt rejection must be returned for {command} {args:?}: {content}"
        );
    }
}

#[tokio::test]
async fn tier_pins_read_only() {
    for (command, args, files) in [
        ("cargo", vec!["--version"], Vec::new()),
        ("python3", vec!["--version"], Vec::new()),
        ("rustfmt", vec!["--check", "f"], vec![("f", "fn f() {}\n")]),
    ] {
        let (outcome, content) =
            contract_case(command, &args, false, &files, fail_if_approval).await;
        assert_eq!(
            outcome.approvals, 0,
            "read-only {command} {args:?} must run"
        );
        assert!(
            !content.contains("read-only agent"),
            "read-only {command} {args:?} must not be denied: {content}"
        );
    }

    for (command, args, files) in [
        ("cargo", vec!["test"], Vec::new()),
        ("python3", vec!["-m", "pytest"], Vec::new()),
        // `python3 -c` is a dev-tier form, not a query that runs read-only.
        // Keep the removed legacy test case covered by the same deny contract.
        ("python3", vec!["-c", "print('a b')"], Vec::new()),
        ("make", Vec::new(), Vec::new()),
        ("awk", vec!["NR>=1{print}", "f"], vec![("f", "line\n")]),
    ] {
        let (outcome, content) =
            contract_case(command, &args, false, &files, fail_if_approval).await;
        assert_eq!(
            outcome.approvals, 0,
            "read-only {command} {args:?} must not prompt"
        );
        assert_eq!(tool_end_status(&outcome), Some(ActivityStatus::Error));
        assert!(
            content.contains("read-only agent"),
            "read-only {command} {args:?} must be denied: {content}"
        );
    }
}

#[tokio::test]
async fn python_and_cargo_query_forms_actually_execute() {
    for (command, args, expected_output) in [
        ("cargo", vec!["--version"], "cargo"),
        ("python3", vec!["--version"], "Python"),
    ] {
        let (outcome, content) = contract_case(command, &args, true, &[], fail_if_approval).await;
        assert_eq!(outcome.approvals, 0, "{command} {args:?} must not prompt");
        assert_eq!(
            tool_end_status(&outcome),
            Some(ActivityStatus::Success),
            "{command} {args:?} must execute successfully: {content}"
        );
        assert!(
            content.contains(expected_output),
            "{command} {args:?} must return non-error version output: {content}"
        );
    }
}

// `/usr/bin/env` in argv used to trip the outside-path scan; these assertions also pin that banner regression.
#[tokio::test]
async fn duplicated_env_launcher_is_grouped_before_policy_evaluation() {
    let (normalized, normalized_content) = contract_case(
        "/usr/bin/env",
        &["/usr/bin/env", "sed", "-n", "1,2p", "f.txt"],
        true,
        &[("f.txt", "one\ntwo\n")],
        fail_if_approval,
    )
    .await;
    assert_eq!(normalized.approvals, 0, "duplicated env + sed should auto-run");
    assert_eq!(
        tool_end_status(&normalized),
        Some(ActivityStatus::Success),
        "normalized sed should execute successfully: {normalized_content}"
    );
    for detail in &normalized.details {
        assert!(
            !detail.contains("outside the configured workspace"),
            "approval detail must not contain the outside-workspace banner: {detail}"
        );
        assert!(
            !detail.contains("hides the real command"),
            "approval detail must not claim the launcher hides the command: {detail}"
        );
    }
    assert!(
        !normalized_content.contains("outside the configured workspace")
            && !normalized_content.contains("hides the real command"),
        "tool result must not contain launcher/path false positives: {normalized_content}"
    );

    let (script_driven, _) = contract_case(
        "/usr/bin/env",
        &["/usr/bin/env", "python3", "-c", "print(1)"],
        true,
        &[],
        |_| Decision::Reject,
    )
    .await;
    assert_eq!(script_driven.approvals, 1, "python3 -c must remain gated");
    assert!(
        !script_driven.details[0].contains("hides the real command"),
        "approval should be for script-driven execution, not a hidden command: {}",
        script_driven.details[0]
    );
    assert!(
        script_driven.details[0].contains("script-driven"),
        "approval should carry the script-driven justification: {}",
        script_driven.details[0]
    );
    assert!(
        !script_driven.details[0].contains("outside the configured workspace"),
        "approval must not contain the outside-workspace banner: {}",
        script_driven.details[0]
    );

    let (assigned_env, _) = contract_case(
        "/usr/bin/env",
        &["/usr/bin/env", "FOO=1", "python3", "-c", "print(1)"],
        true,
        &[],
        |_| Decision::Reject,
    )
    .await;
    assert_eq!(
        assigned_env.approvals,
        1,
        "env with variable assignments must remain wrapped and require approval"
    );
    assert!(
        assigned_env.details[0].contains("wrapper/launcher"),
        "env with variable assignments must keep the launcher justification: {}",
        assigned_env.details[0]
    );
}
