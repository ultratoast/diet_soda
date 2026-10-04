//! Machine-readable classification of delegate/tool errors. The class is
//! attached to error payloads so downstream consumers can branch on a stable
//! string instead of matching rendered message text.

/// Machine-readable failure class for a delegate/tool error.
pub fn classify(error: &anyhow::Error) -> &'static str {
    if error
        .downcast_ref::<crate::engine::context_budget::ContextBudgetExceeded>()
        .is_some()
    {
        return "context_budget_exceeded";
    }
    if let Some(e) = error.downcast_ref::<crate::provider::ProviderHttpError>() {
        let b = e.body.to_ascii_lowercase();
        let overflow = matches!(e.status.as_u16(), 400 | 413)
            && (b.contains("maximum context length")
                || b.contains("context length")
                || b.contains("context_length_exceeded")
                || b.contains("prompt is too long")
                || b.contains("too many tokens"));
        return if overflow {
            "provider_context_overflow"
        } else {
            "provider_http"
        };
    }
    if let Some(e) = error.downcast_ref::<crate::provider::IncompleteStreamError>() {
        return if e.empty {
            "empty_response"
        } else {
            "streaming_error"
        };
    }
    let s = format!("{error:#}");
    if s.contains("timed out") {
        return "timeout";
    }
    if s.contains("Maximum model turns") {
        return "max_turns";
    }
    if s.contains("Cancelled") {
        return "cancelled";
    }
    "other"
}

#[cfg(test)]
mod tests {
    use super::classify;
    use crate::engine::context_budget::ContextBudgetExceeded;
    use crate::model::{Message, Usage};
    use crate::provider::{IncompleteStreamError, ProviderHttpError};

    #[test]
    fn http_400_with_context_length_body_is_provider_context_overflow() {
        let e = anyhow::Error::new(ProviderHttpError {
            status: reqwest::StatusCode::BAD_REQUEST,
            body: r#"{"error":{"message":"This model's maximum context length is 8192 tokens"}}"#
                .to_string(),
        });
        assert_eq!(classify(&e), "provider_context_overflow");
    }

    #[test]
    fn anthropic_prompt_too_long_is_provider_context_overflow() {
        let e = anyhow::Error::new(ProviderHttpError {
            status: reqwest::StatusCode::from_u16(413).unwrap(),
            body: r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens > 200000 maximum"}}"#.to_string(),
        });
        assert_eq!(classify(&e), "provider_context_overflow");
    }

    #[test]
    fn openai_context_length_exceeded_is_provider_context_overflow() {
        let e = anyhow::Error::new(ProviderHttpError {
            status: reqwest::StatusCode::BAD_REQUEST,
            body: r#"{"error":{"message":"This model's maximum context length is 8192 tokens","code":"context_length_exceeded"}}"#.to_string(),
        });
        assert_eq!(classify(&e), "provider_context_overflow");
    }

    #[test]
    fn http_400_with_unrelated_body_is_provider_http() {
        let e = anyhow::Error::new(ProviderHttpError {
            status: reqwest::StatusCode::BAD_REQUEST,
            body: r#"{"error":{"message":"invalid api key"}}"#.to_string(),
        });
        assert_eq!(classify(&e), "provider_http");
    }

    #[test]
    fn http_413_with_too_many_tokens_is_provider_context_overflow() {
        let e = anyhow::Error::new(ProviderHttpError {
            status: reqwest::StatusCode::from_u16(413).unwrap(),
            body: r#"{"error":{"message":"too many tokens in request"}}"#.to_string(),
        });
        assert_eq!(classify(&e), "provider_context_overflow");
    }

    #[test]
    fn http_500_is_provider_http() {
        let e = anyhow::Error::new(ProviderHttpError {
            status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            body: "boom".to_string(),
        });
        assert_eq!(classify(&e), "provider_http");
    }

    #[test]
    fn context_budget_exceeded_is_context_budget_exceeded() {
        let e = anyhow::Error::new(ContextBudgetExceeded {
            estimated: 10,
            budget: 5,
        });
        assert_eq!(classify(&e), "context_budget_exceeded");
    }

    #[test]
    fn timeout_message_is_timeout() {
        let e = anyhow::anyhow!("Agent execution timed out");
        assert_eq!(classify(&e), "timeout");
    }

    #[test]
    fn generic_error_is_other() {
        let e = anyhow::anyhow!("something unspecified broke");
        assert_eq!(classify(&e), "other");
    }

    #[test]
    fn empty_incomplete_stream_is_empty_response() {
        let e = anyhow::Error::new(IncompleteStreamError {
            message: Message::new("assistant", ""),
            reason: "no visible output".to_string(),
            usage: Some(Usage::default()),
            empty: true,
        });
        assert_eq!(classify(&e), "empty_response");
    }

    #[test]
    fn non_empty_incomplete_stream_is_streaming_error() {
        let e = anyhow::Error::new(IncompleteStreamError {
            message: Message::new("assistant", "partial"),
            reason: "connection reset".to_string(),
            usage: None,
            empty: false,
        });
        assert_eq!(classify(&e), "streaming_error");
    }
}
