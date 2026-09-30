//! Bounded, read-only scan for the session browser. It never locks session
//! files and skips corrupt files so one bad session cannot break the listing.

use anyhow::Result;
use std::{
    fs::{self, File},
    io::Read,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

const SCAN_WINDOW: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub modified: SystemTime,
    pub cwd: Option<String>,
    pub preview: String,
}

/// List session JSONL files, inspecting no more than `SCAN_WINDOW` bytes in
/// each file. A live session remains listable because this scan never locks it.
pub fn list_sessions(dir: &Path) -> Result<Vec<SessionSummary>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };

    let mut summaries = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if !crate::config::valid_name(id) {
            continue;
        }

        let Ok(file) = File::open(&path) else {
            continue;
        };
        let mut bytes = Vec::new();
        if (&file)
            .take(SCAN_WINDOW as u64)
            .read_to_end(&mut bytes)
            .is_err()
        {
            continue;
        }
        let hit_cap = bytes.len() == SCAN_WINDOW;

        let mut cwd = None;
        let mut found_start = false;
        let mut messages_seen = 0usize;
        let mut main_user_preview = None;
        let mut any_user_preview = None;
        let mut saw_non_user = false;

        let trailing_unterminated = hit_cap && !bytes.ends_with(b"\n");
        let trailing_line_index =
            trailing_unterminated.then(|| bytes.iter().filter(|byte| **byte == b'\n').count());
        for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
            if Some(index) == trailing_line_index {
                continue;
            }
            let Ok(event) = serde_json::from_slice::<serde_json::Value>(line) else {
                continue;
            };
            match event.get("type").and_then(serde_json::Value::as_str) {
                Some("session") if !found_start => {
                    found_start = true;
                    cwd = event
                        .get("data")
                        .and_then(|data| data.get("cwd"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                }
                Some("message") => {
                    messages_seen += 1;
                    let data = event.get("data");
                    let is_user = data
                        .and_then(|data| data.get("role"))
                        .and_then(serde_json::Value::as_str)
                        == Some("user");
                    let content = data
                        .and_then(|data| data.get("content"))
                        .and_then(serde_json::Value::as_str);

                    if is_user {
                        if event.get("context").and_then(serde_json::Value::as_str) == Some("main")
                        {
                            if main_user_preview.is_none() {
                                main_user_preview = content.map(str::to_owned);
                            }
                        } else if any_user_preview.is_none() {
                            any_user_preview = content.map(str::to_owned);
                        }
                    } else if main_user_preview.is_none() && any_user_preview.is_none() {
                        saw_non_user = true;
                    }
                }
                _ => {}
            }
        }

        if messages_seen == 0 && !hit_cap {
            continue;
        }

        let raw_preview = main_user_preview
            .or(any_user_preview)
            .or_else(|| saw_non_user.then(|| "(workflow)".to_owned()))
            .unwrap_or_else(|| "(preview unavailable)".to_owned());
        let preview = sanitize_preview(&raw_preview);
        let modified = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(UNIX_EPOCH);

        summaries.push(SessionSummary {
            id: id.to_owned(),
            modified,
            cwd,
            preview,
        });
    }

    summaries.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.id.cmp(&b.id)));
    Ok(summaries)
}

