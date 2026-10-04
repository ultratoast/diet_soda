//! Pure request-size estimation and history trimming. No async, no engine
//! state, no I/O: every function here is a deterministic function of inputs.

use crate::model::Message;
use serde_json::{json, Value};
use std::collections::HashMap;

/// Default context window size in tokens.
pub const DEFAULT_CONTEXT_WINDOW: u32 = 131_072;
/// Newest messages never collapsed on the first trim pass.
pub const RECENT_PROTECT: usize = 4;
/// Marker substring written into a head-truncated tool result. Pass 3 treats a
/// message as already handled only when it both ends with the marker's
/// `bytes]` suffix and contains this text followed by `; original ` — a bare
/// occurrence (e.g. tool output quoting this source file) is not a match.
const TRUNCATION_MARKER: &str = "[truncated to fit context budget";
/// Trim down to this fraction of the byte budget (hysteresis).
pub const TRIM_TARGET_FRACTION: f64 = 0.7;

/// Serialized request size in bytes: system prompt + tool schema bytes +
/// JSON serialization of the message list (`Message` implements `Serialize`).
pub fn estimate_bytes(system: &str, messages: &[Message], tools_bytes: usize) -> usize {
    let messages_bytes = serde_json::to_string(messages)
        .map(|s| s.len())
        .unwrap_or(0);
    system.len() + tools_bytes + messages_bytes
}

/// Remaining input budget in bytes after reserving tokens for output and for
/// the system prompt + tool schemas. Returns 0 if `bytes_per_token <= 0.0`
/// (callers validate config; this is a defensive guard, not recovery).
pub fn budget_bytes(
    window: u32,
    output_cap: u32,
    system_and_tools_bytes: usize,
    bytes_per_token: f64,
) -> usize {
    if bytes_per_token <= 0.0 {
        return 0;
    }
    let reserve_tokens = output_cap as f64 + (system_and_tools_bytes as f64 / bytes_per_token);
    let input_tokens = (window as f64 - reserve_tokens).max(0.0);
    (input_tokens * bytes_per_token) as usize
}

/// Outcome of a [`trim`] call. `estimated_*` values are byte estimates of the
/// full serialized request (system + tools + messages).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrimReport {
    pub estimated_before: usize,
    pub estimated_after: usize,
    pub budget_bytes: usize,
    pub collapsed: usize,
    pub cleared_reasoning: usize,
    pub irreducible: bool,
}

/// True when `content` is already a trim placeholder: a JSON object carrying
/// `"trimmed": true`. Guarantees idempotency across repeated `trim` calls.
fn is_placeholder(content: &str) -> bool {
    serde_json::from_str::<Value>(content)
        .map(|v| v.get("trimmed").and_then(Value::as_bool).unwrap_or(false))
        .unwrap_or(false)
}

/// `tool_call_id -> tool name` lookup built from every assistant message.
fn tool_name_map(messages: &[Message]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for message in messages {
        if message.role == "assistant" {
            for call in &message.tool_calls {
                map.insert(call.id.clone(), call.name.clone());
            }
        }
    }
    map
}

/// Collapse a tool-result body into a placeholder and record it.
fn collapse_tool(message: &mut Message, names: &HashMap<String, String>) -> usize {
    let original_bytes = message.content.len();
    let tool = message
        .tool_call_id
        .as_deref()
        .and_then(|id| names.get(id).map(String::as_str))
        .unwrap_or("unknown");
    message.content = json!({
        "trimmed": true,
        "tool": tool,
        "original_bytes": original_bytes,
    })
    .to_string();
    original_bytes
}

/// Clear transport-only reasoning fields from an assistant message.
/// Returns true if anything was actually cleared (keeps counts idempotent).
fn clear_reasoning(message: &mut Message) -> bool {
    let mut cleared = false;
    if message.reasoning.take().is_some() {
        cleared = true;
    }
    if !message.reasoning_details.is_empty() {
        message.reasoning_details.clear();
        cleared = true;
    }
    if !message.native_content.is_empty() {
        message.native_content.clear();
        cleared = true;
    }
    cleared
}

