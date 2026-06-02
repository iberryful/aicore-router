//! Per-family classifier for streaming SSE events. Classifies each event
//! as content / metadata / rate-limit so the proxy's pre-stream peek can
//! decide whether to commit, keep peeking, or fail over to another
//! provider.
//!
//! Why a tri-state instead of "first event commits": the upstream's first
//! event is usually metadata (`response.created`, `message_start`, an
//! initial role-only chat-completion delta, etc.). A rate-limit `error`
//! event from Azure Front Door arrives at sequence 2 or 3, after one or
//! two metadata events. If peek committed on the first event seen it
//! would leak the rate-limit through to the client. Treating metadata as
//! "keep peeking" lets us catch the trailing rate-limit before any bytes
//! reach the client.
//!
//! Captured 2026-05-26 from gpt-5.5 throttling on AI Core (file
//! `/tmp/replay_2.txt`): `response.created` → `response.failed` → `error`
//! with `error.code = "too_many_requests"`. The `response.failed`
//! event itself carries `error: null`; the actionable detail is in the
//! trailing `error` event, so `response.failed` is also classified as
//! metadata (keep peeking) — we'd otherwise commit one event too early.

use crate::proxy::LlmFamily;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventDisposition {
    /// Rate-limit / throttling signal. The pre-stream peek surfaces this
    /// as `ProxyExecuteResult::RateLimited` so the existing 429-retry
    /// loop fails over to the next provider.
    RateLimited,
    /// Content-bearing or terminal-success event — commit the stream
    /// forward to the client.
    Content,
    /// Opening / keepalive / metadata event — keep peeking; the
    /// definitive event (rate-limit OR content) hasn't arrived yet.
    Metadata,
}

/// Classify a single SSE `data:` payload (the JSON body, with the
/// `data: ` prefix already stripped). Unparseable / unknown shapes
/// default to [`EventDisposition::Content`] so the peek loop never
/// stalls — better to forward an unknown event than to wait until the
/// peek timeout.
pub fn classify_first_event(data: &str, family: &LlmFamily) -> EventDisposition {
    let Ok(parsed) = serde_json::from_str::<Value>(data) else {
        return EventDisposition::Content;
    };
    match family {
        LlmFamily::Claude => classify_claude(&parsed),
        LlmFamily::OpenAi => classify_openai_chat(&parsed),
        LlmFamily::OpenAiResponses => classify_openai_responses(&parsed),
        LlmFamily::Gemini => classify_gemini(&parsed),
    }
}

/// OpenAI Responses API. Verified 2026-05-26 against gpt-5.5 on AI Core.
///
/// * `error` event with rate-limit code/type → `RateLimited`.
/// * `response.failed` (always paired with a trailing `error` event in the
///   throttling case) → `Metadata` so we wait for the `error` event that
///   actually carries the code.
/// * `response.created`, `response.in_progress`, `response.queued`,
///   `response.output_item.added`, `response.content_part.added`,
///   `response.reasoning_*.added`, `ping` → `Metadata`.
/// * Anything ending in `.delta`, `.done`, plus `response.completed` and
///   `response.incomplete` → `Content`.
/// * Unknown `response.*` events default to `Content` (don't stall).
fn classify_openai_responses(parsed: &Value) -> EventDisposition {
    let Some(ty) = parsed.get("type").and_then(|v| v.as_str()) else {
        return EventDisposition::Content;
    };
    if ty == "error" {
        let err = parsed.get("error");
        if let Some(err) = err
            && (is_rate_limit_marker(err.get("type")) || is_rate_limit_marker(err.get("code")))
        {
            return EventDisposition::RateLimited;
        }
        // Non-rate-limit error event — let the forwarder pass it through;
        // the client decides what to do.
        return EventDisposition::Content;
    }
    if ty == "response.failed" {
        // Always paired with a following `error` event under throttling
        // (the `error: null` field on `response.failed` itself is
        // intentional). Wait for that `error` event before deciding.
        return EventDisposition::Metadata;
    }
    if ty == "response.completed" || ty == "response.incomplete" {
        return EventDisposition::Content;
    }
    if ty.ends_with(".delta") || ty.ends_with(".done") {
        return EventDisposition::Content;
    }
    // response.created / response.in_progress / response.queued /
    // response.output_item.added / response.content_part.added /
    // response.reasoning_*.added / ping
    EventDisposition::Metadata
}

