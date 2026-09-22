//! Append-only session storage and plain-text export. Each event is one write;
//! fsync happens at conversation checkpoints rather than once per tiny event.
mod export;
use crate::fsutil;
use crate::model::{ActivityEvent, ActivityPhase, Message, Spend, Usage};
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};

/// Byte sequence present in every `serde_json` serialization of a recovery
/// record's `"type":"recovery"` field, used to cheaply pre-filter lines
/// before the candidate JSON parse. See `Session::open`.
const RECOVERY_MARKER: &[u8] = b"\"recovery\"";

/// One row of the all-context display timeline. The TUI replay surface reads
/// this in original JSONL order through `display_events`; `messages` only
/// carries the parent conversation's history.
///
/// This is the payload carried by [`DisplayEvent::Message`] and holds the
/// unredacted live value by design: redaction happens only inside `append`,
/// on the serialized JSONL event, so secrets may be present here in memory.
/// The human-readable export never reads this struct; it re-reads the
/// redacted JSONL file from disk, which is the only redacted representation.
#[derive(Debug, Clone)]
pub struct TranscriptEntry {
    pub context: String,
    pub message: Message,
}

/// Unified display timeline entry. The TUI's session replay surface reads
/// this in original JSONL order: every successfully parsed message
/// (regardless of context, including incomplete assistant transcripts) and
/// every successfully parsed activity record interleaves into a single
/// ordered timeline.
///
/// The enum is an in-memory index only. On disk each entry is an ordinary
/// `message` or `activity` JSONL line; `activity` is an additive kind that
/// older readers tolerate by skipping (the `_ => {}` arm in `Session::open`)
/// rather than understand. `display_events` is rebuilt from those lines on
/// every reopen, and `record_message` / `record_activity` append one entry
/// per call, so the live session and a fresh reopen agree on the ordering.
///
/// `clear` does not reset this timeline; the full-session record is
/// preserved so a later `/export` still shows every cleared turn
/// alongside everything before it. Live main-context model history is
/// the only thing `clear` resets.
#[derive(Debug, Clone)]
pub enum DisplayEvent {
    Message(TranscriptEntry),
    Activity(ActivityEvent),
}