fn sanitize_preview(raw: &str) -> String {
    let mut collapsed = String::new();
    let mut pending_space = false;
    for character in raw.chars() {
        let character = if crate::text::is_unsafe_terminal_char(character) {
            ' '
        } else {
            character
        };
        if character.is_whitespace() {
            pending_space = !collapsed.is_empty();
        } else {
            if pending_space {
                collapsed.push(' ');
                pending_space = false;
            }
            collapsed.push(character);
        }
    }
    let collapsed = collapsed.trim();
    if collapsed.chars().count() > 80 {
        let mut truncated: String = collapsed.chars().take(77).collect();
        truncated.push('…');
        truncated
    } else {
        collapsed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{list_sessions, SessionSummary, SCAN_WINDOW};
    use serde_json::{json, Value};
    use std::{
        fs::{self, File},
        io::Write,
        path::Path,
        time::{Duration, SystemTime},
    };
    use tempfile::tempdir;

    fn write_lines(path: &Path, lines: &[Value]) {
        let mut file = File::create(path).unwrap();
        for line in lines {
            serde_json::to_writer(&mut file, line).unwrap();
            file.write_all(b"\n").unwrap();
        }
    }

    fn start(cwd: Option<&str>) -> Value {
        let mut data = json!({"id": "session-id", "version": 1});
        if let Some(cwd) = cwd {
            data["cwd"] = json!(cwd);
        }
        json!({"type": "session", "at": "2026-01-01T00:00:00Z", "context": "main", "data": data})
    }

    fn message(context: &str, role: &str, content: &str) -> Value {
        json!({
            "type": "message",
            "at": "2026-01-01T00:00:00Z",
            "context": context,
            "data": {"role": role, "content": content}
        })
    }

    fn summary<'a>(summaries: &'a [SessionSummary], id: &str) -> &'a SessionSummary {
        summaries.iter().find(|summary| summary.id == id).unwrap()
    }

    #[test]
    fn lists_sessions_newest_first_and_reads_cwd() {
        let dir = tempdir().unwrap();
        let older = dir.path().join("older.jsonl");
        let newer = dir.path().join("newer.jsonl");
        write_lines(
            &older,
            &[start(Some("/tmp/x")), message("main", "user", "older")],
        );
        write_lines(&newer, &[start(None), message("main", "user", "newer")]);
        let now = SystemTime::now();
        File::open(&older)
            .unwrap()
            .set_modified(now - Duration::from_secs(60))
            .unwrap();
        File::open(&newer).unwrap().set_modified(now).unwrap();

        let summaries = list_sessions(dir.path()).unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].id, "newer");
        assert_eq!(summaries[1].id, "older");
        assert_eq!(summary(&summaries, "older").cwd.as_deref(), Some("/tmp/x"));
        assert_eq!(summary(&summaries, "newer").cwd, None);
    }

    #[test]
    fn ignores_non_jsonl_and_invalid_names() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("diet_soda.log"), b"junk").unwrap();
        fs::write(dir.path().join("notes.txt"), b"junk").unwrap();
        fs::write(dir.path().join("evil name.jsonl"), b"{}\n").unwrap();

        assert!(list_sessions(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn skips_corrupt_file_without_failing() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("broken.jsonl"), b"not json\n").unwrap();
        write_lines(
            &dir.path().join("good.jsonl"),
            &[start(None), message("main", "user", "good")],
        );

        let summaries = list_sessions(dir.path()).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, "good");
    }

    #[test]
    fn omits_empty_sessions() {
        let dir = tempdir().unwrap();
        write_lines(&dir.path().join("empty.jsonl"), &[start(None)]);
        assert!(list_sessions(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn workflow_only_session_previews_placeholder() {
        let dir = tempdir().unwrap();
        write_lines(
            &dir.path().join("workflow.jsonl"),
            &[
                start(None),
                message("subagent:x", "assistant", "work"),
                message("subagent:x", "tool", "result"),
            ],
        );

        let summaries = list_sessions(dir.path()).unwrap();
        assert_eq!(summaries[0].preview, "(workflow)");
    }

    #[test]
    fn user_preview_sanitized_collapsed_truncated() {
        let dir = tempdir().unwrap();
        let content = format!("before\u{202e}  {}\n  after", "x".repeat(200));
        write_lines(
            &dir.path().join("sanitize.jsonl"),
            &[start(None), message("main", "user", &content)],
        );

        let summaries = list_sessions(dir.path()).unwrap();
        let preview = &summaries[0].preview;
        assert!(!preview.chars().any(crate::text::is_unsafe_terminal_char));
        assert!(!preview.contains("  "));
        assert!(preview.chars().count() <= 81);
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn oversized_first_message_still_listed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("oversized.jsonl");
        let mut file = File::create(path).unwrap();
        serde_json::to_writer(&mut file, &start(None)).unwrap();
        file.write_all(b"\n").unwrap();
        serde_json::to_writer(
            &mut file,
            &message("main", "user", &"x".repeat(SCAN_WINDOW)),
        )
        .unwrap();

        let summaries = list_sessions(dir.path()).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].preview, "(preview unavailable)");
    }

    #[test]
    fn missing_dir_is_empty() {
        let dir = tempdir().unwrap();
        assert!(list_sessions(&dir.path().join("nope")).unwrap().is_empty());
    }

    #[test]
    fn any_context_user_message_is_fallback_preview() {
        let dir = tempdir().unwrap();
        write_lines(
            &dir.path().join("fallback.jsonl"),
            &[
                start(None),
                message("subagent:x", "user", "fallback prompt"),
            ],
        );

        let summaries = list_sessions(dir.path()).unwrap();
        assert_eq!(summaries[0].preview, "fallback prompt");
    }
}