/// OpenAI Chat Completions via Azure. Documented shapes (rate-limit
/// shape unverified live — re-check on first hit by capturing the raw
/// chunk).
///
/// * Top-level `error` field with rate-limit marker → `RateLimited`.
/// * `choices[*].delta.content` non-empty OR `choices[*].finish_reason`
///   non-null → `Content`.
/// * Initial role-only chunk (`delta = {"role":"assistant"}`) →
///   `Metadata`.
fn classify_openai_chat(parsed: &Value) -> EventDisposition {
    if let Some(err) = parsed.get("error") {
        if is_rate_limit_marker(err.get("code")) || is_rate_limit_marker(err.get("type")) {
            return EventDisposition::RateLimited;
        }
        return EventDisposition::Content;
    }
    if let Some(choices) = parsed.get("choices").and_then(|v| v.as_array()) {
        for choice in choices {
            if !choice
                .get("finish_reason")
                .map(|v| v.is_null())
                .unwrap_or(true)
            {
                return EventDisposition::Content;
            }
            if let Some(content) = choice
                .get("delta")
                .and_then(|d| d.get("content"))
                .and_then(|c| c.as_str())
                && !content.is_empty()
            {
                return EventDisposition::Content;
            }
        }
        return EventDisposition::Metadata;
    }
    EventDisposition::Content
}

/// Anthropic via Bedrock. AI Core unwraps the AWS event-stream binary
/// framing into SSE before acr sees it.
///
/// * `type == "error"` with rate-limit marker → `RateLimited` (covers
///   both Anthropic's `overloaded_error` shape and raw Bedrock
///   `ThrottlingException` strings in the `message` field).
/// * `message_start` / `content_block_start` / `ping` → `Metadata`.
/// * Everything else (incl. `content_block_delta`, `message_delta`,
///   `message_stop`) → `Content`.
fn classify_claude(parsed: &Value) -> EventDisposition {
    let Some(ty) = parsed.get("type").and_then(|v| v.as_str()) else {
        return EventDisposition::Content;
    };
    if ty == "error" {
        if let Some(err) = parsed.get("error")
            && is_rate_limit_marker(err.get("type"))
        {
            return EventDisposition::RateLimited;
        }
        if let Some(msg) = parsed.get("message").and_then(|v| v.as_str())
            && (msg.contains("ThrottlingException")
                || msg.contains("TooManyRequests")
                || msg.contains("ServiceUnavailableException"))
        {
            return EventDisposition::RateLimited;
        }
        return EventDisposition::Content;
    }
    match ty {
        "message_start" | "content_block_start" | "ping" => EventDisposition::Metadata,
        _ => EventDisposition::Content,
    }
}

/// Gemini via Vertex `streamGenerateContent`. Unverified live — re-check
/// shape on first hit.
///
/// * `error.code == 429` OR `error.status == "RESOURCE_EXHAUSTED"` →
///   `RateLimited`.
/// * Otherwise `Content` — Vertex chunks always carry candidate content,
///   no real metadata-only chunks.
fn classify_gemini(parsed: &Value) -> EventDisposition {
    if let Some(err) = parsed.get("error")
        && (err.get("code").and_then(|v| v.as_u64()) == Some(429)
            || err.get("status").and_then(|v| v.as_str()) == Some("RESOURCE_EXHAUSTED"))
    {
        return EventDisposition::RateLimited;
    }
    EventDisposition::Content
}