pub struct Session {
    pub id: String,
    pub path: PathBuf,
    pub messages: Vec<Message>,
    pub spend: Spend,
    /// Latest main-context request size (input + output tokens), restored on resume.
    pub context_tokens: u64,
    /// Unified in-memory display timeline. Every successfully parsed
    /// message (any context, including incomplete main-context markers)
    /// and every successfully parsed activity interleaves here in the
    /// exact order encountered in the JSONL log. This is the source the
    /// TUI's unified session replay surface reads from.
    ///
    /// Entries hold unredacted live values; redaction happens only inside
    /// `append`, on the serialized JSONL event.
    ///
    /// Invariants:
    /// - One entry per parsed event; malformed non-main message lines and
    ///   malformed activity lines are skipped without a corresponding
    ///   `DisplayEvent`.
    /// - Reopened sessions rebuild this from disk; live recording adds
    ///   one entry per `record_message` / `record_activity` call.
    /// - `clear` preserves this timeline, and the JSONL file retains the
    ///   cleared turns, so a later `/export` (which re-reads the file)
    ///   still shows them alongside everything before it. Only the live
    ///   main-context model history in `messages` is reset.
    /// - Synthetic repair messages appended by [`Session::open`] appear
    ///   at the end via the existing `record_message` path, so they get
    ///   one matching `DisplayEvent::Message` each.
    pub display_events: Vec<DisplayEvent>,
    /// Ids of activity starts that never received an `End` event when the
    /// session was reopened, in original JSONL order. The TUI can expose
    /// these as cancelled; we do not persist a synthetic `End` because doing
    /// so would silently rewrite historical files and break the append-only
    /// invariant established for every other event kind.
    ///
    /// `recovered_unmatched` is computed once per reopen from the on-disk
    /// activity log and therefore reflects the entire session file, not just
    /// the history currently held in `messages`; `clear` does not touch it,
    /// and activities recorded after the reopen are not tracked here.
    pub recovered_unmatched: Vec<String>,
    file: File,
    redactions: Vec<String>,
}
impl Session {
    pub fn open(dir: &Path, id: Option<&str>) -> Result<Self> {
        let id = id
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if !crate::config::valid_name(&id) {
            bail!("Invalid session ID");
        }
        fsutil::create_dir_all_private(dir)?;
        let path = dir.join(format!("{id}.jsonl"));
        let mut file = fsutil::private_open_options()
            .create(true)
            .append(true)
            .open(&path)?;
        file.try_lock_exclusive()
            .context("Session is already open in another process")?;
        let mut messages = vec![];
        let mut spend = Spend::default();
        let mut context_tokens = 0;
        // Transient load-time accumulator, kept only long enough to compute
        // `recovered_unmatched` after the walk. The authoritative retained
        // ordered copy lives in `display_events` as `DisplayEvent::Activity`;
        // it is not stored on the `Session` itself.
        let mut activities: Vec<ActivityEvent> = vec![];
        let mut display_events: Vec<DisplayEvent> = vec![];
        let text = std::fs::read(&path)?;
        let lines: Vec<&[u8]> = text.split(|b| *b == b'\n').collect();
        // Build the ignored-line set without a full JSON parse of every
        // line. Only lines that byte-contain the literal recovery type
        // marker can possibly be recovery records, so scan for that byte
        // sequence first and JSON-parse just the candidates. A valid
        // `serde_json` serialization of `"type":"recovery"` always emits
        // those bytes verbatim (key order and whitespace do not change the
        // string contents), so this cannot produce false negatives for
        // serde-produced logs; false positives are simply rejected by the
        // parse/validation below.
        let ignored: HashSet<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                line.windows(RECOVERY_MARKER.len())
                    .any(|w| w == RECOVERY_MARKER)
            })
            .filter_map(|(_, line)| serde_json::from_slice::<Value>(line).ok())
            .filter(|v| v["type"] == "recovery")
            .filter_map(|v| v["data"]["ignored_line"].as_u64().map(|n| n as usize))
            .collect();
        let mut recover = None;
        for (index, line) in lines.iter().enumerate() {
            if ignored.contains(&index) || line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let mut event: Value = match serde_json::from_slice(line) {
                Ok(value) => value,
                Err(_) if index + 1 == lines.len() && !text.ends_with(b"\n") => {
                    recover = Some(index);
                    continue;
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("Corrupt session line {}", index + 1))
                }
            };
            // Move the `data` subtree out of the envelope once, so the typed
            // conversions below consume the owned value instead of cloning it.
            // The envelope's `type`/`context` fields are read afterwards and
            // remain untouched.
            let data = event
                .get_mut("data")
                .map(Value::take)
                .unwrap_or(Value::Null);
            match event["type"].as_str() {
                Some("message") => {
                    let context = event["context"].as_str().unwrap_or("main").to_owned();
                    // The main-context slice is the source of truth for
                    // model request history; `display_events` retains every
                    // context for replay. `clear` events are still honored
                    // by the main slice below.
                    //
                    // Main-context parsing is strict: a corrupt main-context
                    // message line should fail the reopen, because hiding it
                    // would silently desync the model request history the
                    // provider expects on the next turn. Non-main contexts
                    // are best-effort: malformed child lines are skipped so
                    // they cannot prevent a previously openable session
                    // from loading, and the valid ones stay available for
                    // future replay.
                    //
                    // Incomplete assistant messages are kept in the
                    // display timeline so the user can see what was streamed
                    // before the failure, but never enter the main-context
                    // model request slice. Replaying a partial assistant
                    // turn would either confuse the model or, worse, let
                    // the provider re-execute whatever the engine had
                    // refused to dispatch.
                    match serde_json::from_value::<Message>(data) {
                        Ok(message) => {
                            // Only main-context messages belong on the model
                            // request history; non-main contexts (subagent,
                            // workflow, tool transcript) live on
                            // `display_events` only. Among main-context
                            // messages, an incomplete one is a display
                            // marker for a stream that never finished; it
                            // must not re-enter the next provider request.
                            let include_in_history =
                                context == "main" && message.incomplete.is_none();
                            if include_in_history {
                                messages.push(message.clone());
                            }
                            // The unified display timeline is the sole
                            // all-context message store: every successfully
                            // parsed message (any context, including
                            // incomplete main-context markers) interleaves
                            // here in original JSONL order.
                            display_events
                                .push(DisplayEvent::Message(TranscriptEntry { context, message }));
                        }
                        Err(error) if context == "main" => {
                            return Err(error).with_context(|| {
                                format!("Corrupt main-context message line {}", index + 1)
                            });
                        }
                        Err(_) => {}
                    }
                }
                Some("usage") => {
                    let usage: Usage = serde_json::from_value(data)?;
                    if event["context"] == "main" {
                        context_tokens = usage.input_tokens.saturating_add(usage.output_tokens);
                    }
                    spend.add(&usage);
                }
                Some("clear") => messages.clear(),
                Some("activity") => {
                    // Forward-compatible: a malformed activity line should
                    // not poison the rest of the session. Skip and keep
                    // going; the existing corrupt-line path will surface
                    // truly broken JSON.
                    if let Ok(activity) = serde_json::from_value::<ActivityEvent>(data) {
                        // Accumulate for the load-time `recovered_unmatched`
                        // pass only; the retained copy is the one pushed into
                        // `display_events` below.
                        activities.push(activity.clone());
                        // The unified display timeline includes every
                        // successfully parsed activity in original JSONL
                        // order; malformed activity lines remain skipped
                        // here, matching the existing tolerant behavior.
                        display_events.push(DisplayEvent::Activity(activity));
                    }
                }
                _ => {}
            }
        }
        if !text.is_empty() && !text.ends_with(b"\n") {
            file.write_all(b"\n")?;
        }
        // Compute unmatched activity starts in original JSONL order so the
        // TUI can present them as cancelled without rewriting the file.
        let recovered_unmatched = unmatched_activity_starts(&activities);
        let mut session = Self {
            id,
            path,
            messages,
            spend,
            context_tokens,
            display_events,
            recovered_unmatched,
            file,
            redactions: vec![],
        };
        if let Some(index) = recover {
            session.append("recovery", "main", json!({"ignored_line":index}))?;
        }
        if text.is_empty() {
            session.append("session", "main", json!({"id":session.id,"version":1}))?;
        }
        // Complete interrupted tool exchanges so resumed requests remain provider-valid.
        // Gather every answered tool_call_id once, then walk assistant tool calls
        // in history order and collect the unanswered ids in that same order.
        let answered: HashSet<&str> = session
            .messages
            .iter()
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        let missing: Vec<String> = session
            .messages
            .iter()
            .flat_map(|m| m.tool_calls.iter())
            .filter(|t| !answered.contains(t.id.as_str()))
            .map(|t| t.id.clone())
            .collect();
        for id in missing {
            session.record_message(
                "main",
                Message::tool(
                    &id,
                    "Tool execution interrupted before a result was recorded.",
                ),
            )?;
        }
        Ok(session)
    }
    pub fn append(&mut self, kind: &str, context: &str, data: Value) -> Result<()> {
        let mut event =
            json!({"type":kind,"at":chrono::Utc::now().to_rfc3339(),"context":context,"data":data});
        redact(&mut event, &self.redactions);
        let mut bytes = serde_json::to_vec(&event)?;
        bytes.push(b'\n');
        self.file.write_all(&bytes)?;
        Ok(())
    }
    /// Durability boundary for completed turns/steps and explicit exports.
    pub fn checkpoint(&self) -> Result<()> {
        self.file.sync_data()?;
        Ok(())
    }
    pub fn add_redactions(&mut self, values: Vec<String>) {
        self.redactions.extend(values);
        self.redactions.sort_by_key(|s| std::cmp::Reverse(s.len()));
        self.redactions.dedup();
    }
    pub fn record_message(&mut self, context: &str, message: Message) -> Result<()> {
        let context = context.to_owned();
        self.append("message", &context, serde_json::to_value(&message)?)?;
        // Incomplete assistant messages carry safe partial text but must
        // never re-enter the next provider request. They live in
        // `display_events` so the user can still see them on replay, but the
        // main-context `messages` slice is the model request history.
        if context == "main" && message.incomplete.is_none() {
            self.messages.push(message.clone());
        }
        // Unified display timeline: one entry per call, the sole all-context
        // message store. Synthetic repair messages appended by `Session::open`
        // reach here through the same path, so they show up at the end of the
        // timeline once.
        self.display_events
            .push(DisplayEvent::Message(TranscriptEntry { context, message }));
        Ok(())
    }

    /// Persist an activity lifecycle record and reflect it in memory.
    ///
    /// The `activity` JSONL kind is additive: readers from before it existed
    /// tolerate the unknown kind by skipping it (the `_ => {}` arm in
    /// `Session::open`) rather than understand it, so old binaries load
    /// these sessions unchanged.
    ///
    /// The persisted line goes through the same redaction pass as every
    /// other event, so secrets in the activity payload are stripped before
    /// they hit disk. The retained in-memory `DisplayEvent::Activity` entry
    /// holds the unredacted live value by design; only the JSONL line is
    /// redacted.
    pub fn record_activity(&mut self, event: ActivityEvent) -> Result<()> {
        let context = event.context.clone();
        self.append("activity", &context, serde_json::to_value(&event)?)?;
        // Unified display timeline: one entry per call. `display_events` is
        // the sole retained activity store; only the JSONL append is
        // authoritative on disk.
        self.display_events.push(DisplayEvent::Activity(event));
        Ok(())
    }
    pub fn usage(&mut self, context: &str, usage: &Usage) -> Result<()> {
        self.append("usage", context, serde_json::to_value(usage)?)?;
        if context == "main" {
            self.context_tokens = usage.input_tokens.saturating_add(usage.output_tokens);
        }
        self.spend.add(usage);
        Ok(())
    }
    pub fn clear(&mut self) -> Result<()> {
        self.append("clear", "main", json!({}))?;
        self.messages.clear();
        Ok(())
    }
}

