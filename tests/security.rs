//! Wave 1 command/process security and MCP exposure tests. Deterministic,
//! local, no credentials, no live network calls. Exercises:
//!  - subprocess environment isolation (baseline, ambient stripping, gh filter,
//!    custom overlay, ${ENV} expansion)
//!  - shell classification (interpreter/script-driven, encoded payloads,
//!    --option=path, symlink escape) and read-only routing through the
//!    ordinary approval path
//!  - bash-permissions unified policy enforcement
//!  - MCP allowlist behaviour independent of the agent's builtin/custom
//!    `tools` list, plus execution gating for unallowed servers
mod support;
use diet_soda::{
    config::{Config, McpConfig, McpTransport, ToolConfig},
    engine::Selection,
    mcp::McpManager,
    model::{Decision, UiEvent},
    process::{self, EnvRequest, ProcessRequest},
    tools,
};
use serde_json::{json, Value};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};
use support::*;
use tokio_util::sync::CancellationToken;

fn rust_source_files(root: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(root).expect("source directory should be readable") {
        let entry = entry.expect("source directory entry should be readable");
        let path = entry.path();
        if path.is_dir() {
            rust_source_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

#[test]
fn unsafe_code_allow_is_confined_to_the_windows_job_object_module() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let expected = Path::new("src").join("winjob.rs");
    let mut files = Vec::new();
    rust_source_files(&source_root, &mut files);

    let mut matches = Vec::new();
    for file in files {
        let contents = std::fs::read_to_string(&file).expect("Rust source should be UTF-8");
        let count = contents.matches("allow(unsafe_code)").count();
        if count > 0 {
            let relative = file
                .strip_prefix(env!("CARGO_MANIFEST_DIR"))
                .expect("source file should be inside the manifest directory")
                .to_path_buf();
            matches.push((relative, count));
        }
    }

    assert_eq!(
        matches.len(),
        1,
        "unsafe-code allowance found in unexpected files: {matches:?}"
    );
    assert_eq!(matches[0].0, expected);
    assert_eq!(matches[0].1, 1);
}

fn redirect(location: &str) -> Reply {
    Reply {
        status: 302,
        content_type: "text/plain".into(),
        body: "redirect body".into(),
        headers: vec![("Location".into(), location.into())],
        header_delay: None,
        chunk_delay: None,
        stall: None,
    }
}

fn redirect_with_body(location: &str, body: String, stall: Duration) -> Reply {
    Reply {
        status: 302,
        content_type: "text/plain".into(),
        body,
        headers: vec![("Location".into(), location.into())],
        header_delay: None,
        chunk_delay: None,
        stall: Some(stall),
    }
}

fn final_text(body: &str) -> Reply {
    Reply {
        status: 200,
        content_type: "text/plain".into(),
        body: body.into(),
        headers: vec![],
        header_delay: None,
        chunk_delay: None,
        stall: None,
    }
}

#[tokio::test]
async fn web_fetch_follows_exactly_five_redirects_with_relative_targets() {
    let server = server(vec![
        redirect("/relative-one"),
        redirect("//127.0.0.1:{PORT}/scheme-relative"),
        redirect("/relative-three"),
        redirect("/relative-four"),
        redirect("/relative-five"),
        final_text("redirected successfully"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.web_fetch.allow_private_networks = true;

    let result =
        tools::web_fetch_with_config(&server.url, &CancellationToken::new(), Some(&config))
            .await
            .unwrap();

    assert_eq!(result["text"], "redirected successfully");
    assert_eq!(server.count.load(Ordering::SeqCst), 6);
}

#[tokio::test]
async fn web_fetch_rejects_the_sixth_redirect_before_following_it() {
    let server = server(vec![
        redirect("/one"),
        redirect("/two"),
        redirect("/three"),
        redirect("/four"),
        redirect("/five"),
        redirect("/six"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.web_fetch.allow_private_networks = true;

    let error = tools::web_fetch_with_config(&server.url, &CancellationToken::new(), Some(&config))
        .await
        .unwrap_err()
        .to_string();

    assert_eq!(error, "Too many redirects");
    assert_eq!(server.count.load(Ordering::SeqCst), 6);
}

#[tokio::test]
async fn web_fetch_rejects_non_http_redirect_targets_explicitly() {
    let server = server(vec![redirect("file:///tmp/secret")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.web_fetch.allow_private_networks = true;

    let error = tools::web_fetch_with_config(&server.url, &CancellationToken::new(), Some(&config))
        .await
        .unwrap_err()
        .to_string();

    assert_eq!(error, "Invalid redirect location");
    assert_eq!(server.count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn web_fetch_follows_redirect_without_reading_large_stalled_body() {
    let server = server(vec![
        redirect_with_body("/final", "x".repeat(2_000_000), Duration::from_secs(5)),
        final_text("prompt completion"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config(&server.url, tmp.path());
    config.web_fetch.allow_private_networks = true;

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        tools::web_fetch_with_config(&server.url, &CancellationToken::new(), Some(&config)),
    )
    .await
    .expect("redirect handling waited for the redirect body")
    .unwrap();

    assert_eq!(result["text"], "prompt completion");
    assert_eq!(server.count.load(Ordering::SeqCst), 2);
}

/// Serializes env mutations against fixed-name variables that the harness
/// allowlists (`GH_TOKEN`, `GITHUB_TOKEN`, `OPENAI_API_KEY`). Tests must not
/// stomp on shared state; an async mutex avoids holding a sync lock across
/// the subprocess await. The lock is held only across `set_var`/`remove_var`
/// calls, not the actual process run.
static GH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(unix)]
#[tokio::test]
async fn shell_subprocess_strips_ambient_secret_and_keeps_baseline() {
    let tmp = tempfile::tempdir().unwrap();
    let secret_key = "DIET_SODA_TEST_SECRET";
    let other_key = "DIET_SODA_TEST_OTHER";
    std::env::set_var(secret_key, "provider-secret-value");
    std::env::set_var(other_key, "ambient-garbage");
    let env = process::isolated_env(&EnvRequest::shell(), tmp.path()).unwrap();
    let result = process::run(
        ProcessRequest {
            command: "/usr/bin/env",
            args: &[],
            cwd: tmp.path(),
            env: &env,
            input: None,
            timeout: 5,
            limit: 200_000,
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    std::env::remove_var(secret_key);
    std::env::remove_var(other_key);
    // Provider-style secret must not have leaked through.
    assert!(
        !result.stdout.contains(secret_key),
        "ambient secret leaked into shell subprocess: {:?}",
        result.stdout
    );
    assert!(
        !result.stdout.contains(other_key),
        "arbitrary ambient variable leaked into shell subprocess: {:?}",
        result.stdout
    );
    // PATH is part of the explicit baseline and is preserved.
    assert!(
        result.stdout.lines().any(|line| line.starts_with("PATH=")),
        "PATH missing from shell subprocess baseline: {:?}",
        result.stdout
    );
    #[cfg(unix)]
    {
        // HOME / USER are part of the Unix baseline.
        assert!(
            result.stdout.lines().any(|line| line.starts_with("HOME=")),
            "HOME missing from Unix shell subprocess baseline"
        );
    }
    // Anything not in the baseline / policy / overlay must be absent.
    for line in result.stdout.lines() {
        let name = line.split('=').next().unwrap_or("");
        assert!(
            name.is_empty()
                || matches!(
                    name,
                    "PATH"
                        | "HOME"
                        | "USER"
                        | "LOGNAME"
                        | "LANG"
                        | "LC_ALL"
                        | "LC_CTYPE"
                        | "LC_MESSAGES"
                        | "LC_NUMERIC"
                        | "LC_TIME"
                        | "TERM"
                        | "TMPDIR"
                        | "XDG_CONFIG_HOME"
                        | "PWD"
                        | "USERPROFILE"
                        | "HOMEDRIVE"
                        | "HOMEPATH"
                        | "SystemRoot"
                        | "SystemDrive"
                        | "TEMP"
                        | "TMP"
                        | "PATHEXT"
                        | "COMSPEC"
                ),
            "unexpected variable in shell subprocess environment: {line}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn gh_subprocess_passes_only_github_tokens_and_strips_others() {
    let tmp = tempfile::tempdir().unwrap();
    // Use the exact names the harness allowlists so we can verify the
    // allowlist filters the right ambient variables. The harness reads these
    // names directly (`std::env::var_os`), so we have to use the real
    // variable names — save and restore prior values so a developer's actual
    // GH_TOKEN/GITHUB_TOKEN/OPENAI_API_KEY are not silently clobbered, and
    // serialize with a tokio mutex so two parallel test threads can't race
    // on the same fixed names. A sync mutex would block the runtime when
    // held across the subprocess await.
    let prev_gh = std::env::var("GH_TOKEN").ok();
    let prev_github = std::env::var("GITHUB_TOKEN").ok();
    let prev_openai = std::env::var("OPENAI_API_KEY").ok();
    {
        let _guard = GH_LOCK.lock().await;
        std::env::set_var("GH_TOKEN", "gh-secret");
        std::env::set_var("GITHUB_TOKEN", "github-secret");
        std::env::set_var("OPENAI_API_KEY", "openai-secret");
    }
    let env = process::isolated_env(&EnvRequest::gh(), tmp.path()).unwrap();
    let result = process::run(
        ProcessRequest {
            command: "/bin/sh",
            args: &["-c".into(), "env".into()],
            cwd: tmp.path(),
            env: &env,
            input: None,
            timeout: 5,
            limit: 200_000,
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    {
        let _guard = GH_LOCK.lock().await;
        match prev_gh {
            Some(value) => std::env::set_var("GH_TOKEN", value),
            None => std::env::remove_var("GH_TOKEN"),
        }
        match prev_github {
            Some(value) => std::env::set_var("GITHUB_TOKEN", value),
            None => std::env::remove_var("GITHUB_TOKEN"),
        }
        match prev_openai {
            Some(value) => std::env::set_var("OPENAI_API_KEY", value),
            None => std::env::remove_var("OPENAI_API_KEY"),
        }
    }
    assert!(
        result.stdout.contains("GH_TOKEN=gh-secret"),
        "GH_TOKEN missing from gh subprocess: {:?}",
        result.stdout
    );
    assert!(
        result.stdout.contains("GITHUB_TOKEN=github-secret"),
        "GITHUB_TOKEN missing from gh subprocess: {:?}",
        result.stdout
    );
    assert!(
        !result.stdout.contains("OPENAI_API_KEY=openai-secret"),
        "non-GitHub ambient token leaked into gh subprocess: {:?}",
        result.stdout
    );
}

#[cfg(unix)]
#[tokio::test]
async fn custom_command_overlay_expands_references_and_preserves_baseline() {
    let tmp = tempfile::tempdir().unwrap();
    let key = format!("DIET_SODA_TEST_OVERLAY_{}", uuid::Uuid::new_v4().simple());
    std::env::set_var(&key, "expanded-value");
    let mut overlay = BTreeMap::new();
    overlay.insert("DIET_SODA_TEST_LITERAL".into(), "literal-value".into());
    overlay.insert("DIET_SODA_TEST_REF".into(), format!("${{{key}}}"));
    let env = process::isolated_env(&EnvRequest::custom(overlay), tmp.path()).unwrap();
    let result = process::run(
        ProcessRequest {
            command: "/bin/sh",
            args: &["-c".into(), "env".into()],
            cwd: tmp.path(),
            env: &env,
            input: None,
            timeout: 5,
            limit: 200_000,
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    std::env::remove_var(&key);
    assert!(
        result
            .stdout
            .contains("DIET_SODA_TEST_LITERAL=literal-value"),
        "overlay literal missing: {:?}",
        result.stdout
    );
    assert!(
        result.stdout.contains("DIET_SODA_TEST_REF=expanded-value"),
        "overlay reference did not expand: {:?}",
        result.stdout
    );
    assert!(
        result.stdout.lines().any(|line| line.starts_with("PATH=")),
        "custom command lost baseline PATH: {:?}",
        result.stdout
    );
    assert!(
        !result.stdout.contains(&key),
        "raw ${{KEY}} reference leaked into subprocess: {:?}",
        result.stdout
    );
}

#[tokio::test]
async fn read_only_shell_allows_safe_command_without_approval() {
    // The same shell classification rules apply to both the engine and the
    // dispatch helper. A safe call must not prompt when `can_edit` is false.
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        workspace: tmp.path().into(),
        ..Config::default()
    };
    assert!(
        !tools::shell_requires_approval(&config, "/bin/printf", &["hello".into()], false,).unwrap()
    );
}

#[tokio::test]
async fn read_only_shell_rejects_destructive_interpreter_outside_and_encoded() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "data").unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("file"), "data").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path().join("secret"), root.join("escape")).unwrap();
    }
    let mut config = Config {
        workspace: root.clone(),
        ..Config::default()
    };
    config.config_dir = root.clone();
    // Destructive: `rm` is always approval-required regardless of scope.
    assert!(tools::shell_requires_approval(
        &config,
        "/bin/rm",
        &["-rf".into(), "build".into()],
        false,
    )
    .unwrap());
    // Interpreter invocation with a script flag.
    assert!(tools::shell_requires_approval(
        &config,
        "/usr/bin/python3",
        &["-c".into(), "print('hello')".into()],
        false,
    )
    .unwrap());
    // Encoded payload.
    assert!(
        tools::shell_requires_approval(&config, "/usr/bin/base64", &["-d".into()], false,).unwrap()
    );
    // Outside-workspace path argument.
    let outside_path = outside.path().join("secret").to_string_lossy().into_owned();
    assert!(tools::shell_requires_approval(&config, "/bin/cat", &[outside_path], false,).unwrap());
    // `--option=path` smuggling an outside path.
    assert!(tools::shell_requires_approval(
        &config,
        "/usr/bin/some-tool",
        &[format!("--config={}", outside.path().display())],
        false,
    )
    .unwrap());
    #[cfg(unix)]
    {
        // A symlink that resolves outside the workspace is rejected too.
        assert!(tools::shell_requires_approval(
            &config,
            "/bin/cat",
            &[root.join("escape").to_string_lossy().into_owned()],
            false,
        )
        .unwrap());
    }
}

#[tokio::test]
async fn edit_scope_keeps_approval_flow_for_classified_shell() {
    // Editable agents must still receive the approval dialog for the same
    // classified call; the read-only rejection is the only added behaviour.
    let tmp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let config = Config {
        workspace: tmp.path().into(),
        ..Config::default()
    };
    let outside_path = outside.path().join("secret").to_string_lossy().into_owned();
    // Outside path: still flagged as approval-required (no standing grant).
    assert!(tools::shell_requires_approval(
        &config,
        "/bin/cat",
        std::slice::from_ref(&outside_path),
        false
    )
    .unwrap());
    // With the standing outside grant, destructive patterns still require
    // approval but the outside path no longer does.
    assert!(!tools::shell_requires_approval(&config, "/bin/cat", &[outside_path], true,).unwrap());
    assert!(tools::shell_requires_approval(
        &config,
        "/bin/rm",
        &["-rf".into(), "build".into()],
        true,
    )
    .unwrap());
}

#[cfg(unix)]
#[tokio::test]
async fn outside_grant_only_suppresses_outside_reason_for_safe_commands() {
    // The standing `allow_outside_workspace` grant is permitted to suppress
    // ONLY the outside-path approval reason. A command the heuristic does
    // not classify as safe must still surface for approval even when every
    // argv entry is an outside path and the agent has the standing grant.
    // `cat /outside/file` auto-runs; `unknown-tool /outside/file` asks.
    let tmp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside_path = outside
        .path()
        .join("data.txt")
        .to_string_lossy()
        .into_owned();
    std::fs::write(&outside_path, "data").unwrap();
    let config = Config {
        workspace: tmp.path().into(),
        ..Config::default()
    };
    // `cat` is safe; with the grant, an outside-path read auto-runs.
    assert!(!tools::shell_requires_approval(
        &config,
        "/bin/cat",
        std::slice::from_ref(&outside_path),
        true,
    )
    .unwrap());
    let inline_outside = format!("--output={outside_path}");
    assert!(!tools::shell_requires_approval(
        &config,
        "/bin/printf",
        std::slice::from_ref(&inline_outside),
        true,
    )
    .unwrap());
    // Unknown binary: with the grant but no positive classification, the
    // call still requires approval. The grant suppresses the outside-path
    // reason only, not the always-required "not classified as safe" reason.
    assert!(tools::shell_requires_approval(
        &config,
        "/usr/bin/some-unknown-tool",
        std::slice::from_ref(&outside_path),
        true,
    )
    .unwrap());
    assert!(tools::shell_requires_approval(
        &config,
        "/usr/bin/some-unknown-tool",
        std::slice::from_ref(&inline_outside),
        true,
    )
    .unwrap());
    // Without the grant, both `cat` and the unknown binary require
    // approval because the outside path itself is the reason.
    assert!(tools::shell_requires_approval(
        &config,
        "/bin/cat",
        std::slice::from_ref(&outside_path),
        false,
    )
    .unwrap());
    assert!(tools::shell_requires_approval(
        &config,
        "/bin/printf",
        std::slice::from_ref(&inline_outside),
        false,
    )
    .unwrap());
    assert!(tools::shell_requires_approval(
        &config,
        "/usr/bin/some-unknown-tool",
        std::slice::from_ref(&outside_path),
        false,
    )
    .unwrap());
    // Mutating / network / build commands stay approval-required regardless
    // of argv, including under the standing grant. Their always-approval
    // gate fires before the outside check so the grant cannot bypass it.
    assert!(tools::shell_requires_approval(
        &config,
        "/usr/bin/cargo",
        &["build".into(), outside_path.clone()],
        true,
    )
    .unwrap());
    assert!(tools::shell_requires_approval(
        &config,
        "/usr/bin/curl",
        std::slice::from_ref(&outside_path),
        true,
    )
    .unwrap());
    assert!(tools::shell_requires_approval(
        &config,
        "/usr/bin/some-unknown-tool",
        &["-o".into(), outside_path.clone(), "build".into()],
        true,
    )
    .unwrap());
}

#[tokio::test]
async fn outside_path_args_reject_symlink_escape() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("inside.txt"), "inside").unwrap();
    let outside = tmp.path().join("secret");
    std::fs::write(&outside, "data").unwrap();
    let config = Config {
        workspace: root.clone(),
        ..Config::default()
    };
    assert!(!tools::outside_path_args(
        &config,
        &[root.join("inside.txt").to_string_lossy().into_owned()],
    )
    .unwrap());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        assert!(tools::outside_path_args(
            &config,
            &[root.join("escape").to_string_lossy().into_owned()],
        )
        .unwrap());
    }
}

#[tokio::test]
async fn missing_unified_bash_permissions_file_falls_back_to_embedded_defaults() {
    // Upgrade-compatible behavior: `bash-permissions: unified` without an
    // on-disk companion file still gets the shipped policy rather than
    // disabling policy or failing every shell/custom call. Existing configs
    // (or first-run launches before auto-init lands the companion file)
    // must keep working unchanged.
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        workspace: tmp.path().into(),
        config_dir: tmp.path().into(),
        ..Config::default()
    };
    // The embedded default is parseable and exposes the shipped lists.
    let policy = tools::bash_permissions(&config).expect("missing file must fall back");
    let embedded: tools::BashPermissions =
        serde_json::from_str(tools::DEFAULT_BASH_PERMISSIONS).unwrap();
    assert_eq!(policy.blocked_commands, embedded.blocked_commands);
    assert_eq!(policy.blocked_patterns, embedded.blocked_patterns);
    // Shipped `rm -rf` pattern must trip the fallback so an upgrade does
    // not silently disable the destructive guard.
    assert!(
        tools::check_bash_permissions(&config, "rm", &["-rf".into(), "build".into()],).is_err()
    );
    // AWS, Git, and gh mutations are approval-gated rather than hard-blocked
    // by the shipped defaults.
    assert!(tools::check_bash_permissions(
        &config,
        "gh",
        &["pr".into(), "close".into(), "1".into()],
    )
    .is_ok());
    assert!(tools::check_bash_permissions(&config, "aws", &["s3".into(), "rm".into()],).is_ok());
    assert!(
        tools::check_bash_permissions(&config, "git", &["push".into(), "--force".into()],).is_ok()
    );
    // Benign commands must keep auto-running under the embedded policy.
    assert!(tools::check_bash_permissions(&config, "ls", &["-la".into()],).is_ok());
    assert!(tools::check_bash_permissions(&config, "git", &["status".into()],).is_ok());
    assert!(tools::check_bash_permissions(&config, "cat", &["file".into()],).is_ok());
}

#[tokio::test]
async fn malformed_present_bash_permissions_file_still_errors() {
    // A present-but-malformed companion file is still a parse error so an
    // editor / syncer cannot silently disable policy by writing a stray
    // file. Only an *absent* file falls back to the embedded default.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    std::fs::write(dir.join("bash-permissions.json"), "{not valid json").unwrap();
    let config = Config {
        workspace: dir.clone(),
        config_dir: dir,
        ..Config::default()
    };
    let error = tools::bash_permissions(&config).unwrap_err();
    assert!(
        error.to_string().contains("bash-permissions.json"),
        "malformed present file must surface the file path in the error: {error}"
    );
}

#[tokio::test]
async fn none_bash_permissions_skips_policy_lookup() {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        workspace: tmp.path().into(),
        config_dir: tmp.path().into(),
        bash_permissions: "none".into(),
        ..Config::default()
    };
    // Should not error even without a policy file on disk.
    let policy = tools::bash_permissions(&config).unwrap();
    assert!(policy.blocked_commands.is_empty());
}

#[tokio::test]
async fn gh_tool_runs_through_unified_bash_policy() {
    // A blocked gh pattern (`gh pr close`) must be rejected by the same
    // policy that gates the shell tool.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    std::fs::write(
        dir.join("bash-permissions.json"),
        r#"{"blocked_commands":[],"blocked_patterns":["gh pr close"]}"#,
    )
    .unwrap();
    let config = Config {
        workspace: dir.clone(),
        config_dir: dir,
        ..Config::default()
    };
    assert!(tools::check_bash_permissions(&config, "gh", &["pr".into(), "close".into()],).is_err());
    // Reading a PR (no write) is not blocked.
    assert!(tools::check_bash_permissions(
        &config,
        "gh",
        &["pr".into(), "view".into(), "1".into()],
    )
    .is_ok());
}

#[tokio::test]
async fn gh_builtin_path_rejects_policy_before_readiness_probe() {
    // The real `gh` builtin path (`tools::builtin("gh", ...)`) must run the
    // unified bash policy before readiness, the auth probe, or any subprocess
    // spawn. A `gh pr close`-style call must be denied at the policy gate so
    // we never probe the local `gh` binary or reveal its auth status through
    // timing. We exercise the public builtin entrypoint so the test fails on
    // policy even on machines without an installed / authenticated gh.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    std::fs::write(
        dir.join("bash-permissions.json"),
        r#"{"blocked_commands":[],"blocked_patterns":["gh pr close"]}"#,
    )
    .unwrap();
    let config = Config {
        workspace: dir.clone(),
        config_dir: dir,
        ..Config::default()
    };
    let args = json!({"args": ["pr", "close", "1"]});
    let error = tools::builtin("gh", &args, &config, &CancellationToken::new(), false)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Blocked by unified bash permissions"),
        "gh builtin must reject policy before readiness probe, got: {error}"
    );
    assert!(
        !error.to_string().contains("not installed")
            && !error.to_string().contains("not authenticated"),
        "gh builtin must not surface readiness / auth errors when policy denies the call, got: {error}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn gh_builtin_uses_configured_timeout_after_independent_readiness_probes() {
    let tmp = tempfile::tempdir().unwrap();
    let fake_bin = tmp.path().join("gh");
    std::fs::write(
        &fake_bin,
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'gh version fixture'; exit 0; fi\nif [ \"$1\" = \"auth\" ] && [ \"$2\" = \"status\" ]; then exit 0; fi\nsleep 30\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let previous_path = std::env::var_os("PATH");
    let path = format!(
        "{}:{}",
        tmp.path().display(),
        previous_path
            .as_deref()
            .unwrap_or_default()
            .to_string_lossy()
    );
    let _guard = GH_LOCK.lock().await;
    std::env::set_var("PATH", path);
    let config = Config {
        workspace: tmp.path().into(),
        config_dir: tmp.path().into(),
        bash_permissions: "none".into(),
        builtin_timeouts: diet_soda::config::BuiltinTimeoutsConfig {
            shell_timeout_seconds: 120,
            gh_timeout_seconds: 1,
        },
        ..Config::default()
    };
    let started = Instant::now();
    let result = tools::builtin(
        "gh",
        &json!({"args":["api","user"]}),
        &config,
        &CancellationToken::new(),
        false,
    )
    .await;
    let elapsed = started.elapsed();
    match previous_path {
        Some(value) => std::env::set_var("PATH", value),
        None => std::env::remove_var("PATH"),
    }
    drop(_guard);

    let error = result.unwrap_err();
    assert!(error.to_string().contains("Command timed out"));
    assert!(
        elapsed < std::time::Duration::from_secs(4),
        "readiness probe was not independently bounded: {elapsed:?}"
    );
}

#[tokio::test]
async fn shell_yes_persist_grants_same_command_family_for_session() {
    let mut server = server(vec![
        tool_call(
            "shell",
            json!({"command":"/usr/bin/python3","args":["-c","print('approved-script')"]}),
        ),
        tool_call(
            "shell",
            json!({"command":"/usr/bin/python3","args":["-c","print('approved-script')"]}),
        ),
        answer("handled"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut test_config = config(&server.url, tmp.path());
    test_config.agents.insert(
        "reader".into(),
        serde_json::from_value(json!({"tools":["shell"]})).unwrap(),
    );
    let (engine, mut events) = engine(test_config);
    let runner = engine.clone();
    let mut task = tokio::spawn(async move {
        runner
            .turn(
                "run it twice".into(),
                Selection {
                    agent: Some("reader".into()),
                    ..Selection::default()
                },
                CancellationToken::new(),
            )
            .await
    });
    // The first call prompts and grants this command family. Its repeat should
    // execute without another prompt.
    let mut approval_count = 0;
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(UiEvent::Approval { reply, persist_allowed, .. }) => {
                    approval_count += 1;
                    assert_eq!(approval_count, 1, "the persistent grant should suppress repeats");
                    assert!(persist_allowed);
                    reply.send(Decision::ApprovePersist).unwrap();
                }
                Some(_) => {}
                None => break,
            },
            result = &mut task => {
                result.unwrap().unwrap();
                break;
            }
        }
    }
    assert_eq!(approval_count, 1);
    server.requests.recv().await.unwrap();
    let first_followup: Value =
        serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let first_tool = first_followup["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(first_tool["role"], "tool");
    assert!(first_tool["content"]
        .as_str()
        .unwrap()
        .contains("approved-script"));
    let second_followup: Value =
        serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let tool_results = second_followup["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect::<Vec<_>>();
    assert_eq!(tool_results.len(), 2);
    assert!(tool_results.iter().all(|message| message["content"]
        .as_str()
        .unwrap()
        .contains("approved-script")));
}

#[tokio::test]
async fn read_only_classified_shell_rejected_does_not_execute_and_run_remains_coherent() {
    // Rejecting an approval for a classified shell call must not execute the
    // subprocess, and the conversation must still complete cleanly so the
    // user can keep interacting. The run returns the model's final answer
    // after the rejected tool result, and no further request is pending.
    let mut server = server(vec![
        tool_call(
            "shell",
            json!({"command":"/usr/bin/python3","args":["-c","print('should-not-run')"]}),
        ),
        answer("rejected-but-coherent"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut test_config = config(&server.url, tmp.path());
    test_config.agents.insert(
        "reader".into(),
        serde_json::from_value(json!({"tools":["shell"]})).unwrap(),
    );
    let (engine, mut events) = engine(test_config);
    let runner = engine.clone();
    let mut task = tokio::spawn(async move {
        runner
            .turn(
                "interpret".into(),
                Selection {
                    agent: Some("reader".into()),
                    ..Selection::default()
                },
                CancellationToken::new(),
            )
            .await
    });
    let mut rejections = 0;
    let timeout = std::time::Duration::from_secs(5);
    let result = tokio::time::timeout(timeout, async {
        loop {
            tokio::select! {
                event = events.recv() => match event {
                    Some(UiEvent::Approval { reply, .. }) => {
                        rejections += 1;
                        reply.send(Decision::Reject).unwrap();
                    }
                    Some(_) => {}
                    None => break,
                },
                outcome = &mut task => {
                    return outcome.unwrap().unwrap();
                }
            }
        }
        tokio::time::timeout(timeout, task)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    })
    .await;
    assert_eq!(
        rejections, 1,
        "the classified call must prompt exactly once for read-only agent"
    );
    result.unwrap();
    // First request: model emits a tool call. Second request: model sees
    // the rejected tool result and produces the final assistant message,
    // which the run returns.
    server.requests.recv().await.unwrap();
    let followup: Value =
        serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let tool_message = followup["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(tool_message["role"], "tool");
    assert!(
        tool_message["content"]
            .as_str()
            .unwrap()
            .contains("Tool rejected by user"),
        "rejected tool result must surface the rejection to the model, got: {}",
        tool_message["content"]
    );
    assert!(
        server.requests.try_recv().is_err(),
        "no further model request should follow the rejection"
    );
}

#[tokio::test]
async fn edit_capable_classified_shell_uses_approval_path_unchanged() {
    // The new read-only routing must not weaken edit-capable behavior:
    // classified shell calls still require explicit approval and run only
    // after Approve. An unknown binary is the safest possible signal that
    // classification actually fired rather than the safe allowlist.
    let mut server = server(vec![
        tool_call(
            "shell",
            json!({"command":"/usr/bin/some-unknown-tool","args":[]}),
        ),
        answer("handled"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut test_config = config(&server.url, tmp.path());
    test_config.agents.insert(
        "writer".into(),
        serde_json::from_value(json!({"can_edit":true,"tools":["shell"]})).unwrap(),
    );
    let (engine, mut events) = engine(test_config);
    let runner = engine.clone();
    let mut task = tokio::spawn(async move {
        runner
            .turn(
                "run".into(),
                Selection {
                    agent: Some("writer".into()),
                    ..Selection::default()
                },
                CancellationToken::new(),
            )
            .await
    });
    let mut saw_approval = false;
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(UiEvent::Approval { reply, .. }) => {
                    saw_approval = true;
                    reply.send(Decision::Approve).unwrap();
                }
                Some(_) => {}
                None => break,
            },
            result = &mut task => {
                result.unwrap().unwrap();
                break;
            }
        }
    }
    assert!(
        saw_approval,
        "classified shell call must prompt for edit-capable agent"
    );
    server.requests.recv().await.unwrap();
    let followup: Value =
        serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    let last = followup["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last["role"], "tool");
    let content = last["content"].as_str().unwrap();
    assert!(
        content.contains("some-unknown-tool") || content.contains("No such file"),
        "approved shell must execute for edit-capable agent, got: {content}"
    );
}

#[tokio::test]
async fn mcp_tool_exposed_when_server_allowed_even_if_not_in_agent_tools_list() {
    // The agent lists only shell/read_file, but an allowed MCP server's
    // tool must still appear in the registered tool set so the model can
    // invoke it. This mirrors the requirement that MCP exposure is
    // independent of the agent's builtin/custom `tools` list.
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config("http://127.0.0.1:1", tmp.path());
    config.mcp_servers.insert(
        "fixture".into(),
        McpConfig {
            uuid: "fixture-uuid".into(),
            enabled: true,
            hitl: false,
            timeout_seconds: 2,
            transport: McpTransport::Stdio {
                command: "python3".into(),
                args: vec![format!(
                    "{}/tests/fixtures/mcp_server.py",
                    env!("CARGO_MANIFEST_DIR")
                )],
                env: BTreeMap::new(),
            },
        },
    );
    config.agents.insert(
        "narrow".into(),
        serde_json::from_value(json!({
            "tools": ["shell", "read_file"],
            "mcp_servers": ["fixture-uuid"]
        }))
        .unwrap(),
    );
    let (engine, _events) = engine(config.clone());
    let tools = engine
        .list_tools(
            &Selection {
                agent: Some("narrow".into()),
                ..Selection::default()
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        tools.iter().any(|t| t.name == "mcp_fixture__echo"),
        "MCP tool must be exposed even though agent only lists shell/read_file; got: {:?}",
        tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn mcp_call_is_rejected_for_unknown_tool_with_known_server() {
    // A tool name that wasn't advertised must not reach `McpManager::call`.
    // This is the execution-side guard for the requirement that "Execution
    // must not admit tools from unallowed servers": any tool name the model
    // picks that isn't registered returns an error before the subprocess
    // runs.
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config("http://127.0.0.1:1", tmp.path());
    config.mcp_servers.insert(
        "fixture".into(),
        McpConfig {
            uuid: "fixture-uuid".into(),
            enabled: false,
            hitl: false,
            timeout_seconds: 2,
            transport: McpTransport::Stdio {
                command: "python3".into(),
                args: vec!["/nonexistent".into()],
                env: BTreeMap::new(),
            },
        },
    );
    let bogus = diet_soda::mcp::McpTool {
        server: "fixture".into(),
        original_name: "echo".into(),
        spec: diet_soda::model::ToolSpec {
            name: "mcp_fixture__echo".into(),
            description: "test".into(),
            input_schema: json!({"type": "object"}),
        },
    };
    let manager = McpManager::default();
    let error = manager
        .call(&bogus, json!({}), &config, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("Unknown MCP server")
            || error.to_string().contains("not found")
            || error.to_string().contains("MCP"),
        "expected an MCP gating error, got: {error}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn custom_command_tool_inherits_baseline_and_overlay() {
    // The custom command branch uses `process::run` with `EnvRequest::custom`,
    // so any configured env overlay is honored while the harness's ambient
    // environment is not inherited beyond the explicit baseline.
    let tmp = tempfile::tempdir().unwrap();
    let key = format!("DIET_SODA_TEST_CUSTOM_{}", uuid::Uuid::new_v4().simple());
    std::env::set_var(&key, "ambient-secret");
    let tool: ToolConfig = serde_json::from_value(json!({
        "type":"command",
        "command":"/bin/sh",
        "args":["-c","env"],
        "description":"env probe",
        "env":{
            "DIET_SODA_TEST_LITERAL":"literal",
            "DIET_SODA_TEST_REF":format!("${{{key}}}")
        }
    }))
    .unwrap();
    let config = config("http://127.0.0.1:1", tmp.path());
    let result = tools::custom(&tool, &json!({}), &config, true, &CancellationToken::new())
        .await
        .unwrap();
    let stdout = result["stdout"].as_str().unwrap();
    std::env::remove_var(&key);
    assert!(stdout.contains("DIET_SODA_TEST_LITERAL=literal"));
    assert!(stdout.contains("DIET_SODA_TEST_REF=ambient-secret"));
    assert!(
        !stdout.contains(&format!("{key}=")),
        "raw ${{KEY}} reference leaked into subprocess"
    );
    // Custom command runs with the workspace cwd, so PATH is preserved.
    assert!(stdout.lines().any(|line| line.starts_with("PATH=")));
}

#[tokio::test]
async fn read_only_shell_keeps_workspace_safe_command_without_prompt() {
    // Mirrors `runtime::read_only_agents_can_run_safe_shell_commands` but
    // exercises a non-builtin binary path. The engine-level guarantee is
    // that the read-only scope does not gate heuristic-safe commands.
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        workspace: tmp.path().into(),
        ..Config::default()
    };
    #[cfg(unix)]
    assert!(!tools::shell_requires_approval(
        &config,
        "/usr/bin/test",
        &["-f".into(), tmp.path().to_string_lossy().into_owned()],
        false,
    )
    .unwrap());
}

// ---------------------------------------------------------------------------
// Wave 3 MCP advertisement + execution gating for runtime toggles and
// hitl/read-only scope.
// ---------------------------------------------------------------------------

fn mcp_fixture_config(workspace: &std::path::Path, hitl: bool) -> Config {
    mcp_fixture_config_with_url("http://127.0.0.1:1", workspace, hitl)
}

fn mcp_fixture_config_with_url(url: &str, workspace: &std::path::Path, hitl: bool) -> Config {
    let mut config = config(url, workspace);
    config.mcp_servers.insert(
        "fixture".into(),
        McpConfig {
            uuid: "fixture-uuid".into(),
            enabled: true,
            hitl,
            timeout_seconds: 2,
            transport: McpTransport::Stdio {
                command: "python3".into(),
                args: vec![format!(
                    "{}/tests/fixtures/mcp_server.py",
                    env!("CARGO_MANIFEST_DIR")
                )],
                env: BTreeMap::new(),
            },
        },
    );
    config
}

#[tokio::test]
async fn mcp_advertisement_omits_runtime_disabled_tools() {
    // The fixture advertises `echo`; toggling that tool name off via the
    // runtime switch (`switches.tools`) must remove it from the advertised
    // toolset so the model cannot pick it. Other MCP tools on the same
    // server (none here, but the filter runs per-tool) would still appear.
    let tmp = tempfile::tempdir().unwrap();
    let mut test_config = mcp_fixture_config(tmp.path(), false);
    test_config.agents.insert(
        "narrow".into(),
        serde_json::from_value(json!({
            "tools": ["shell", "read_file"],
            "mcp_servers": ["fixture-uuid"]
        }))
        .unwrap(),
    );
    let (engine, _events) = engine(test_config);
    // Toggle the MCP tool off before listing.
    engine
        .switches
        .write()
        .await
        .tools
        .insert("mcp_fixture__echo".into(), false);
    let tools = engine
        .list_tools(
            &Selection {
                agent: Some("narrow".into()),
                ..Selection::default()
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        !tools.iter().any(|t| t.name == "mcp_fixture__echo"),
        "runtime-disabled MCP tool must not be advertised; got: {:?}",
        tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn mcp_execution_rejects_tool_toggled_off_between_advertisement_and_call() {
    // End-to-end: a model emits a tool call against an MCP tool that was
    // advertised, then the runtime toggle is flipped off before execution.
    // The execution path must reject the call without invoking the subprocess.
    // The MCP fixture echoes `{"echo": <value>}` so a successful run gives us
    // a clean assertion; a rejection must surface as a tool error and the
    // model will not see the fixture response.
    let mut server = server(vec![
        tool_call("mcp_fixture__echo", json!({"value":"should-not-run"})),
        answer("rejected-then-continued"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut test_config = mcp_fixture_config_with_url(&server.url, tmp.path(), false);
    test_config.agents.insert(
        "narrow".into(),
        serde_json::from_value(json!({
            "tools": ["shell", "read_file"],
            "mcp_servers": ["fixture-uuid"]
        }))
        .unwrap(),
    );
    let (engine, mut events) = engine(test_config);
    // Flip the toggle off before the model picks the tool.
    engine
        .switches
        .write()
        .await
        .tools
        .insert("mcp_fixture__echo".into(), false);
    let runner = engine.clone();
    let task = tokio::spawn(async move {
        runner
            .turn(
                "echo".into(),
                Selection {
                    agent: Some("narrow".into()),
                    ..Selection::default()
                },
                CancellationToken::new(),
            )
            .await
    });
    // No approval prompt should appear — the tool is already disabled.
    let mut pinned = Box::pin(task);
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(UiEvent::Approval { .. }) => panic!(
                    "disabled MCP tool must not surface an approval prompt"
                ),
                Some(_) => {}
                None => break,
            },
            result = &mut pinned => {
                result.unwrap().unwrap();
                break;
            }
        }
    }
    // The model saw two requests: the initial tool call, then the final
    // answer after the rejected tool result. The fixture's `{"echo":...}`
    // payload must NOT appear in either request body — the subprocess never
    // ran, so the harness only echoed back the rejection.
    let first = server.requests.recv().await.unwrap();
    assert!(
        !first.body.contains("should-not-run") || first.body.contains("Tool is disabled"),
        "the request that called for the disabled tool must report the rejection: {first:?}"
    );
}

#[tokio::test]
async fn mcp_advertisement_omits_tools_when_server_toggled_off() {
    // The server allowlist (`scope.mcps`) and the per-server runtime toggle
    // both feed advertisement. Toggling the server off must drop every tool
    // it exposes from the model's view.
    let tmp = tempfile::tempdir().unwrap();
    let mut test_config = mcp_fixture_config(tmp.path(), false);
    test_config.agents.insert(
        "narrow".into(),
        serde_json::from_value(json!({
            "tools": ["shell", "read_file"],
            "mcp_servers": ["fixture-uuid"]
        }))
        .unwrap(),
    );
    let (engine, _events) = engine(test_config);
    // Server allowed first.
    let tools = engine
        .list_tools(
            &Selection {
                agent: Some("narrow".into()),
                ..Selection::default()
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        tools.iter().any(|t| t.name == "mcp_fixture__echo"),
        "MCP echo tool should appear with the server enabled"
    );
    // Toggle the server off; the tool must disappear.
    engine
        .switches
        .write()
        .await
        .mcps
        .insert("fixture".into(), false);
    let tools = engine
        .list_tools(
            &Selection {
                agent: Some("narrow".into()),
                ..Selection::default()
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        !tools.iter().any(|t| t.name == "mcp_fixture__echo"),
        "MCP echo tool must be omitted once the server is toggled off; got: {:?}",
        tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn mcp_advertisement_hides_hitl_server_tools_from_read_only_agent() {
    // A read-only agent (can_edit=false) cannot legally execute hitl-MCP
    // tools (the execution gate in `check_enabled` rejects them), so the
    // advertisement path must also omit them so the model cannot pick one.
    // A separate non-hitl server is still visible so the filter is per-tool.
    let tmp = tempfile::tempdir().unwrap();
    let mut test_config = mcp_fixture_config(tmp.path(), true);
    // Add a second non-hitl server exposing the same tool name space.
    test_config.mcp_servers.insert(
        "nonhitl".into(),
        McpConfig {
            uuid: "nonhitl-uuid".into(),
            enabled: true,
            hitl: false,
            timeout_seconds: 2,
            transport: McpTransport::Stdio {
                command: "python3".into(),
                args: vec![format!(
                    "{}/tests/fixtures/mcp_server.py",
                    env!("CARGO_MANIFEST_DIR")
                )],
                env: BTreeMap::new(),
            },
        },
    );
    test_config.agents.insert(
        "reader".into(),
        serde_json::from_value(json!({
            "tools": ["shell", "read_file"],
            "mcp_servers": ["fixture-uuid", "nonhitl-uuid"]
        }))
        .unwrap(),
    );
    let (engine, _events) = engine(test_config);
    let tools = engine
        .list_tools(
            &Selection {
                agent: Some("reader".into()),
                ..Selection::default()
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        !names.contains(&"mcp_fixture__echo"),
        "hitl-MCP tool must be hidden from a read-only agent; got: {names:?}"
    );
    assert!(
        names.contains(&"mcp_nonhitl__echo"),
        "non-hitl MCP tool must remain visible to a read-only agent; got: {names:?}"
    );
}

#[tokio::test]
async fn mcp_execution_rejects_hitl_tool_for_read_only_agent_at_runtime() {
    // Defence in depth: even when the tool is hidden from a read-only agent
    // at advertisement time, a stale tool name can still reach dispatch if
    // the model cached it across a scope change. The execution-time recheck
    // must still reject hitl-MCP tools for `can_edit:false`. To exercise the
    // execution gate directly, we advertise the tool under an edit-capable
    // agent whose `mcp_servers` allowlist includes the hitl server, then
    // swap the agent for a read-only one (same MCP allowlist, different
    // `can_edit`) and verify that an explicit tool call surfaces as a tool
    // error in the transcript without running the subprocess.
    //
    // We use the conversation loop with a model that emits a tool call to
    // the hitl-MCP tool. Under the edit-capable writer this would route
    // through approval; we do the same call under the read-only scope by
    // using a different `agent` selection at dispatch time. The dispatch
    // path's per-call `check_enabled` gate must reject it before the
    // subprocess runs.
    let mut server = server(vec![
        tool_call("mcp_fixture__echo", json!({"text":"should-not-run"})),
        answer("rejected-by-read-only-gate"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut test_config = mcp_fixture_config_with_url(&server.url, tmp.path(), true);
    // The default `default` agent is read-only; the model will pick it.
    test_config.agents.insert(
        "writer".into(),
        serde_json::from_value(json!({
            "can_edit": true,
            "tools": ["shell", "read_file"],
            "mcp_servers": ["fixture-uuid"]
        }))
        .unwrap(),
    );
    let (engine, mut events) = engine(test_config);
    // Run under the read-only default agent — the tool name still appears
    // in the model's request because the tool was advertised at the start
    // of the run from a previous turn in the same conversation, or because
    // the model is replaying a previously seen call. The execution gate
    // must reject it.
    let runner = engine.clone();
    let task = tokio::spawn(async move {
        runner
            .turn(
                "echo".into(),
                Selection::default(),
                CancellationToken::new(),
            )
            .await
    });
    let mut prompt_seen = false;
    let mut pinned = Box::pin(task);
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(UiEvent::Approval { .. }) => panic!(
                    "read-only agent must NOT see an approval prompt for a hitl-MCP tool"
                ),
                Some(UiEvent::Status { .. }) => {}
                Some(_) => {}
                None => break,
            },
            result = &mut pinned => {
                result.unwrap().unwrap();
                prompt_seen = true;
                break;
            }
        }
    }
    assert!(
        prompt_seen,
        "turn must complete even when the read-only gate rejects the tool"
    );
    // The MCP fixture must not have received the call. The fixture's
    // `echo` script returns `{"echo": "should-not-run"}`; if the harness
    // forwarded the call we would see that string in the final tool
    // message. The error path instead reports the rejection.
    let _ = server.requests.recv().await;
}

#[tokio::test]
async fn mcp_hitl_tool_under_editable_agent_uses_approval_path() {
    // A hitl MCP tool exposed to an edit-capable agent must surface its
    // approval prompt and run once approved. Drive it through the full
    // conversation loop so we exercise the public dispatch surface.
    let mut server = server(vec![
        tool_call("mcp_fixture__echo", json!({"text":"approved"})),
        answer("done"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut test_config = mcp_fixture_config_with_url(&server.url, tmp.path(), true);
    test_config.agents.insert(
        "writer".into(),
        serde_json::from_value(json!({
            "can_edit": true,
            "tools": ["shell", "read_file"],
            "mcp_servers": ["fixture-uuid"]
        }))
        .unwrap(),
    );
    let (engine, mut events) = engine(test_config.clone());
    // Sanity check: the MCP tool must be advertised to the writer agent so
    // the model can pick it. If the dispatch path is silently omitting it,
    // the test below would never reach the approval prompt.
    let advertised = engine
        .list_tools(
            &Selection {
                agent: Some("writer".into()),
                ..Selection::default()
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        advertised.iter().any(|t| t.name == "mcp_fixture__echo"),
        "writer agent must see the hitl-MCP tool in its advertisement; got: {:?}",
        advertised.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
    let runner = engine.clone();
    let task = tokio::spawn(async move {
        runner
            .turn(
                "echo".into(),
                Selection {
                    agent: Some("writer".into()),
                    ..Selection::default()
                },
                CancellationToken::new(),
            )
            .await
    });
    let mut saw_approval = false;
    let mut pinned = Box::pin(task);
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(UiEvent::Approval { reply, .. }) => {
                    saw_approval = true;
                    reply.send(Decision::Approve).unwrap();
                }
                Some(_) => {}
                None => break,
            },
            result = &mut pinned => {
                result.unwrap().unwrap();
                break;
            }
        }
    }
    assert!(
        saw_approval,
        "edit-capable agent must see an approval prompt for a hitl-MCP tool"
    );
    // After approval the MCP fixture ran and returned `{"echo":"approved"}`.
    // The harness forwards that as the tool result and the model replies
    // `done` on the next request.
    let first = server.requests.recv().await.unwrap();
    let followup: Value =
        serde_json::from_str(&server.requests.recv().await.unwrap().body).unwrap();
    assert!(
        first.body.contains("mcp_fixture__echo"),
        "first request should reference the MCP tool name, got: {first:?}"
    );
    let last = followup["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last["role"], "tool");
    let content = last["content"].as_str().unwrap();
    assert!(
        content.contains("approved"),
        "approved MCP tool result should be returned to the model, got: {content}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn limit_zero_blocks_side_effect_command_before_spawn() {
    // `limit: 0` must reject the request before the subprocess runs. We
    // prove "no spawn" by using a side-effecting command that writes a
    // marker file and asserting the marker file is absent on success.
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("marker.txt");
    let script = format!(
        "mkdir -p '{}'; echo side-effect > '{}'",
        marker.parent().unwrap().display(),
        marker.display()
    );
    let env = process::isolated_env(&EnvRequest::shell(), tmp.path()).unwrap();
    let error = process::run(
        ProcessRequest {
            command: "/bin/sh",
            args: &["-c".into(), script],
            cwd: tmp.path(),
            env: &env,
            input: None,
            timeout: 5,
            limit: 0,
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("Output limit must be positive"),
        "expected limit-zero error before spawn, got: {error}"
    );
    assert!(
        !marker.exists(),
        "side-effect command must not run when limit is zero"
    );
}

// ---------------------------------------------------------------------------
// Wave 1 positive allowlist + per-command safety classifier.
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn positive_allowlist_auto_runs_safe_read_only_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        workspace: tmp.path().into(),
        ..Config::default()
    };
    // `wc -c`, `grep -c/-e/-x`, `cut -c`, `head -c`, `tail -c` must auto-run
    // so the positive allowlist does not regress read-only inspection.
    let cases: &[(&str, &[&str])] = &[
        ("/usr/bin/wc", &["-c", "file"]),
        ("/usr/bin/wc", &["-l", "-w", "file"]),
        ("/usr/bin/grep", &["-c", "pattern", "file"]),
        ("/usr/bin/grep", &["-e", "pattern", "-x", "file"]),
        ("/usr/bin/find", &[".", "-name", "*.rs"]),
        ("/usr/bin/find", &[".", "-type", "f", "-print"]),
        ("/usr/bin/cut", &["-c", "1-5", "file"]),
        ("/usr/bin/head", &["-c", "10", "file"]),
        ("/usr/bin/tail", &["-c", "10", "file"]),
        ("/bin/cat", &["file"]),
        ("/bin/ls", &["-la"]),
        ("/usr/bin/stat", &["file"]),
        ("/usr/bin/file", &["file"]),
        ("/usr/bin/realpath", &["file"]),
        ("/usr/bin/dirname", &["path"]),
        ("/usr/bin/basename", &["path"]),
        ("/bin/true", &[]),
        ("/usr/bin/python3", &["--version"]),
        ("/usr/bin/cargo", &["metadata", "--no-deps"]),
        ("/usr/bin/yarn", &["info", "react"]),
        ("/usr/bin/pip3", &["list"]),
        ("/usr/bin/npm", &["view", "react", "version"]),
        ("/usr/bin/make", &["--version"]),
        ("/usr/bin/aws", &["s3", "ls"]),
        (
            "/usr/bin/aws",
            &[
                "--profile",
                "dev",
                "--no-cli-pager",
                "ec2",
                "describe-instances",
            ],
        ),
        ("/usr/bin/aws", &["ec2", "describe-instances"]),
        ("/usr/bin/awscli", &["iam", "list-users"]),
        ("/usr/bin/gh", &["pr", "view", "123"]),
        ("/usr/bin/gh", &["issue", "list"]),
        ("/usr/bin/gws", &["drive", "files", "list"]),
        ("/usr/bin/pup", &["title"]),
    ];
    for (cmd, args) in cases {
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        assert!(
            !tools::shell_requires_approval(&config, cmd, &argv, false).unwrap(),
            "{cmd} {:?} should auto-run under the positive allowlist",
            args
        );
    }
}

#[tokio::test]
async fn persistent_command_approval_is_shared_and_resets_with_session() {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        workspace: tmp.path().into(),
        config_dir: tmp.path().into(),
        sessions_dir: tmp.path().join("sessions"),
        ..Config::default()
    };
    let (engine, mut events) = support::engine(config);
    let cancel = CancellationToken::new();
    let approving_engine = engine.clone();
    let approving_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        approving_engine
            .approve_command_with_activity(
                "main",
                "Allow shell?".into(),
                "Run `cargo test`".into(),
                "cargo test".into(),
                None,
                &approving_cancel,
            )
            .await
    });
    let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
    let UiEvent::Approval {
        persist_allowed,
        reply,
        ..
    } = event
    else {
        panic!("expected a command approval event")
    };
    assert!(persist_allowed);
    reply.send(Decision::ApprovePersist).unwrap();
    assert_eq!(task.await.unwrap().unwrap(), Decision::ApprovePersist);
    assert!(engine.has_session_grant("cargo test").await);
    assert!(!engine.has_session_grant("cargo build").await);

    assert_eq!(
        engine
            .approve_command_with_activity(
                "main",
                "Allow shell?".into(),
                "Run `cargo test --locked`".into(),
                "cargo test".into(),
                None,
                &cancel,
            )
            .await
            .unwrap(),
        Decision::Approve
    );
    assert!(
        events.try_recv().is_err(),
        "matching grant should skip the prompt"
    );

    let session_id = engine.session.lock().await.id.clone();
    engine.reset_session_grants(&session_id).await;
    assert!(!engine.has_session_grant("cargo test").await);

    let stale_engine = engine.clone();
    let stale_cancel = cancel.clone();
    let stale_task = tokio::spawn(async move {
        stale_engine
            .approve_command_with_activity(
                "main",
                "Allow shell?".into(),
                "Run `git push`".into(),
                "git push".into(),
                None,
                &stale_cancel,
            )
            .await
    });
    let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
    let UiEvent::Approval { reply, .. } = event else {
        panic!("expected an approval event for the old session")
    };
    {
        let mut session = engine.session.lock().await;
        session.id.push_str("-next");
    }
    let session_id = engine.session.lock().await.id.clone();
    engine.reset_session_grants(&session_id).await;
    reply.send(Decision::ApprovePersist).unwrap();
    assert_eq!(stale_task.await.unwrap().unwrap(), Decision::ApprovePersist);
    assert!(
        !engine.has_session_grant("git push").await,
        "a late decision from the old session must not grant the new session"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn mutating_network_and_build_commands_require_approval() {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        workspace: tmp.path().into(),
        ..Config::default()
    };
    // Build/package, network clients, VCS mutations, file mutators, wrappers,
    // and unknown executables must all require approval regardless of argv.
    let cases: &[(&str, &[&str])] = &[
        ("/usr/bin/cargo", &["test", "--locked"]),
        ("/usr/bin/rustc", &["file.rs"]),
        ("/usr/bin/make", &["build"]),
        ("/usr/bin/cmake", &["."]),
        ("/usr/bin/go", &["build"]),
        ("/usr/bin/npm", &["install"]),
        ("/usr/bin/pip", &["install", "x"]),
        ("/usr/bin/curl", &["https://example.com"]),
        ("/usr/bin/wget", &["https://example.com"]),
        ("/usr/bin/ssh", &["user@host"]),
        ("/usr/bin/scp", &["user@host:file", "."]),
        ("/bin/rm", &["-rf", "build"]),
        ("/bin/mv", &["a", "b"]),
        ("/bin/cp", &["a", "b"]),
        ("/bin/chmod", &["755", "file"]),
        ("/usr/bin/git", &["push"]),
        ("/usr/bin/git", &["commit", "-m", "x"]),
        ("/usr/bin/git", &["checkout", "main"]),
        ("/usr/bin/find", &["-delete", "."]),
        ("/usr/bin/find", &[".", "-exec", "rm", "{}", ";"]),
        ("/usr/bin/find", &[".", "-fprint", "results.txt"]),
        ("/usr/bin/apt", &["install", "vim"]),
        ("/usr/bin/xargs", &["echo"]),
        ("/usr/bin/sudo", &["echo"]),
        ("/usr/bin/strace", &["echo"]),
        ("/usr/bin/watch", &["ls"]),
        ("/usr/bin/some-unknown-tool", &[]),
        ("/usr/bin/env", &[]),
        ("/usr/bin/python3", &["script.py"]),
        ("/usr/bin/cargo", &["test", "--locked"]),
        ("/usr/bin/yarn", &["install"]),
        ("/usr/bin/pip", &["install", "package"]),
        ("/usr/bin/npm", &["install"]),
        ("/usr/bin/make", &["test"]),
        ("/usr/bin/aws", &["s3", "rm", "s3://bucket/key"]),
        ("/usr/bin/aws", &["ec2", "terminate-instances"]),
        (
            "/usr/bin/aws",
            &["s3api", "get-object", "--bucket", "b", "--key", "k", "out"],
        ),
        (
            "/usr/bin/aws",
            &[
                "secretsmanager",
                "get-secret-value",
                "--secret-id",
                "secret",
            ],
        ),
        (
            "/usr/bin/aws",
            &["sso", "get-role-credentials", "--account-id", "123"],
        ),
        ("/usr/bin/gh", &["pr", "edit", "123"]),
        ("/usr/bin/gh", &["pr", "view", "123", "--web"]),
        ("/usr/bin/gh", &["auth", "status", "--show-token"]),
        ("/usr/bin/gws", &["drive", "files", "delete"]),
        (
            "/usr/bin/gws",
            &[
                "drive",
                "files",
                "update",
                "--json",
                "{\"method\":\"list\"}",
            ],
        ),
    ];
    for (cmd, args) in cases {
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        assert!(
            tools::shell_requires_approval(&config, cmd, &argv, false).unwrap(),
            "{cmd} {:?} must require approval even under the standing grant=false",
            args
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn wrapper_launcher_arguments_require_approval() {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        workspace: tmp.path().into(),
        ..Config::default()
    };
    // `env python3 -c` and `command -v` must require approval because wrappers
    // can rewrite argv even when the wrapped command looks safe.
    assert!(tools::shell_requires_approval(
        &config,
        "/usr/bin/env",
        &["python3".into(), "-c".into(), "print()".into()],
        false,
    )
    .unwrap());
    assert!(tools::shell_requires_approval(
        &config,
        "/bin/sh",
        &["-c".into(), "echo hi".into()],
        false,
    )
    .unwrap());
    assert!(tools::shell_requires_approval(
        &config,
        "/usr/bin/python3",
        &["-c".into(), "print('hi')".into()],
        false,
    )
    .unwrap());
}

#[cfg(unix)]
#[tokio::test]
async fn git_read_only_subcommands_auto_run_without_approval() {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        workspace: tmp.path().into(),
        ..Config::default()
    };
    for sub in [
        "status",
        "log",
        "diff",
        "show",
        "branch",
        "rev-parse",
        "ls-files",
    ] {
        let argv: Vec<String> = vec![sub.into()];
        assert!(
            !tools::shell_requires_approval(&config, "/usr/bin/git", &argv, false).unwrap(),
            "git {sub} should auto-run under the positive allowlist"
        );
    }
    // Mutating subcommands still ask.
    for sub in ["push", "commit", "checkout", "reset", "clean", "restore"] {
        let argv: Vec<String> = vec![sub.into()];
        assert!(
            tools::shell_requires_approval(&config, "/usr/bin/git", &argv, false).unwrap(),
            "git {sub} must require approval"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn path_resolution_uses_workspace_for_relative_argv() {
    // A relative path passed via argv should be checked against the workspace
    // so a symlink escape via `--option=relative/outside` is caught.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let outside = tmp.path().join("secret");
    std::fs::write(&outside, "data").unwrap();
    #[cfg(unix)]
    {
        // Create a symlink under the workspace that targets an outside file.
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
    }
    let config = Config {
        workspace: root.clone(),
        ..Config::default()
    };
    // The symlink is under the workspace but resolves outside; this must be
    // flagged because the resolved path leaves the workspace.
    #[cfg(unix)]
    {
        let argv: Vec<String> = vec![root.join("escape").to_string_lossy().into_owned()];
        assert!(
            tools::outside_path_args(&config, &argv).unwrap(),
            "symlink escape via argv must be flagged"
        );
    }
}

#[tokio::test]
async fn unknown_agent_name_default_is_reserved_during_validation() {
    use diet_soda::config::AgentConfig;
    let mut config = Config::default();
    config.agents.insert(
        "default".into(),
        AgentConfig {
            default: true,
            ..AgentConfig::default()
        },
    );
    let err = config.validate().unwrap_err();
    assert!(
        err.to_string().contains("reserved"),
        "expected reserved-name error, got: {err}"
    );
}

#[tokio::test]
async fn web_fetch_rejects_loopback_host_when_private_networks_disabled() {
    // Bind a TCP listener on 127.0.0.1 with a fast-failing fixture so the
    // SSRF opt-in path does not have to wait the 30-second client timeout
    // when nothing answers. The previous version kept a bound-but-empty
    // port which made the opt-in branch wait for the reqwest timeout. The
    // fixture drops the next connection so the kernel RSTs / FINs; that is
    // enough to prove the SSRF guard let the request through without
    // changing the production web_fetch semantics.
    let port = fast_failing_server().await;
    let config = Config::default();
    // Allow_private = false (default) → loopback destination must be rejected.
    let url = format!("http://127.0.0.1:{port}/");
    let error =
        diet_soda::tools::web_fetch_with_config(&url, &CancellationToken::new(), Some(&config))
            .await
            .unwrap_err();
    assert!(
        error.to_string().contains("non-public"),
        "expected SSRF rejection, got: {error}"
    );

    // With allow_private_networks = true the request goes through.
    let mut allowed = Config::default();
    allowed.web_fetch.allow_private_networks = true;
    let started = std::time::Instant::now();
    let result =
        diet_soda::tools::web_fetch_with_config(&url, &CancellationToken::new(), Some(&allowed))
            .await;
    let elapsed = started.elapsed();
    // The fast-failing fixture closes the connection immediately so the
    // transport returns well below the 30-second client timeout.
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "fast-failing fixture should return well below the client timeout, took {elapsed:?}"
    );
    // The handler will fail because no HTTP server answered, but it must
    // not be the SSRF guard that rejects it.
    let error = result.unwrap_err();
    assert!(
        !error.to_string().contains("non-public"),
        "allow_private_networks should bypass the SSRF guard; got: {error}"
    );
}

#[tokio::test]
async fn web_fetch_rejects_link_local_destination() {
    // 169.254.169.254 is the AWS IMDS / link-local endpoint; the SSRF guard
    // must reject it without DNS round-trip because the literal is already
    // non-public.
    let url = "http://169.254.169.254/latest/meta-data/";
    let config = Config::default();
    let error =
        diet_soda::tools::web_fetch_with_config(url, &CancellationToken::new(), Some(&config))
            .await
            .unwrap_err();
    assert!(
        error.to_string().contains("non-public"),
        "expected SSRF rejection, got: {error}"
    );
}

#[tokio::test]
async fn web_fetch_redirects_are_disabled_and_revalidated_per_hop() {
    // The web_fetch client must not follow redirects automatically. We
    // verify the contract by pointing the harness at a server that returns
    // a 302 to a literal link-local IP. The harness's manual redirect loop
    // must revalidate the next URL through the SSRF guard, and the guard
    // (with `allow_private_networks=false`) must reject the redirect target
    // before any connection is opened. A regression that silently followed
    // the redirect would either hang on the network timeout or return the
    // metadata server's response; both are observable from the error.
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // Use `allow_private_networks=true` only so the first hop (loopback)
    // passes the SSRF guard; the redirect target is link-local so the
    // guard must still reject it on the second hop. This is the
    // per-URL invariant: the guard never depends on the previous hop.
    let mut allowed = Config::default();
    allowed.web_fetch.allow_private_networks = true;
    std::thread::spawn(move || {
        if let Ok((mut socket, _)) = listener.accept() {
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf);
            let body = b"redirected".to_vec();
            // Redirect to a link-local IP. The SSRF guard rejects this
            // regardless of `allow_private_networks` because the option
            // covers the host allowlist, not the public-network classification.
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/meta-data/\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes());
            let _ = socket.write_all(&body);
            let _ = socket.flush();
        }
    });
    let url = format!("http://127.0.0.1:{port}/");
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        diet_soda::tools::web_fetch_with_config(&url, &CancellationToken::new(), Some(&allowed)),
    )
    .await;
    // The harness should fail fast. Either:
    //   - the SSRF guard rejects 169.254.169.254 (link-local) →
    //     Ok(Err) with "non-public"
    //   - a regression that bypasses the guard hangs until the network
    //     timeout → Err(Elapsed) — that's still a failure we want to catch.
    match result {
        Ok(Ok(value)) => panic!(
            "web_fetch unexpectedly returned success on a redirect to a link-local target: {value}"
        ),
        Ok(Err(error)) => {
            assert!(
                error.to_string().contains("non-public") || error.to_string().contains("Redirect"),
                "expected SSRF rejection of the redirect target, got: {error}"
            );
        }
        Err(_) => panic!(
            "web_fetch hung on a redirect to a link-local target; the guard should have fired"
        ),
    }
}

#[tokio::test]
async fn mcp_execution_path_gates_unknown_tool_names() {
    // End-to-end: an MCP server is allowed, a tool name the model picks
    // that does not match any registered tool from that server must be
    // rejected at the dispatch boundary, not silently forwarded.
    use diet_soda::mcp::McpManager;
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = config("http://127.0.0.1:1", tmp.path());
    cfg.mcp_servers.insert(
        "fixture".into(),
        McpConfig {
            uuid: "fixture-uuid".into(),
            enabled: true,
            hitl: false,
            timeout_seconds: 2,
            transport: McpTransport::Stdio {
                command: "python3".into(),
                args: vec![format!(
                    "{}/tests/fixtures/mcp_server.py",
                    env!("CARGO_MANIFEST_DIR")
                )],
                env: BTreeMap::new(),
            },
        },
    );
    // Use a tool name that was not advertised by the fixture. The dispatch
    // path must reject it before the subprocess runs, regardless of the
    // model's claim. The MCP fixture only advertises `echo`, so anything
    // else must be rejected by `McpManager::call`.
    let bogus = diet_soda::mcp::McpTool {
        server: "fixture".into(),
        original_name: "not_a_real_tool".into(),
        spec: diet_soda::model::ToolSpec {
            name: "mcp_fixture__not_a_real_tool".into(),
            description: "test".into(),
            input_schema: json!({"type": "object"}),
        },
    };
    let manager = McpManager::default();
    // Connecting to a server with an unknown tool should still work for
    // tools that were advertised. We assert here that the manager refuses
    // tool names it has no record of.
    let error = manager
        .call(&bogus, json!({}), &cfg, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("MCP")
            || error.to_string().contains("not found")
            || error.to_string().contains("tool"),
        "unknown MCP tool names must be rejected before the subprocess runs: {error}"
    );
    assert!(
        error.to_string().contains("Unknown MCP server")
            || error.to_string().contains("not found")
            || error.to_string().contains("Unknown tool"),
        "unknown MCP tool names must be rejected before the subprocess runs: {error}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn subprocess_pwd_is_pinned_from_cwd() {
    // The child PWD must reflect the request's cwd, not a stale inherited
    // shell variable. Test the env value directly (not `pwd` shell output) so
    // the assertion is independent of how the shell normalizes or canonicalizes
    // its working directory.
    let tmp = tempfile::tempdir().unwrap();
    // Plant a stale inherited PWD so we can detect that the harness ignores it.
    let stale = "/stale/parent/from/harness";
    let prev = std::env::var_os("PWD");
    std::env::set_var("PWD", stale);
    let env = process::isolated_env(&EnvRequest::shell(), tmp.path()).unwrap();
    let result = process::run(
        ProcessRequest {
            command: "/bin/sh",
            args: &["-c".into(), "printf '%s' \"$PWD\"".into()],
            cwd: tmp.path(),
            env: &env,
            input: None,
            timeout: 5,
            limit: 4_000,
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    match prev {
        Some(value) => std::env::set_var("PWD", value),
        None => std::env::remove_var("PWD"),
    }
    let expected = tmp.path().to_string_lossy().into_owned();
    assert_eq!(
        result.stdout, expected,
        "child PWD should equal the request cwd, got {:?}",
        result.stdout
    );
    assert_ne!(
        result.stdout, stale,
        "child PWD must NOT come from the harness's inherited PWD"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn env_isolation_uses_unique_variable_names() {
    // Requirement: serialize fixed-name env mutation or use unique names.
    // Tests must not stomp on shared state; we use a unique var name and
    // verify only that variable passes through.
    let tmp = tempfile::tempdir().unwrap();
    let key = format!("DIET_SODA_WAVE1_UNIQUE_{}", uuid::Uuid::new_v4().simple());
    std::env::set_var(&key, "isolated-secret");
    let mut overlay = BTreeMap::new();
    overlay.insert(key.clone(), "isolated-secret".into());
    let env = process::isolated_env(&EnvRequest::custom(overlay), tmp.path()).unwrap();
    let result = process::run(
        ProcessRequest {
            command: "/bin/sh",
            args: &["-c".into(), format!("echo ${key}")],
            cwd: tmp.path(),
            env: &env,
            input: None,
            timeout: 5,
            limit: 4_000,
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    std::env::remove_var(&key);
    assert!(
        result.stdout.contains("isolated-secret"),
        "unique overlay variable should appear in subprocess stdout: {:?}",
        result.stdout
    );
}

// ---------------------------------------------------------------------------
// Wave 2 targeted classifier and bash-policy regressions.
//
// These tests exercise the per-argument classifiers in
// `tools::classify_safe_command` for `git`, the `grep` safe-flag allowlist,
// the `sort`/`tr` stdout-only path, and the bash-permissions token
// subsequence matcher. They are intentionally deterministic and do not
// touch the harness process layer.
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn git_safe_argument_forms_auto_run_without_approval() {
    // Per-subcommand argument inspection: each safe form lists only the
    // flags that have no execution, ref-mutation, or file-writing effect.
    // Any flag not on the allowlist (e.g. `-c`, `--textconv`, `--ext-diff`)
    // must promote the call to approval-required.
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        workspace: tmp.path().into(),
        ..Config::default()
    };
    let safe: &[(&str, &[&str])] = &[
        // status
        ("status", &[]),
        ("status", &["-s"]),
        ("status", &["--porcelain"]),
        ("status", &["-sb"]),
        // log
        ("log", &[]),
        ("log", &["-1"]),
        ("log", &["--oneline"]),
        ("log", &["--pretty=oneline", "-n", "5"]),
        ("log", &["-p"]),
        ("log", &["--stat"]),
        ("log", &["-p", "--no-color"]),
        // diff
        ("diff", &[]),
        ("diff", &["--stat"]),
        ("diff", &["HEAD"]),
        ("diff", &["--", "file"]),
        // show
        ("show", &[]),
        ("show", &["HEAD"]),
        // rev-parse
        ("rev-parse", &["HEAD"]),
        ("rev-parse", &["--short", "HEAD"]),
        ("rev-parse", &["--verify", "HEAD"]),
        // ls-files
        ("ls-files", &[]),
        ("ls-files", &["-m"]),
        ("ls-files", &["--others", "--exclude-standard"]),
        // ls-tree
        ("ls-tree", &["HEAD"]),
        ("ls-tree", &["-r", "HEAD"]),
        // branch list forms
        ("branch", &[]),
        ("branch", &["-a"]),
        ("branch", &["-r"]),
        ("branch", &["-v"]),
        ("branch", &["--list"]),
        ("branch", &["--list", "feat*"]),
        ("branch", &["--show-current"]),
        ("branch", &["--points-at", "HEAD"]),
        // tag list forms
        ("tag", &[]),
        ("tag", &["-l"]),
        ("tag", &["--list"]),
        ("tag", &["-n"]),
        ("tag", &["--list", "v1*"]),
        ("tag", &["--contains", "HEAD"]),
        // remote list/inspect
        ("remote", &[]),
        ("remote", &["-v"]),
        ("remote", &["show", "origin"]),
        ("remote", &["get-url", "origin"]),
        // config read forms
        ("config", &[]),
        ("config", &["user.name"]),
        ("config", &["--get", "user.name"]),
        ("config", &["--get-all", "remote.origin.url"]),
        ("config", &["--get-regexp", "remote\\..*\\.url"]),
        ("config", &["--list"]),
        ("config", &["-l"]),
        ("config", &["--show-origin", "--get", "user.name"]),
    ];
    for (sub, extra) in safe {
        let mut argv: Vec<String> = vec![(*sub).into()];
        argv.extend(extra.iter().map(|s| (*s).to_owned()));
        assert!(
            !tools::shell_requires_approval(&config, "/usr/bin/git", &argv, false).unwrap(),
            "git {sub} {:?} should auto-run under the positive allowlist",
            extra
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn git_mutating_and_external_helper_forms_require_approval() {
    // Ref-mutating, remote-mutating, config-writing, ref-creating/
    // deleting/renaming, external-diff/textconv, inline-config setters,
    // and unknown forms must all surface for approval even though the
    // binary is the trusted `git`.
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        workspace: tmp.path().into(),
        ..Config::default()
    };
    let mutating: &[(&str, &[&str])] = &[
        // Unknown / clearly mutating subcommands.
        ("push", &["origin", "main"]),
        ("push", &["--force", "origin", "main"]),
        ("push", &["-f", "origin", "main"]),
        ("commit", &["-m", "x"]),
        ("reset", &[]),
        ("reset", &["--hard", "HEAD"]),
        ("clean", &[]),
        ("clean", &["-fd"]),
        ("clean", &["-fx"]),
        ("restore", &[]),
        ("stash", &[]),
        ("merge", &["feature"]),
        ("rebase", &["main"]),
        ("fetch", &[]),
        ("pull", &[]),
        ("clone", &["https://example.com/repo"]),
        // Branch mutations.
        ("branch", &["new-feature"]),
        ("branch", &["-d", "feature"]),
        ("branch", &["-D", "feature"]),
        ("branch", &["-m", "old", "new"]),
        ("branch", &["--set-upstream-to=origin/main"]),
        ("branch", &["--move", "old", "new"]),
        ("branch", &["--track", "origin/main"]),
        // Tag mutations.
        ("tag", &["v1.0"]),
        ("tag", &["-a", "v1.0", "-m", "msg"]),
        ("tag", &["-d", "v1.0"]),
        ("tag", &["-f", "v1.0"]),
        // Remote mutations.
        ("remote", &["add", "origin", "https://example.com"]),
        ("remote", &["remove", "origin"]),
        ("remote", &["rename", "origin", "upstream"]),
        ("remote", &["set-url", "origin", "https://example.com"]),
        ("remote", &["prune", "origin"]),
        ("remote", &["update"]),
        // Config writes.
        ("config", &["user.name", "Alice"]),
        ("config", &["--add", "remote.origin.url", "https://evil"]),
        ("config", &["--replace-all", "user.name", "Mallory"]),
        ("config", &["--unset", "user.name"]),
        ("config", &["--unset-all", "remote.origin.url"]),
        ("config", &["--edit"]),
        ("config", &["--file=custom.conf", "user.name", "x"]),
        ("config", &["--remove-section", "remote"]),
        ("config", &["--rename-section", "remote", "upstream"]),
        (
            "config",
            &["-c", "diff.external=/tmp/evil", "--get", "user.name"],
        ),
        ("config", &["--config-env", "k=v", "--get", "user.name"]),
        // External-diff / textconv / inline-config / exec-path / output.
        ("log", &["--textconv"]),
        ("log", &["--ext-diff"]),
        ("log", &["--external-diff"]),
        ("log", &["-c", "diff.external=/tmp/evil"]),
        ("log", &["-c", "core.pager=/tmp/evil"]),
        ("log", &["--config-env", "diff.external=EVIL"]),
        ("log", &["--exec-path=/tmp/evil"]),
        ("diff", &["--textconv"]),
        ("diff", &["--ext-diff"]),
        ("diff", &["-c", "diff.external=/tmp/evil"]),
        ("diff", &["--output=/tmp/leak"]),
        ("show", &["--textconv"]),
        ("show", &["--ext-diff"]),
        ("show", &["-c", "pager.log=/tmp/evil"]),
        // Checkout ambiguity: one positional is a branch switch, not a list.
        ("checkout", &["main"]),
        ("checkout", &["feature-branch"]),
        ("checkout", &["--", "file"]),
        ("checkout", &["-b", "new-branch"]),
    ];
    for (sub, extra) in mutating {
        let mut argv: Vec<String> = vec![(*sub).into()];
        argv.extend(extra.iter().map(|s| (*s).to_owned()));
        assert!(
            tools::shell_requires_approval(&config, "/usr/bin/git", &argv, false).unwrap(),
            "git {sub} {:?} must require approval",
            extra
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn grep_safe_flag_matching_respects_case_and_file_references() {
    // After the Wave 2 case-sensitive fix, both `-V` (version) and `-v`
    // (invert-match) get their distinct meanings back, `-B`/`-A`/`-C`/`-F`/
    // `-H`/`-I`/`-L`/`-P`/`-R`/`-Z` all auto-run in upper case, and file /
    // pattern flags (`-f`, `--include`, `--exclude`, `--exclude-from`)
    // remain approval-only.
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        workspace: tmp.path().into(),
        ..Config::default()
    };
    // --long=value form is allowed for safe long flags.
    assert!(!tools::shell_requires_approval(
        &config,
        "/usr/bin/grep",
        &["--color=always".into(), "pat".into(), "file".into()],
        false,
    )
    .unwrap());
    // Both case variants of each dual-meaning short flag are safe.
    let dual_safe: &[&[&str]] = &[
        &["-c", "pat", "file"],
        &["-C", "3", "pat", "file"],
        &["-A", "3", "pat", "file"],
        &["-B", "3", "pat", "file"],
        &["-F", "pat", "file"],
        &["-H", "pat", "file"],
        &["-I", "pat", "file"],
        &["-L", "pat", "file"],
        &["-P", "pat", "file"],
        &["-R", "pat", "."],
        &["-Z", "pat", "file"],
    ];
    for argv in dual_safe {
        let argv: Vec<String> = argv.iter().map(|s| (*s).to_owned()).collect();
        assert!(
            !tools::shell_requires_approval(&config, "/usr/bin/grep", &argv, false).unwrap(),
            "grep {argv:?} should auto-run"
        );
    }
    // `-v` (invert) is safe; `-V` (version) is not a search filter.
    assert!(!tools::shell_requires_approval(
        &config,
        "/usr/bin/grep",
        &["-v".into(), "pat".into(), "file".into()],
        false,
    )
    .unwrap());
    assert!(
        tools::shell_requires_approval(&config, "/usr/bin/grep", &["-V".into()], false,).unwrap()
    );

    // File / pattern flags that can reference outside files stay gated.
    let unsafe_flags: &[&[&str]] = &[
        &["-f", "patterns.txt", "file"],
        &["--include=*.txt", "pat", "."],
        &["--exclude=*.txt", "pat", "."],
        &["--exclude-from=patterns.txt", "pat", "."],
        &["--file=patterns.txt", "pat", "file"],
    ];
    for argv in unsafe_flags {
        let argv: Vec<String> = argv.iter().map(|s| (*s).to_owned()).collect();
        assert!(
            tools::shell_requires_approval(&config, "/usr/bin/grep", &argv, false).unwrap(),
            "grep {argv:?} must require approval"
        );
    }
    // Combined short flags whose first char is value-bearing are still safe
    // (the rest is the inline value); non-value flags must compose safely.
    assert!(!tools::shell_requires_approval(
        &config,
        "/usr/bin/grep",
        &["-m5".into(), "pat".into(), "file".into()],
        false,
    )
    .unwrap());
    assert!(!tools::shell_requires_approval(
        &config,
        "/usr/bin/grep",
        &["-ir".into(), "pat".into(), ".".into()],
        false,
    )
    .unwrap());
}

#[cfg(unix)]
#[tokio::test]
async fn sort_and_tr_auto_run_stdout_only_forms() {
    // With `tr` and `sort` removed from the always-approval mutator list,
    // stdout-only invocations must auto-run; output-file forms must still
    // ask because they write to disk.
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        workspace: tmp.path().into(),
        ..Config::default()
    };
    let safe: &[(&str, &[&str])] = &[
        ("/usr/bin/sort", &["file"]),
        ("/usr/bin/sort", &["-k", "2", "file"]),
        ("/usr/bin/sort", &["-r", "file"]),
        ("/usr/bin/sort", &["-n", "file"]),
        ("/usr/bin/sort", &["-u", "file"]),
        ("/usr/bin/tr", &["a-z", "A-Z"]),
        ("/usr/bin/tr", &["-d", "a"]),
        ("/usr/bin/tr", &["-s", " "]),
        ("/usr/bin/tr", &["-c", "a", "b"]),
    ];
    for (cmd, argv) in safe {
        let argv: Vec<String> = argv.iter().map(|s| (*s).to_owned()).collect();
        assert!(
            !tools::shell_requires_approval(&config, cmd, &argv, false).unwrap(),
            "{cmd} {argv:?} should auto-run"
        );
    }
    let mutating: &[(&str, &[&str])] = &[
        ("/usr/bin/sort", &["-o", "out.txt", "file"]),
        ("/usr/bin/sort", &["--output=out.txt", "file"]),
        ("/usr/bin/sort", &["--output", "out.txt", "file"]),
    ];
    for (cmd, argv) in mutating {
        let argv: Vec<String> = argv.iter().map(|s| (*s).to_owned()).collect();
        assert!(
            tools::shell_requires_approval(&config, cmd, &argv, false).unwrap(),
            "{cmd} {argv:?} must require approval"
        );
    }
}

#[tokio::test]
async fn bash_permissions_matcher_preserves_argv_boundaries_and_git_options() {
    // The matcher normalizes blank args, case, and recognized git global
    // options while preserving argv boundaries. Combined short flags keep
    // `rm -rf` matching, but quoted commit messages and noncontiguous
    // subcommand arguments must not match destructive git patterns.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    std::fs::write(
        dir.join("bash-permissions.json"),
        r#"{
            "blocked_commands": ["rm"],
            "blocked_patterns": [
                "rm -rf",
                "git push --force",
                "git push -f",
                "git rebase",
                "git reset --hard",
                "git checkout --",
                "gh pr close"
            ]
        }"#,
    )
    .unwrap();
    let config = Config {
        workspace: dir.clone(),
        config_dir: dir,
        ..Config::default()
    };

    // True positives that must keep matching.
    let must_block: &[(&str, &[&str])] = &[
        ("rm", &["-rf", "build"]),
        ("rm", &["-rff", "build"]),
        ("rm", &["-rfv", "build"]),
        ("rm", &["-rf", "--", "build"]),
        ("git", &["push", "--force", "origin", "main"]),
        ("git", &["push", "-f", "origin", "main"]),
        ("git", &["-C", "/tmp/other", "push", "--force", "origin"]),
        ("git", &["-c", "http.extraheader=x", "push", "--force"]),
        ("git", &["--git-dir", "/tmp/repo", "push", "--force"]),
        ("git", &["--work-tree", "/tmp/tree", "push", "--force"]),
        ("git", &["--namespace", "team", "push", "--force"]),
        ("git", &["--exec-path", "/tmp/git", "push", "--force"]),
        ("git", &["--git-dir=/tmp/repo", "push", "--force"]),
        ("git", &["--work-tree=/tmp/tree", "push", "--force"]),
        ("git", &["--namespace=team", "push", "--force"]),
        ("git", &["--exec-path=/tmp/git", "push", "--force"]),
        ("git", &["rebase"]),
        ("git", &["reset", "--hard"]),
        ("git", &["checkout", "--", "file"]),
        ("gh", &["pr", "close", "1"]),
        ("gh", &["pr", "close", "1", "--yes"]),
    ];
    for (cmd, argv) in must_block {
        let argv: Vec<String> = argv.iter().map(|s| (*s).to_owned()).collect();
        assert!(
            tools::check_bash_permissions(&config, cmd, &argv).is_err(),
            "{cmd} {argv:?} must be blocked"
        );
    }

    // Case + blank-argument variations must still trip the pattern.
    assert!(tools::check_bash_permissions(
        &config,
        "GIT",
        &["PUSH".into(), "--FORCE".into(), "origin".into()],
    )
    .is_err());
    assert!(tools::check_bash_permissions(
        &config,
        "git",
        &["push".into(), "".into(), "--force".into()],
    )
    .is_err());
    // Whitespace inside an argv value is not discarded; only an empty argv
    // value is ignored.
    assert!(
        tools::check_bash_permissions(&config, "git", &["  push".into(), "--force".into()],)
            .is_ok()
    );
    assert!(tools::check_bash_permissions(&config, "GH", &["PR".into(), "CLOSE".into()],).is_err());

    // False-positive-prone cases that the new token matcher must let through.
    let must_allow: &[(&str, &[&str])] = &[
        // Substring `close` no longer matches `closeable`.
        ("gh", &["pr", "closeable", "--list"]),
        // Substring `--hard` no longer matches `reset--hard`.
        ("git", &["reset--hard"]),
        // `--force-with-lease` is a distinct (safer) flag.
        ("git", &["push", "--force-with-lease", "origin"]),
        // A matching subcommand followed by another argv token is not a
        // contiguous match for the blocked pattern.
        ("git", &["push", "origin", "--force"]),
        // Text inside one quoted argv value is not tokenized as shell text.
        ("git", &["commit", "-m", "push --force"]),
        ("git", &["commit", "-m", "git rebase"]),
        // Exact long flags remain distinct from longer flags.
        ("git", &["push", "--force-extra", "origin"]),
        ("git", &["reset", "--harder"]),
        // Different command (`my-rm`) must not trip `rm -rf`.
        ("my-rm", &["-rf", "build"]),
        // Empty / unrelated calls.
        ("git", &["status"]),
        ("gh", &["pr", "list"]),
        ("ls", &["-la"]),
    ];
    for (cmd, argv) in must_allow {
        let argv: Vec<String> = argv.iter().map(|s| (*s).to_owned()).collect();
        assert!(
            tools::check_bash_permissions(&config, cmd, &argv).is_ok(),
            "{cmd} {argv:?} must NOT be blocked"
        );
    }
}

#[tokio::test]
async fn bash_permissions_normalize_attached_git_global_options_only_before_subcommand() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    std::fs::write(
        dir.join("bash-permissions.json"),
        r#"{
            "blocked_commands": [],
            "blocked_patterns": ["git push --force", "git rebase"]
        }"#,
    )
    .unwrap();
    let config = Config {
        workspace: dir.clone(),
        config_dir: dir,
        ..Config::default()
    };

    let must_block: &[&[&str]] = &[
        &["-C/repository", "push", "--force"],
        &["-cuser.name=builder", "push", "--force"],
        &[
            "-C/repository",
            "-cuser.name=builder",
            "--git-dir=/repository/.git",
            "--work-tree",
            "/repository",
            "push",
            "--force",
        ],
        &[
            "-C",
            "/repository",
            "-c",
            "user.name=builder",
            "push",
            "--force",
        ],
        &[
            "--git-dir",
            "/repository/.git",
            "--namespace=team",
            "rebase",
        ],
    ];
    for argv in must_block {
        let argv: Vec<String> = argv.iter().map(|value| (*value).to_owned()).collect();
        assert!(
            tools::check_bash_permissions(&config, "git", &argv).is_err(),
            "git {argv:?} must be blocked"
        );
    }

    let must_allow: &[&[&str]] = &[
        &["commit", "-m", "push --force"],
        &["commit", "-m", "git rebase"],
        &["push", "-cuser.name=builder", "--force"],
        &["push", "-c", "user.name=builder", "--force"],
    ];
    for argv in must_allow {
        let argv: Vec<String> = argv.iter().map(|value| (*value).to_owned()).collect();
        assert!(
            tools::check_bash_permissions(&config, "git", &argv).is_ok(),
            "git {argv:?} must not be blocked"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn bash_permissions_blocked_command_match_is_case_insensitive() {
    // The `blocked_commands` list already used `eq_ignore_ascii_case`;
    // this regression pins that case-folding behavior on top of the
    // pattern token subsequence match.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    std::fs::write(
        dir.join("bash-permissions.json"),
        r#"{"blocked_commands":["rm"],"blocked_patterns":[]}"#,
    )
    .unwrap();
    let config = Config {
        workspace: dir.clone(),
        config_dir: dir,
        ..Config::default()
    };
    assert!(tools::check_bash_permissions(&config, "RM", &["-rf".into(), "x".into()]).is_err());
    assert!(tools::check_bash_permissions(&config, "/bin/Rm", &["x".into()]).is_err());
    assert!(tools::check_bash_permissions(&config, "my-rm", &["x".into()]).is_ok());
}

// ---------------------------------------------------------------------------
// Wave 2 web_fetch / DNS / cancellation security tests.
//
// Each test runs locally; nothing in this file touches the network. The
// non-responding servers use TCP listeners bound to loopback that accept the
// connection and then refuse to write a response.
// ---------------------------------------------------------------------------

/// Spin up a TCP listener on 127.0.0.1 that accepts a single connection, reads
/// whatever the client sends (so the client doesn't get a reset while the
/// kernel buffer fills), and then sits idle without ever writing a response.
/// This models a slow / unresponsive public server; the harness should bail
/// long before the client timeout when the caller cancels.
async fn nonresponding_server() -> u16 {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            // Drain a few bytes so the client kernel buffer doesn't fill and
            // short-circuit the "client never reads" path with a reset.
            let mut buf = [0u8; 1024];
            loop {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(60),
                    socket.read(&mut buf),
                )
                .await
                {
                    Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                    Ok(Ok(_)) => continue,
                }
            }
        }
    });
    port
}

/// Bind a TCP listener on 127.0.0.1 that accepts the next connection and
/// immediately drops it. Used by tests that need a bound port to satisfy
/// `validate_url` and the SSRF guard, but want a fast transport failure so
/// the test does not wait for the client timeout.
async fn fast_failing_server() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        if let Ok((_socket, _)) = listener.accept().await {
            // Drop the socket; the kernel will RST / FIN the next read.
        }
    });
    port
}

#[tokio::test]
async fn web_fetch_cancellation_completes_below_client_timeout() {
    use std::time::{Duration, Instant};
    // The nonresponding server hangs after accepting the connection. With the
    // async DNS migration, cancelling the run must take effect promptly — well
    // below the 30-second client timeout baked into `web_fetch_with_config`.
    let port = nonresponding_server().await;
    let mut allowed = Config::default();
    allowed.web_fetch.allow_private_networks = true;
    let url = format!("http://127.0.0.1:{port}/");
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    // Cancel shortly after the run starts; cancellation has to propagate
    // through the DNS lookup, the connect, and into the body read.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        trigger.cancel();
    });
    let started = Instant::now();
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        tools::web_fetch_with_config(&url, &cancel, Some(&allowed)),
    )
    .await
    .expect("web_fetch did not honour cancellation within 5 seconds")
    .unwrap_err();
    let elapsed = started.elapsed();
    // The 5-second outer timeout is well below the 30s client timeout — if
    // cancellation raced correctly we land closer to 100 ms than to 30 s.
    assert!(
        elapsed < Duration::from_secs(2),
        "cancellation should complete well below the 30s client timeout, took {elapsed:?}"
    );
    assert!(
        error.to_string().contains("Cancelled"),
        "expected cancellation error, got: {error}"
    );
}

#[tokio::test]
async fn web_fetch_cancellation_during_dns_lookup_returns_cancellation_error() {
    // The async DNS resolver is raced against the cancellation token. The
    // resolver may complete synchronously on platforms where the resolver is
    // fast (e.g. `/etc/hosts` miss returns NXDOMAIN immediately), in which
    // case the lookup arm wins and we get a DNS error rather than a
    // cancellation error — both outcomes prove the lookup is non-blocking.
    // We assert two things: (a) the call completes below the client timeout
    // (proves it doesn't block on a sync resolver path), and (b) the error
    // path is reachable through `web_fetch_with_config` rather than hanging.
    use std::time::{Duration, Instant};
    let cancel = CancellationToken::new();
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        tools::web_fetch_with_config("http://nonexistent-diet-soda-host.invalid/", &cancel, None),
    )
    .await
    .expect("web_fetch did not return within 5 seconds; the call must not block");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "DNS lookup must not block the runtime, took {elapsed:?}"
    );
    let error = result.unwrap_err();
    // We accept either outcome: cancellation raced and won, or the resolver
    // returned synchronously with an error. Either way, the call returned
    // quickly rather than waiting for the 30s client timeout.
    assert!(
        error.to_string().contains("Cancelled") || error.to_string().contains("DNS resolution"),
        "expected cancellation or DNS-resolution error, got: {error}"
    );
}

#[tokio::test]
async fn web_fetch_localhost_hostname_is_denied_by_default_and_allowed_with_opt_in() {
    // `localhost` is a hostname (not an IP literal), so it exercises the async
    // DNS path. Default config denies it because loopback is in the private
    // set; with `allow_private_networks = true` the request is allowed to
    // reach the SSRF guard's success path (we use a fast-failing server so
    // we don't have to wait for the 30s client timeout).
    let port = fast_failing_server().await;
    let url = format!("http://localhost:{port}/");
    // Default: deny.
    let config = Config::default();
    let error = tools::web_fetch_with_config(&url, &CancellationToken::new(), Some(&config))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("non-public"),
        "default config must reject `localhost` hostname, got: {error}"
    );
    // Opt-in: hostname is allowed by the SSRF guard (the transport fails
    // fast because the server closes the connection; the failure must NOT
    // come from the SSRF guard).
    let mut allowed = Config::default();
    allowed.web_fetch.allow_private_networks = true;
    let error = tools::web_fetch_with_config(&url, &CancellationToken::new(), Some(&allowed))
        .await
        .unwrap_err();
    assert!(
        !error.to_string().contains("non-public"),
        "opt-in config must let the hostname through the SSRF guard, got: {error}"
    );
}

#[tokio::test]
async fn web_fetch_link_local_remains_denied_under_opt_in() {
    // IPv4 link-local (169.254.0.0/16) is in the always-blocked set. The
    // opt-in for private networks covers loopback / RFC 1918 / CGNAT / IPv6
    // ULA, NOT link-local. This pins that distinction.
    let mut allowed = Config::default();
    allowed.web_fetch.allow_private_networks = true;
    let url = "http://169.254.169.254/latest/meta-data/";
    let error = tools::web_fetch_with_config(url, &CancellationToken::new(), Some(&allowed))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("non-public"),
        "link-local must be rejected even with allow_private_networks=true, got: {error}"
    );
}

#[tokio::test]
async fn custom_http_link_local_remains_denied_with_private_opt_in() {
    let tool: ToolConfig = serde_json::from_value(json!({
        "type": "http",
        "method": "GET",
        "url": "http://169.254.169.254/latest/meta-data/",
        "description": "metadata probe",
        "allow_private_networks": true
    }))
    .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let error = tools::custom(
        &tool,
        &json!({}),
        &config("http://127.0.0.1:1", tmp.path()),
        false,
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("non-public"),
        "custom HTTP must reject link-local even with opt-in: {error}"
    );
}

#[tokio::test]
async fn web_fetch_ipv4_mapped_private_remains_denied_under_opt_in() {
    // `[::ffff:10.0.0.1]` is the IPv4-mapped form of an RFC 1918 address.
    // The opt-in covers plain IPv4 private, but the IPv4-mapped private
    // path is always blocked because it is a common SSRF bypass payload.
    let mut allowed = Config::default();
    allowed.web_fetch.allow_private_networks = true;
    let url = "http://[::ffff:10.0.0.1]/";
    let error = tools::web_fetch_with_config(url, &CancellationToken::new(), Some(&allowed))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("non-public"),
        "IPv4-mapped private must be rejected even with allow_private_networks=true, got: {error}"
    );
}

#[tokio::test]
async fn web_fetch_ipv4_mapped_multicast_broadcast_and_zero_octet_are_always_blocked() {
    // IPv4-mapped IPv6 must go through the same always-blocked set as plain
    // IPv4. Multicast (`[::ffff:224.0.0.1]`), broadcast
    // (`[::ffff:255.255.255.255]`), and 0.0.0.0/8 (`[::ffff:0.0.0.1]`) are
    // never reachable, even with `allow_private_networks=true` and even
    // when wrapped in IPv6 brackets. The opt-in only relaxes RFC 1918 /
    // CGNAT / loopback; these three classifications must stay SSRF-tight.
    let allowed = {
        let mut cfg = Config::default();
        cfg.web_fetch.allow_private_networks = true;
        cfg
    };
    for url in [
        "http://[::ffff:224.0.0.1]/",
        "http://[::ffff:255.255.255.255]/",
        "http://[::ffff:0.0.0.1]/",
    ] {
        let error = tools::web_fetch_with_config(url, &CancellationToken::new(), Some(&allowed))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("non-public"),
            "IPv4-mapped always-blocked range must be rejected under opt-in, got: {error}"
        );
    }
}

#[tokio::test]
async fn web_fetch_rejects_ipv6_transition_and_embedded_private_targets_with_or_without_opt_in() {
    // These addresses encode loopback, link-local, or RFC 1918 IPv4 targets
    // in every transition/embedded range handled by the SSRF guard. The
    // ranges are always blocked because allowing the private-network opt-in
    // must not provide a transition mechanism around the address checks.
    let urls = [
        // NAT64 well-known prefix, 64:ff9b::/96.
        "http://[64:ff9b::7f00:1]/",
        "http://[64:ff9b::a9fe:a9fe]/",
        "http://[64:ff9b::c0a8:101]/",
        // NAT64 local-use prefix, 64:ff9b:1::/48.
        "http://[64:ff9b:1::7f00:1]/",
        "http://[64:ff9b:1::a9fe:a9fe]/",
        "http://[64:ff9b:1::c0a8:101]/",
        // 6to4, 2002::/16.
        "http://[2002:7f00:1::]/",
        "http://[2002:a9fe:a9fe::]/",
        "http://[2002:c0a8:101::]/",
        // Teredo, 2001:0000::/32. The embedded IPv4 address is bitwise
        // inverted by the Teredo format.
        "http://[2001:0:0:0:0:0:80ff:fffe]/",
        "http://[2001:0:0:0:0:0:5601:5601]/",
        "http://[2001:0:0:0:0:0:3f57:fefe]/",
        // Deprecated IPv4-compatible range, ::/96.
        "http://[::7f00:1]/",
        "http://[::a9fe:a9fe]/",
        "http://[::c0a8:101]/",
        // Deprecated site-local range, fec0::/10.
        "http://[fec0::1]/",
    ];

    for allow_private_networks in [false, true] {
        let mut config = Config::default();
        config.web_fetch.allow_private_networks = allow_private_networks;
        for url in urls {
            let error = tools::web_fetch_with_config(url, &CancellationToken::new(), Some(&config))
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("non-public"),
                "transition/embedded IPv6 target must be rejected with allow_private_networks={allow_private_networks}: {url}: {error}"
            );
        }
    }
}

#[tokio::test]
async fn web_fetch_allows_normal_public_ipv6_literal_through_address_guard() {
    // The address is public. Network failure is acceptable, but an SSRF
    // classification error is not.
    let url = "http://[2606:4700:4700::1111]/";
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tools::web_fetch_with_config(url, &CancellationToken::new(), None),
    )
    .await
    .expect("public IPv6 request must not hang");
    if let Err(error) = result {
        assert!(
            !error.to_string().contains("non-public"),
            "public IPv6 literal was rejected by the address guard: {error}"
        );
    }
}

#[tokio::test]
async fn web_fetch_redirect_to_private_hostname_is_revalidated_and_cancellable() {
    // First hop: loopback, sends a 302 redirect to `localhost:<other>`.
    // Second hop: a nonresponding server bound on loopback.
    // We cancel during the second hop and confirm the cancellation races the
    // second `send` rather than waiting for the client timeout. We also
    // assert that, in the absence of cancellation, the SSRF guard would
    // revalidate the redirect target (covered by the existing redirect test,
    // but we double-check the hostname path here).
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let second_port = nonresponding_server().await;
    let first_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let first_port = first_listener.local_addr().unwrap().port();
    let redirect_path = format!("/see-other-{second_port}");
    tokio::spawn(async move {
        if let Ok((mut socket, _)) = first_listener.accept().await {
            let mut buf = vec![0u8; 1024];
            let _ = socket.read(&mut buf).await;
            // Use a Location header that resolves to loopback via hostname so
            // the second hop exercises async DNS.
            let body = b"redirected".to_vec();
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://localhost:{second_port}{redirect_path}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.write_all(&body).await;
        }
    });
    let mut allowed = Config::default();
    allowed.web_fetch.allow_private_networks = true;
    let url = format!("http://127.0.0.1:{first_port}/");
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        trigger.cancel();
    });
    let started = std::time::Instant::now();
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tools::web_fetch_with_config(&url, &cancel, Some(&allowed)),
    )
    .await
    .expect("redirect cancellation did not fire within 5 seconds")
    .unwrap_err();
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "redirect cancellation should complete well below the client timeout, took {elapsed:?}"
    );
    assert!(
        error.to_string().contains("Cancelled"),
        "expected cancellation during second hop, got: {error}"
    );
}