/// Match well-known rate-limit string codes/types across providers, plus
/// the integer `429` form (Vertex / generic Azure).
fn is_rate_limit_marker(v: Option<&Value>) -> bool {
    let Some(v) = v else { return false };
    if v.as_u64() == Some(429) {
        return true;
    }
    let Some(s) = v.as_str() else { return false };
    matches!(
        s,
        "too_many_requests"
            | "rate_limit_exceeded"
            | "rate_limit_error"
            | "overloaded_error"
            | "throttling_exception"
            | "ThrottlingException"
            | "429"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- OpenAiResponses ---------------------------------------------------

    #[test]
    fn responses_error_with_too_many_requests_flagged() {
        let data = json!({
            "type": "error",
            "error": {"type": "too_many_requests", "code": "too_many_requests"}
        })
        .to_string();
        assert_eq!(
            classify_first_event(&data, &LlmFamily::OpenAiResponses),
            EventDisposition::RateLimited
        );
    }

    #[test]
    fn responses_response_failed_is_metadata() {
        // The captured wire order is response.created → response.failed →
        // error; `response.failed.error` is null — the classifier must
        // keep peeking so it can see the trailing `error` event.
        let data = json!({
            "type": "response.failed",
            "response": {"status": "failed", "error": null}
        })
        .to_string();
        assert_eq!(
            classify_first_event(&data, &LlmFamily::OpenAiResponses),
            EventDisposition::Metadata
        );
    }

    #[test]
    fn responses_created_is_metadata() {
        let data = json!({
            "type": "response.created",
            "response": {"id": "r_1", "status": "in_progress"}
        })
        .to_string();
        assert_eq!(
            classify_first_event(&data, &LlmFamily::OpenAiResponses),
            EventDisposition::Metadata
        );
    }

    #[test]
    fn responses_in_progress_is_metadata() {
        let data = json!({"type": "response.in_progress"}).to_string();
        assert_eq!(
            classify_first_event(&data, &LlmFamily::OpenAiResponses),
            EventDisposition::Metadata
        );
    }

    #[test]
    fn responses_output_text_delta_is_content() {
        let data = json!({"type": "response.output_text.delta", "delta": "hi"}).to_string();
        assert_eq!(
            classify_first_event(&data, &LlmFamily::OpenAiResponses),
            EventDisposition::Content
        );
    }

    #[test]
    fn responses_completed_is_content() {
        let data = json!({"type": "response.completed", "response": {"usage": {}}}).to_string();
        assert_eq!(
            classify_first_event(&data, &LlmFamily::OpenAiResponses),
            EventDisposition::Content
        );
    }

    #[test]
    fn responses_output_item_added_is_metadata() {
        let data = json!({"type": "response.output_item.added", "item": {}}).to_string();
        assert_eq!(
            classify_first_event(&data, &LlmFamily::OpenAiResponses),
            EventDisposition::Metadata
        );
    }

    // -- OpenAi (Chat Completions) ----------------------------------------

    #[test]
    fn chat_top_level_rate_limit_error_flagged() {
        let data = json!({"error": {"code": "rate_limit_exceeded"}}).to_string();
        assert_eq!(
            classify_first_event(&data, &LlmFamily::OpenAi),
            EventDisposition::RateLimited
        );
    }

    #[test]
    fn chat_role_only_first_chunk_is_metadata() {
        let data = json!({
            "choices": [{"delta": {"role": "assistant"}, "index": 0}]
        })
        .to_string();
        assert_eq!(
            classify_first_event(&data, &LlmFamily::OpenAi),
            EventDisposition::Metadata
        );
    }

    #[test]
    fn chat_content_delta_is_content() {
        let data = json!({
            "choices": [{"delta": {"content": "hi"}, "index": 0}]
        })
        .to_string();
        assert_eq!(
            classify_first_event(&data, &LlmFamily::OpenAi),
            EventDisposition::Content
        );
    }

    #[test]
    fn chat_finish_reason_is_content() {
        let data = json!({
            "choices": [{"delta": {}, "finish_reason": "stop", "index": 0}]
        })
        .to_string();
        assert_eq!(
            classify_first_event(&data, &LlmFamily::OpenAi),
            EventDisposition::Content
        );
    }

    // -- Claude -----------------------------------------------------------

    #[test]
    fn claude_overloaded_flagged() {
        let data = json!({
            "type": "error",
            "error": {"type": "overloaded_error", "message": "Overloaded"}
        })
        .to_string();
        assert_eq!(
            classify_first_event(&data, &LlmFamily::Claude),
            EventDisposition::RateLimited
        );
    }

    #[test]
    fn claude_bedrock_throttling_message_flagged() {
        let data = json!({
            "type": "error",
            "message": "ThrottlingException: Rate exceeded"
        })
        .to_string();
        assert_eq!(
            classify_first_event(&data, &LlmFamily::Claude),
            EventDisposition::RateLimited
        );
    }

    #[test]
    fn claude_message_start_is_metadata() {
        let data = json!({"type": "message_start", "message": {"id": "m"}}).to_string();
        assert_eq!(
            classify_first_event(&data, &LlmFamily::Claude),
            EventDisposition::Metadata
        );
    }

    #[test]
    fn claude_content_block_delta_is_content() {
        let data =
            json!({"type": "content_block_delta", "delta": {"type": "text_delta"}}).to_string();
        assert_eq!(
            classify_first_event(&data, &LlmFamily::Claude),
            EventDisposition::Content
        );
    }

    // -- Gemini -----------------------------------------------------------

    #[test]
    fn gemini_resource_exhausted_flagged() {
        let data = json!({
            "error": {"code": 429, "status": "RESOURCE_EXHAUSTED"}
        })
        .to_string();
        assert_eq!(
            classify_first_event(&data, &LlmFamily::Gemini),
            EventDisposition::RateLimited
        );
    }

    #[test]
    fn gemini_normal_chunk_is_content() {
        let data = json!({
            "candidates": [{"content": {"parts": [{"text": "hi"}], "role": "model"}}]
        })
        .to_string();
        assert_eq!(
            classify_first_event(&data, &LlmFamily::Gemini),
            EventDisposition::Content
        );
    }

    // -- malformed --------------------------------------------------------

    #[test]
    fn malformed_json_defaults_to_content() {
        // Better to forward unknown shapes than to stall the peek loop.
        for fam in [
            LlmFamily::OpenAiResponses,
            LlmFamily::OpenAi,
            LlmFamily::Claude,
            LlmFamily::Gemini,
        ] {
            assert_eq!(
                classify_first_event("not json at all", &fam),
                EventDisposition::Content
            );
        }
    }
}