/// Apply every secret to `text` in the configured order, replacing both the
/// JSON-escaped and the raw occurrence. `secrets` is sorted longest-first by
/// [`Session::add_redactions`], so no shorter secret can split a longer one
/// into unredacted fragments.
fn redact_text(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for secret in secrets {
        let escaped = serde_json::to_string(secret).expect("Strings serialize as JSON");
        out = out.replace(&escaped[1..escaped.len() - 1], "[REDACTED]");
        out = out.replace(secret, "[REDACTED]");
    }
    out
}

fn redact(value: &mut Value, secrets: &[String]) {
    match value {
        Value::String(s) => {
            *s = redact_text(s, secrets);
        }
        Value::Array(a) => {
            for v in a {
                redact(v, secrets);
            }
        }
        Value::Object(o) => {
            // Redact keys with the same rules as string values, then rebuild
            // the map so every entry survives even when distinct secret-bearing
            // keys collapse onto one `[REDACTED]` key. Collisions get a stable
            // numeric suffix derived from input order; the suffix is never a
            // secret, so redaction stays idempotent and JSON stays valid.
            let entries: Vec<(String, Value)> = std::mem::take(o)
                .into_iter()
                .map(|(key, mut v)| {
                    redact(&mut v, secrets);
                    (redact_text(&key, secrets), v)
                })
                .collect();
            let mut used: HashSet<String> = HashSet::new();
            for (key, value) in entries {
                let mut unique = key.clone();
                let mut n = 2;
                while used.contains(&unique) {
                    unique = format!("{key} ({n})");
                    n += 1;
                }
                used.insert(unique.clone());
                o.insert(unique, value);
            }
        }
        _ => {}
    }
}