/// Trim history until the estimate fits the budget. A request that already
/// fits `budget_bytes` is returned unchanged — `TRIM_TARGET_FRACTION` is only
/// the stop target once over budget.
///
/// Pass 1 (oldest-first): collapses `role == "tool"` bodies and clears
/// assistant reasoning, never touching index 0 or the newest
/// [`RECENT_PROTECT`] messages, stopping once the running estimate is at or
/// below `budget_bytes * TRIM_TARGET_FRACTION`.
///
/// Pass 2 (last resort): if still over `budget_bytes`, may also collapse
/// protected tool-result messages — but never index 0 and never the very last
/// message.
///
/// Pass 3 (final resort): if still over `budget_bytes` and the newest message
/// is an oversized tool result, head-truncate it to fit (leaving a truncation
/// marker, which makes the pass idempotent). Never index 0. If still over
/// budget afterwards, `irreducible` is set — a huge final user/assistant
/// message stays irreducible.
///
/// Deterministic and idempotent: a second call on unchanged input reports
/// `collapsed == 0` and leaves the estimate unchanged.
pub fn trim(
    system: &str,
    messages: &mut Vec<Message>,
    tools_bytes: usize,
    budget_bytes: usize,
    bytes_per_token: f64,
) -> TrimReport {
    let _ = bytes_per_token; // reserved; estimates are byte-based today
    let estimated_before = estimate_bytes(system, messages, tools_bytes);
    if estimated_before <= budget_bytes {
        return TrimReport {
            estimated_before,
            estimated_after: estimated_before,
            budget_bytes,
            collapsed: 0,
            cleared_reasoning: 0,
            irreducible: false,
        };
    }
    let target = (budget_bytes as f64 * TRIM_TARGET_FRACTION) as usize;
    let names = tool_name_map(messages);
    let mut running = estimated_before;
    let mut collapsed = 0usize;
    let mut cleared_reasoning = 0usize;
    let recent_start = messages.len().saturating_sub(RECENT_PROTECT);
    // Pass 1 protection also covers the last assistant message carrying
    // tool_calls: with N parallel calls it can sit just outside the recent
    // tail, and clearing its reasoning/native_content makes providers reject
    // the request. Pass 2/3 keep the plain recent window.
    let mut protect_start = recent_start;
    if let Some(k) = messages
        .iter()
        .rposition(|m| m.role == "assistant" && !m.tool_calls.is_empty())
    {
        protect_start = protect_start.min(k);
    }

    // Pass 1: oldest-first, skip index 0 and the protected tail.
    for i in 0..messages.len() {
        if running <= target {
            break;
        }
        if i == 0 || i >= protect_start {
            continue;
        }
        let changed = if messages[i].role == "tool" && !is_placeholder(&messages[i].content) {
            collapse_tool(&mut messages[i], &names);
            collapsed += 1;
            true
        } else if messages[i].role == "assistant" && clear_reasoning(&mut messages[i]) {
            cleared_reasoning += 1;
            true
        } else {
            false
        };
        if changed {
            running = estimate_bytes(system, messages, tools_bytes);
        }
    }

    // Pass 2: last resort — protected tool results only; never index 0,
    // never the final message.
    if running > budget_bytes {
        for i in recent_start..messages.len() {
            if running <= budget_bytes {
                break;
            }
            if i == 0 || i + 1 == messages.len() {
                continue;
            }
            if messages[i].role == "tool" && !is_placeholder(&messages[i].content) {
                collapse_tool(&mut messages[i], &names);
                collapsed += 1;
                running = estimate_bytes(system, messages, tools_bytes);
            }
        }
    }

    // Pass 3: a lone oversized newest tool result is head-truncated to fit.
    // Never index 0; a huge final user/assistant message stays irreducible.
    if running > budget_bytes && messages.len() > 1 {
        let last = messages.len() - 1;
        let content = &messages[last].content;
        // Already pass-3-truncated only when anchored to our exact marker
        // ending; a tool result merely containing the marker text (e.g. this
        // source file) must not be mistaken for a prior truncation.
        let already_truncated = content.trim_end().ends_with("bytes]")
            && content.contains("[truncated to fit context budget; original ");
        if messages[last].role == "tool" && !is_placeholder(content) && !already_truncated {
            let over = running.saturating_sub(budget_bytes);
            let original_len = messages[last].content.len();
            let mut cut = original_len.saturating_sub(over + 128);
            while cut > 0 && !messages[last].content.is_char_boundary(cut) {
                cut -= 1;
            }
            let head = messages[last].content[..cut].to_string();
            messages[last].content =
                format!("{head}\n{TRUNCATION_MARKER}; original {original_len} bytes]\n");
            running = estimate_bytes(system, messages, tools_bytes);
        }
    }

    TrimReport {
        estimated_before,
        estimated_after: running,
        budget_bytes,
        collapsed,
        cleared_reasoning,
        irreducible: running > budget_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ToolCall;
    use std::collections::HashSet;

    fn m(role: &str, content: &str) -> Message {
        Message {
            role: role.to_string(),
            content: content.to_string(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning: None,
            reasoning_details: Vec::new(),
            native_content: Vec::new(),
            incomplete: None,
        }
    }

    fn assistant_calling(id: &str) -> Message {
        let mut a = m("assistant", "");
        a.tool_calls = vec![ToolCall {
            id: id.to_string(),
            name: "read".to_string(),
            arguments: "{}".to_string(),
        }];
        a
    }

    fn tool_result(id: &str, content: &str) -> Message {
        let mut t = m("tool", content);
        t.tool_call_id = Some(id.to_string());
        t
    }

    /// Every tool message's `tool_call_id` still pairs with an assistant
    /// `tool_calls` id after trimming.
    fn assert_pairing(messages: &[Message]) {
        let ids: HashSet<&str> = messages
            .iter()
            .filter(|m| m.role == "assistant")
            .flat_map(|m| m.tool_calls.iter().map(|c| c.id.as_str()))
            .collect();
        for message in messages.iter().filter(|m| m.role == "tool") {
            if let Some(id) = message.tool_call_id.as_deref() {
                assert!(
                    ids.contains(id),
                    "tool message {id} lost its assistant pairing"
                );
            }
        }
    }

    #[test]
    fn estimate_grows_with_message_count() {
        let one = vec![m("user", "hello")];
        let many: Vec<Message> = (0..20).map(|i| m("user", &format!("msg {i}"))).collect();
        assert!(estimate_bytes("sys", &one, 0) < estimate_bytes("sys", &many, 0));
        assert!(estimate_bytes("sys", &many, 500) > estimate_bytes("sys", &many, 0));
    }

    #[test]
    fn budget_never_goes_negative() {
        assert_eq!(budget_bytes(10, 50, 100, 4.0), 0);
        assert_eq!(
            budget_bytes(131_072, 8_192, 4_000, 4.0),
            (131_072 - 8_192 - 1_000) * 4
        );
        assert_eq!(budget_bytes(131_072, 8_192, 4_000, 0.0), 0);
        assert_eq!(budget_bytes(131_072, 8_192, 4_000, -1.0), 0);
    }

    #[test]
    fn trim_noop_when_under_budget() {
        let mut messages = vec![
            m("user", "hi"),
            assistant_calling("t1"),
            tool_result("t1", "small"),
        ];
        let before = estimate_bytes("sys", &messages, 0);
        let report = trim("sys", &mut messages, 0, before * 2, 4.0);
        assert_eq!(report.collapsed, 0);
        assert_eq!(report.cleared_reasoning, 0);
        assert_eq!(report.estimated_before, before);
        assert_eq!(report.estimated_after, before);
        assert!(!report.irreducible);
        assert_eq!(messages[2].content, "small");
    }

    #[test]
    fn collapses_oldest_tool_results_first_and_protects_edges() {
        let big = "x".repeat(8_000);
        let mut messages = vec![m("user", "u")];
        for n in 1..=4 {
            messages.push(assistant_calling(&format!("t{n}")));
            messages.push(tool_result(&format!("t{n}"), &big));
        }
        // Layout: 0 user, 1-2 t1, 3-4 t2, 5-6 t3, 7-8 t4. Last 4 (5..9)
        // are protected on pass 1.
        let original_first = messages[0].content.clone();
        let original_t3 = messages[6].content.clone();
        let original_t4 = messages[8].content.clone();
        let estimate = estimate_bytes("sys", &messages, 0);
        // Budget genuinely BELOW the estimate: a request that fits must not
        // be trimmed, so trimming has to be actually required here.
        let budget = estimate - 1_000;
        assert!(estimate > budget);

        let report = trim("sys", &mut messages, 0, budget, 4.0);

        assert_eq!(report.estimated_before, estimate);
        assert!(report.collapsed >= 1);
        assert!(report.estimated_after <= budget);
        assert!(!report.irreducible);
        // Index 0 and protected tail untouched.
        assert_eq!(messages[0].content, original_first);
        assert_eq!(messages[6].content, original_t3);
        assert_eq!(messages[8].content, original_t4);
        assert!(!is_placeholder(&messages[0].content));
        assert!(!is_placeholder(&messages[6].content));
        // Oldest eligible tool result collapsed.
        assert!(is_placeholder(&messages[2].content));
        assert_pairing(&messages);
    }

    #[test]
    fn returns_unchanged_when_request_fits_above_trim_target() {
        let big = "x".repeat(8_000);
        let mut messages = vec![m("user", "u")];
        for n in 1..=4 {
            messages.push(assistant_calling(&format!("t{n}")));
            messages.push(tool_result(&format!("t{n}"), &big));
        }
        let estimate = estimate_bytes("sys", &messages, 0);
        // ~80% of budget: over the 0.7 trim target but still fitting, so
        // nothing may be collapsed, cleared, or truncated.
        let budget = estimate * 5 / 4;
        assert!(estimate <= budget);
        assert!(estimate > budget * 7 / 10);
        let contents: Vec<String> = messages.iter().map(|m| m.content.clone()).collect();

        let report = trim("sys", &mut messages, 0, budget, 4.0);

        assert_eq!(report.estimated_before, estimate);
        assert_eq!(report.estimated_after, estimate);
        assert_eq!(report.collapsed, 0);
        assert_eq!(report.cleared_reasoning, 0);
        assert!(!report.irreducible);
        assert_eq!(
            contents,
            messages
                .iter()
                .map(|m| m.content.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn trim_is_idempotent() {
        let big = "x".repeat(8_000);
        let mut messages = vec![m("user", "u")];
        for n in 1..=4 {
            messages.push(assistant_calling(&format!("t{n}")));
            messages.push(tool_result(&format!("t{n}"), &big));
        }
        // Budget below the estimate so the first call trims for real.
        let budget = estimate_bytes("sys", &messages, 0) - 1_000;
        let first = trim("sys", &mut messages, 0, budget, 4.0);
        let snapshot: Vec<String> = messages.iter().map(|m| m.content.clone()).collect();

        let second = trim("sys", &mut messages, 0, budget, 4.0);
        assert_eq!(second.collapsed, 0);
        assert_eq!(second.cleared_reasoning, 0);
        assert_eq!(second.estimated_before, first.estimated_after);
        assert_eq!(second.estimated_after, first.estimated_after);
        assert_eq!(
            snapshot,
            messages
                .iter()
                .map(|m| m.content.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn trim_head_truncates_lone_oversized_newest_tool_result() {
        let big = "é".repeat(20_000); // 40_000 bytes, all multi-byte
        let mut messages = vec![
            m("user", "keep me"),
            assistant_calling("t1"),
            tool_result("t1", &big),
        ];
        let before = estimate_bytes("sys", &messages, 0);
        let budget = before / 4;

        let report = trim("sys", &mut messages, 0, budget, 4.0);

        assert!(!report.irreducible);
        assert!(report.estimated_after <= budget);
        // Index 0 untouched.
        assert_eq!(messages[0].content, "keep me");
        // Newest tool result was head-truncated with a marker.
        let last = &messages[2].content;
        assert!(last.len() < big.len());
        assert!(last.starts_with("é"));
        assert!(last.contains("[truncated to fit context budget"));
        assert!(last.contains(&format!("original {} bytes", big.len())));
        // Valid UTF-8 after cutting at a char boundary.
        assert!(std::str::from_utf8(last.as_bytes()).is_ok());
        assert_pairing(&messages);

        // Re-running trim is idempotent (same budget)...
        let snapshot = last.clone();
        let second = trim("sys", &mut messages, 0, budget, 4.0);
        assert_eq!(second.estimated_after, report.estimated_after);
        assert_eq!(messages[2].content, snapshot);

        // ...and the marker guard blocks a second truncation even if the
        // budget shrinks further.
        let third = trim("sys", &mut messages, 0, budget / 2, 4.0);
        assert!(third.irreducible);
        assert_eq!(messages[2].content, snapshot);
    }

    #[test]
    fn trim_keeps_oversized_final_user_message_irreducible() {
        let big = "x".repeat(20_000);
        let mut messages = vec![m("user", "keep me"), m("user", &big)];
        let before = estimate_bytes("sys", &messages, 0);
        let budget = before / 4;

        let report = trim("sys", &mut messages, 0, budget, 4.0);

        assert!(report.irreducible);
        assert_eq!(report.estimated_after, before);
        assert_eq!(messages[1].content, big);
        assert_eq!(messages[0].content, "keep me");
    }

    #[test]
    fn placeholder_is_valid_json_with_metadata() {
        let original = "y".repeat(8_000);
        let mut messages = vec![
            m("user", "u"),
            assistant_calling("t1"),
            tool_result("t1", &original),
            assistant_calling("t2"),
            tool_result("t2", "small"),
            m("assistant", "done"),
            m("assistant", "end"),
        ];
        // len 7 -> protect_start 3, so index 2 is eligible on pass 1.
        // Budget below the estimate so trimming is actually required.
        let budget = estimate_bytes("sys", &messages, 0) - 1_000;
        trim("sys", &mut messages, 0, budget, 4.0);

        let parsed: Value =
            serde_json::from_str(&messages[2].content).expect("placeholder must be valid JSON");
        assert_eq!(parsed.get("trimmed"), Some(&Value::Bool(true)));
        assert_eq!(parsed.get("tool"), Some(&Value::String("read".into())));
        assert_eq!(
            parsed.get("original_bytes"),
            Some(&Value::from(original.len()))
        );
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("t1"));
        assert_pairing(&messages);
    }

    #[test]
    fn oversized_recent_message_survives_pass_one_last_resort_collapses_it() {
        let huge = "z".repeat(20_000);
        let mut messages = vec![
            m("user", "u"),
            assistant_calling("t1"),
            tool_result("t1", &huge),
            m("assistant", "final"),
        ];
        let budget = 5_000;
        assert!(estimate_bytes("sys", &messages, 0) > budget);
        let first_content = messages[0].content.clone();
        let last_content = messages[3].content.clone();

        let report = trim("sys", &mut messages, 0, budget, 4.0);

        // All 4 messages are in the protected tail, so pass 1 must skip the
        // oversized message; pass 2 collapses it.
        assert!(report.collapsed >= 1);
        assert!(is_placeholder(&messages[2].content));
        assert!(!report.irreducible);
        assert!(report.estimated_after <= budget);
        assert_eq!(messages[0].content, first_content);
        assert_eq!(messages[3].content, last_content);
        assert_pairing(&messages);
    }

    #[test]
    fn last_message_over_budget_is_irreducible() {
        let huge = "z".repeat(20_000);
        // Final USER message: a final tool result would now be head-truncated
        // instead (see trim_head_truncates_lone_oversized_newest_tool_result).
        let mut messages = vec![m("user", "u"), assistant_calling("t1"), m("user", &huge)];
        let budget = 5_000;

        let report = trim("sys", &mut messages, 0, budget, 4.0);

        assert!(report.irreducible);
        assert_eq!(report.collapsed, 0);
        assert!(report.estimated_after > budget);
        assert_eq!(messages[2].content, huge);
        assert!(!is_placeholder(&messages[2].content));
    }

    #[test]
    fn pass_one_protects_last_assistant_tool_call_message() {
        let big = "x".repeat(8_000);
        // Parallel tool calls: the issuing assistant message sits at index 1,
        // outside the recent-4 tail (len 7 -> recent window starts at 3).
        let mut issuing = assistant_calling("t1");
        issuing.tool_calls.push(ToolCall {
            id: "t2".to_string(),
            name: "read".to_string(),
            arguments: "{}".to_string(),
        });
        issuing.reasoning = Some("thinking".to_string());
        issuing.reasoning_details = vec![json!({"provider": "x"})];
        issuing.native_content = vec![json!({"raw": true})];
        let mut messages = vec![
            m("user", "u"),
            issuing,
            tool_result("t1", "small"),
            tool_result("t2", &big),
            m("user", "filler"),
            m("assistant", "filler"),
            m("user", "end"),
        ];
        let recent_start = messages.len().saturating_sub(RECENT_PROTECT);
        assert_eq!(recent_start, 3);
        assert!(1 < recent_start);
        let budget = 1_000;
        assert!(estimate_bytes("sys", &messages, 0) > budget);

        let report = trim("sys", &mut messages, 0, budget, 4.0);

        // Pass 1 must not clear the issuing assistant's reasoning fields.
        assert!(messages[1].reasoning.is_some());
        assert!(!messages[1].reasoning_details.is_empty());
        assert!(!messages[1].native_content.is_empty());
        // Pass 2 behavior is unchanged: it starts at the recent window, so
        // t2 (index 3) collapses while t1 (index 2, before the window) does
        // not.
        assert!(report.collapsed >= 1);
        assert!(is_placeholder(&messages[3].content));
        assert!(!is_placeholder(&messages[2].content));
        assert_pairing(&messages);
    }

    #[test]
    fn reasoning_cleared_on_trimmed_assistant_messages() {
        let long = "r".repeat(3_000);
        let mut a1 = m("assistant", "thinking");
        a1.reasoning = Some(long.clone());
        a1.reasoning_details = vec![json!({"provider": "x"})];
        a1.native_content = vec![json!({"raw": true})];
        let mut a2 = m("assistant", "thinking too");
        a2.reasoning = Some(long.clone());
        let mut messages = vec![
            m("user", "u"),
            a1,
            tool_result("t1", "small"),
            a2,
            m("assistant", "done"),
            m("assistant", "end"),
        ];
        // protect_start = 2, so index 1 is eligible and index 3 is protected.
        // Budget below the estimate so pass 1 actually runs.
        let budget = estimate_bytes("sys", &messages, 0) - 1_000;

        let report = trim("sys", &mut messages, 0, budget, 4.0);

        assert_eq!(report.cleared_reasoning, 1);
        assert!(messages[1].reasoning.is_none());
        assert!(messages[1].reasoning_details.is_empty());
        assert!(messages[1].native_content.is_empty());
        // Protected assistant keeps its reasoning.
        assert_eq!(messages[3].reasoning.as_deref(), Some(long.as_str()));
    }
}

#[derive(Debug)]
pub struct ContextBudgetExceeded {
    pub estimated: usize,
    pub budget: usize,
}
impl std::fmt::Display for ContextBudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "context budget exceeded (~{} bytes estimated vs {} byte budget): split the task or read in slices", self.estimated, self.budget)
    }
}
impl std::error::Error for ContextBudgetExceeded {}
