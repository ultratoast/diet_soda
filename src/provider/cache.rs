//! OpenRouter prompt-cache request markers. Anthropic models need explicit
//! `cache_control`; other providers cache automatically, so they get no markers.
use serde_json::{json, Value};

/// True when an OpenRouter model id names an Anthropic model (a leading `~`
/// router-alias prefix is ignored).
pub(super) fn wants_prompt_cache(model: &str) -> bool {
    model.trim_start_matches('~').starts_with("anthropic/")
}

/// Add (1) top-level automatic caching, whose breakpoint follows the end of the
/// growing conversation, and (2) an explicit breakpoint on the system message so
/// tools + system prompt stay cached even when history outgrows the 20-block
/// lookback window. Idempotent; never changes message text.
pub(super) fn apply_prompt_cache(body: &mut Value) {
    body["cache_control"] = json!({"type": "ephemeral"});
    let Some(first) = body["messages"].get_mut(0) else {
        return;
    };
    if first["role"] != "system" {
        return;
    }
    let text = match first["content"].as_str() {
        Some(t) if !t.is_empty() => t.to_owned(),
        _ => return,
    };
    first["content"] = json!([{"type":"text","text":text,"cache_control":{"type":"ephemeral"}}]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wants_prompt_cache_only_for_anthropic() {
        assert!(wants_prompt_cache("anthropic/claude-sonnet-4"));
        assert!(wants_prompt_cache("~anthropic/claude-sonnet-latest"));
        assert!(!wants_prompt_cache("openai/gpt-4.1-mini"));
        assert!(!wants_prompt_cache("google/gemini-2.5-pro"));
        assert!(!wants_prompt_cache("x-anthropic/foo"));
    }

    #[test]
    fn apply_prompt_cache_sets_markers() {
        let mut body = json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "SYS"},
                {"role": "user", "content": "hi"}
            ]
        });
        apply_prompt_cache(&mut body);

        assert_eq!(body["cache_control"]["type"], "ephemeral");

        let content = &body["messages"][0]["content"];
        let arr = content.as_array().expect("content should be an array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["text"], "SYS");
        assert_eq!(arr[0]["cache_control"]["type"], "ephemeral");

        assert_eq!(
            body["messages"][1],
            json!({"role": "user", "content": "hi"})
        );
    }

    #[test]
    fn apply_prompt_cache_is_idempotent() {
        let mut once = json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "SYS"},
                {"role": "user", "content": "hi"}
            ]
        });
        apply_prompt_cache(&mut once);

        let mut twice = once.clone();
        apply_prompt_cache(&mut twice);

        assert_eq!(once, twice);
    }

    #[test]
    fn apply_prompt_cache_without_system_message_is_safe() {
        let original = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "hi"}
            ]
        });
        let mut body = original.clone();
        apply_prompt_cache(&mut body);
        assert_eq!(body["messages"], original["messages"]);
        assert_eq!(body["cache_control"]["type"], "ephemeral");

        let mut empty = json!({"model": "m", "messages": []});
        apply_prompt_cache(&mut empty);
        assert_eq!(empty["messages"], json!([]));
        assert_eq!(empty["cache_control"]["type"], "ephemeral");

        let mut missing = json!({"model": "m"});
        apply_prompt_cache(&mut missing);
        // Indexing a missing key inserts null; the call must simply not panic.
        assert!(missing.get("messages").is_none() || missing["messages"].is_null());
        assert_eq!(missing["cache_control"]["type"], "ephemeral");
    }
}