/// Ids of activity starts that never received a matching `End` event.
/// Ordering follows the original JSONL so the TUI can present them in
/// chronological order alongside the rest of the run.
///
/// The tracker is single-pass and in-order: each `Start` pushes the id onto
/// an open list, each `End` removes the most recently opened occurrence of
/// that id. The list at the end of the walk is the result. This avoids the
/// set-based "any matching `End` anywhere in the log closes every start"
/// failure mode, where a reordered or orphaned `End` permanently masked
/// later starts of the same id.
///
/// Concretely:
/// - A `Start` always opens a new entry, even if the id is already on the
///   open list, so a producer that reuses an id after a closed lifecycle
///   still recovers the unclosed one.
/// - An `End` closes only the topmost matching id, never an earlier one.
/// - An `End` with no preceding matching `Start` is a no-op, so orphan or
///   reordered ends cannot permanently mask later starts.
///
/// Important: producers must still use **globally unique** ids for each
/// activity. The recovery tracker tolerates sequential reuse (id A ends,
/// then id A starts again) but cannot distinguish two concurrent open
/// lifecycles that share an id — both will be reported as unmatched when
/// the session is reopened. The on-disk id contract is unchanged; the
/// tracker is just best-effort against a best-effort id.
fn unmatched_activity_starts(activities: &[ActivityEvent]) -> Vec<String> {
    let mut open: Vec<String> = Vec::new();
    for activity in activities {
        match activity.phase {
            ActivityPhase::Start => open.push(activity.id.clone()),
            ActivityPhase::End => {
                if let Some(index) = open.iter().rposition(|id| id == &activity.id) {
                    open.remove(index);
                }
            }
        }
    }
    open
}

// -------------------------------------------------------------------------
// Performance harness (non-gating). Loads and resumes an approximately
// 20 MiB session through the real `Session::open` path.
// Marked `#[ignore]` so ordinary `cargo test` skips it; run with, e.g.:
//
//   cargo test --release --lib -- --ignored \
//       perf_session_20mib_load_and_resume --nocapture
//
// The harness prints timing to stdout and is informational only; it
// always passes after recording the sample so it never gates CI.
// Run it in release mode: debug-build overhead dominates these
// measurements and the numbers are not comparable across optlevels.
// -------------------------------------------------------------------------
#[cfg(test)]
mod perf_tests {
    use super::{Message, Session};
    use serde_json::json;
    use std::{fs::OpenOptions, io::Write, path::PathBuf, time::Instant};

    /// Build an ~20 MiB JSONL session by streaming `record_message`
    /// calls into a real session directory, force a checkpoint, and
    /// reopen it through `Session::open` to measure the resume cost.
    /// The setup write itself is measured separately so the reported
    /// figure is the load time on a fully-written file, not the cost
    /// of constructing the fixture.
    #[test]
    #[ignore = "performance harness; release-only, run with --ignored --nocapture"]
    fn perf_session_20mib_load_and_resume() {
        let tmp = tempfile::tempdir().unwrap();
        let dir: PathBuf = tmp.path().to_path_buf();
        let target = 20 * 1024 * 1024usize;
        // Body size picked so each turn stays under the cap and the
        // final session lands at ~target bytes of JSONL content.
        let body = "x".repeat(2 * 1024);
        // Write the session once so we can reopen it.
        let write_count = {
            let mut session = Session::open(&dir, Some("perf-20mib")).unwrap();
            let mut count = 0usize;
            while std::fs::metadata(&session.path).unwrap().len() < target as u64 {
                let role = if count % 2 == 0 { "user" } else { "assistant" };
                session
                    .record_message("main", Message::new(role, body.as_str()))
                    .unwrap();
                count += 1;
            }
            session.checkpoint().unwrap();
            let bytes = std::fs::metadata(&session.path).unwrap().len();
            assert!(
                bytes >= target as u64,
                "session file {bytes} < target {target}"
            );
            // Sanity: the in-memory message list is what got serialized.
            assert_eq!(session.messages.len(), count);
            count
        };
        // The writing `Session` is dropped at the end of the block
        // above, releasing its file handle (and any exclusive advisory
        // lock) before we reopen the directory below. On Windows that
        // lock would otherwise block `Session::open`; on POSIX it is an
        // advisory check that `Session::open` performs anyway.
        // Reopen through the public Session::open path; this is the
        // path the TUI uses when resuming an existing session.
        let reopen_start = Instant::now();
        let mut session = Session::open(tmp.path(), Some("perf-20mib")).unwrap();
        let reopen_elapsed = reopen_start.elapsed();
        assert_eq!(session.messages.len(), write_count);
        let bytes = std::fs::metadata(&session.path).unwrap().len();
        eprintln!(
            "[perf_session_20mib_load_and_resume] bytes={bytes} messages={write_count} reopen={:.3} ms",
            reopen_elapsed.as_secs_f64() * 1000.0,
        );
        // Touch the session so the harness has a real resume use site:
        // appending and clearing exercises the post-load append path.
        let clear_start = Instant::now();
        session.clear().unwrap();
        session.checkpoint().unwrap();
        let clear_elapsed = clear_start.elapsed();
        eprintln!(
            "[perf_session_20mib_load_and_resume] post_resume_clear={:.3} ms",
            clear_elapsed.as_secs_f64() * 1000.0,
        );
    }

    /// Round-trip a single hand-written JSONL message line through
    /// `Session::open` to lock in the on-disk line shape that the
    /// harness also relies on. Writes one event with the documented
    /// fields (`type`, `at`, `context`, `data`) and confirms the
    /// reopened session parses it back into a `Message` with the
    /// expected content.
    #[test]
    fn perf_session_helpers_round_trip_message_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("helper.jsonl");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let event = json!({
            "type": "message",
            "at": "2026-01-01T00:00:00Z",
            "context": "main",
            "data": {
                "role": "user",
                "content": "hi",
                "tool_calls": [],
            }
        });
        let mut bytes = serde_json::to_vec(&event).unwrap();
        bytes.push(b'\n');
        file.write_all(&bytes).unwrap();
        let session = Session::open(tmp.path(), Some("helper")).unwrap();
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content, "hi");
    }
}
