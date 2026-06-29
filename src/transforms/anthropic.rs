//! Anthropic / Claude (via AWS Bedrock) request shaping.
//!
//! Source-of-truth references:
//! * Messages API & cache_control: <https://docs.claude.com/en/api/messages>
//! * Prompt caching (TTL semantics, ephemeral blocks):
//!   <https://docs.claude.com/en/docs/build-with-claude/prompt-caching>
//! * Extended thinking: <https://docs.claude.com/en/docs/build-with-claude/extended-thinking>
//! * Beta headers: <https://docs.claude.com/en/api/beta-headers>
//! * Bedrock-specific request shape (acceptable fields, `anthropic_version`,
//!   `anthropic_beta`): <https://docs.aws.amazon.com/bedrock/latest/userguide/model-parameters-anthropic-claude-messages.html>

use anyhow::Result;
use axum::http::HeaderMap;
use serde_json::{Map, Value, json};

use crate::constants::api::{
    ANTHROPIC_BETA_HEADER, ANTHROPIC_DEFAULT_MAX_TOKENS, ANTHROPIC_TO_BEDROCK_BETA_REMAP,
    ANTHROPIC_VERSION, BUDGET_RESERVE_MARGIN, MIN_BUDGET_TOKENS_FOR_THINKING,
};
use crate::constants::models::{CLAUDE_OPUS_4_7, CLAUDE_OPUS_4_8};

/// Prepare a Claude request body for Bedrock.
///
/// Steps (order is load-bearing):
/// 1. Validate the messages array (fail fast on obvious client bugs).
/// 2. Stamp `anthropic_version`, drop fields Bedrock doesn't accept, default `max_tokens`.
/// 3. Strip `cache_control.scope` (sent by Claude Code 2.1.88+, rejected by Bedrock).
/// 4. Inject `ttl: "1h"` into ephemeral cache_control blocks (extends Bedrock's prompt
///    cache from 5min default to 1h — net win for acr's interactive workload).
/// 5. Clamp / disable `thinking` to satisfy Bedrock's budget constraints.
/// 6. Apply adaptive-thinking model overrides last so they see the post-clamp `thinking`.
pub fn prepare(body: &mut Value, model: &str) -> Result<()> {
    validate_messages(body)?;

    let Some(obj) = body.as_object_mut() else {
        return Ok(());
    };

    obj.insert("anthropic_version".to_string(), json!(ANTHROPIC_VERSION));
    obj.remove("stream");
    obj.remove("model");
    obj.remove("context_management");

    if !obj.contains_key("max_tokens") {
        obj.insert(
            "max_tokens".to_string(),
            json!(ANTHROPIC_DEFAULT_MAX_TOKENS),
        );
    }

    strip_cache_control_scope(obj);
    inject_cache_ttl(obj);
    clamp_thinking(obj);

    if requires_adaptive_thinking(model) {
        apply_adaptive_thinking_overrides(obj);
    }

    Ok(())
}

/// Parse the `Anthropic-Beta` header into a list of beta features for the Bedrock
/// request body, applying the Anthropic→Bedrock policy in
/// [`ANTHROPIC_TO_BEDROCK_BETA_REMAP`]: rename, drop (Bedrock-incompatible), or
/// transparent passthrough for unknown names. Returns an empty vec if the header
/// is absent or non-UTF-8.
pub fn extract_anthropic_beta(headers: &HeaderMap) -> Vec<String> {
    let header_value = match headers.get(ANTHROPIC_BETA_HEADER) {
        Some(v) => match v.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => return vec![],
        },
        None => return vec![],
    };

    let mut features = Vec::new();
    for raw in header_value.split(',') {
        let feature = raw.trim().to_lowercase();
        if feature.is_empty() {
            continue;
        }
        // Three-way lookup: Some(other) → rename, None → drop,
        // missing entry → passthrough unchanged.
        let mapped = match ANTHROPIC_TO_BEDROCK_BETA_REMAP
            .iter()
            .find(|&&(anthropic, _)| feature == anthropic)
        {
            Some((_, None)) => continue,
            Some((_, Some(bedrock))) => bedrock.to_string(),
            None => feature,
        };
        if !features.contains(&mapped) {
            features.push(mapped);
        }
    }
    features
}

/// Validate the messages array is non-empty and messages have content.
/// The last message may be an empty assistant message (pre-fill pattern).
fn validate_messages(body: &Value) -> Result<()> {
    let Some(messages) = body.get("messages").and_then(|v| v.as_array()) else {
        return Ok(());
    };

    if messages.is_empty() {
        anyhow::bail!("messages array cannot be empty");
    }

    for (i, msg) in messages.iter().enumerate() {
        let content = msg.get("content");
        let is_empty = match content {
            None => true,
            Some(Value::String(s)) => s.is_empty(),
            Some(Value::Array(a)) => a.is_empty(),
            Some(Value::Null) => true,
            _ => false,
        };

        if is_empty {
            let is_last = i == messages.len() - 1;
            let is_assistant = msg.get("role").and_then(|v| v.as_str()) == Some("assistant");
            if !is_last || !is_assistant {
                anyhow::bail!(
                    "message at index {} has empty content (only the last assistant message may be empty)",
                    i
                );
            }
        }
    }

    Ok(())
}

/// Strip the unsupported `scope` field from `cache_control` blocks in `system` and message
/// content. Claude Code 2.1.88+ adds this field; Bedrock rejects it.
fn strip_cache_control_scope(obj: &mut Map<String, Value>) {
    for_each_cache_control(obj, |cc| {
        cc.remove("scope");
    });
}

/// Inject `ttl: "1h"` into ephemeral `cache_control` blocks that don't already specify a ttl.
/// Idempotent. Per the Anthropic Prompt Caching docs, `ttl` is only meaningful on
/// `type: "ephemeral"` blocks and accepts the literal values `"5m"` (default) or `"1h"`.
fn inject_cache_ttl(obj: &mut Map<String, Value>) {
    for_each_cache_control(obj, |cc| {
        if cc.get("type").and_then(|v| v.as_str()) == Some("ephemeral") && !cc.contains_key("ttl") {
            cc.insert("ttl".to_string(), json!("1h"));
        }
    });
}

/// Walk every `cache_control` object inside a Claude request body — top-level
/// `system` content, every `messages[].content` block, and every entry in
/// `tools[]` — and apply `f` to each. Centralizes the traversal so individual
/// transforms focus on the per-block edit.
///
/// Per Anthropic's prompt-caching docs, `cache_control` is valid on tool
/// definitions in the `tools` array (the first cacheable block in the
/// `tools → system → messages` hierarchy), in addition to system and message
/// content blocks. Bedrock rejects unknown sibling fields like `cache_control.scope`
/// on tool definitions (verified empirically), so the walker must include `tools`.
fn for_each_cache_control<F: FnMut(&mut Map<String, Value>)>(
    obj: &mut Map<String, Value>,
    mut f: F,
) {
    if let Some(system) = obj.get_mut("system") {
        visit_cache_control_in_content(system, &mut f);
    }
    if let Some(Value::Array(messages)) = obj.get_mut("messages") {
        for message in messages.iter_mut() {
            if let Some(content) = message.get_mut("content") {
                visit_cache_control_in_content(content, &mut f);
            }
        }
    }
    // Tool definitions can carry `cache_control` directly on the tool object.
    if let Some(Value::Array(tools)) = obj.get_mut("tools") {
        for tool in tools.iter_mut() {
            if let Some(Value::Object(cc)) = tool.get_mut("cache_control") {
                f(cc);
            }
        }
    }
}

fn visit_cache_control_in_content<F: FnMut(&mut Map<String, Value>)>(
    content: &mut Value,
    f: &mut F,
) {
    match content {
        Value::Array(blocks) => {
            for block in blocks.iter_mut() {
                if let Some(Value::Object(cc)) = block.get_mut("cache_control") {
                    f(cc);
                }
            }
        }
        Value::Object(obj) => {
            if let Some(Value::Object(cc)) = obj.get_mut("cache_control") {
                f(cc);
            }
        }
        _ => {}
    }
}

/// Validate and clamp the `thinking` block for Bedrock compatibility.
///
/// * Disables thinking if `max_tokens < MIN_BUDGET_TOKENS_FOR_THINKING + BUDGET_RESERVE_MARGIN`
/// * Ensures `budget_tokens >= MIN_BUDGET_TOKENS_FOR_THINKING` (Anthropic minimum)
/// * Clamps `budget_tokens < max_tokens` (Bedrock constraint)
fn clamp_thinking(obj: &mut Map<String, Value>) {
    let thinking = match obj.get("thinking") {
        Some(t) if t.is_object() => t,
        _ => return,
    };

    let thinking_type = thinking.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if thinking_type != "enabled" {
        return;
    }

    let max_tokens = obj.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(0);

    let min_required = MIN_BUDGET_TOKENS_FOR_THINKING + BUDGET_RESERVE_MARGIN;

    if max_tokens < min_required {
        tracing::debug!(
            "Disabling thinking: max_tokens ({}) < minimum required ({})",
            max_tokens,
            min_required
        );
        obj.remove("thinking");
        return;
    }

    let budget_tokens = thinking
        .get("budget_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let mut new_budget = if budget_tokens > 0 && budget_tokens < MIN_BUDGET_TOKENS_FOR_THINKING {
        MIN_BUDGET_TOKENS_FOR_THINKING
    } else {
        budget_tokens
    };

    if new_budget >= max_tokens {
        new_budget = max_tokens - BUDGET_RESERVE_MARGIN;
    }

    if new_budget != budget_tokens {
        tracing::debug!(
            "Clamping thinking budget_tokens: {} -> {} (max_tokens: {})",
            budget_tokens,
            new_budget,
            max_tokens
        );
        if let Some(thinking_obj) = obj.get_mut("thinking").and_then(|t| t.as_object_mut()) {
            thinking_obj.insert("budget_tokens".to_string(), json!(new_budget));
        }
    }
}

/// Reports whether the resolved client-facing model requires acr's
/// adaptive-thinking transform. Receives the normalized name (e.g.
/// `claude-opus-4-7`), not the AI Core internal name.
///
/// Both Opus 4.7 and Opus 4.8 reject `thinking.type.enabled` upstream and
/// require `thinking.type.adaptive`, and both deprecate `temperature` /
/// `top_p` / `top_k` at the model level (verified via empirical probe against
/// AI Core gpt-5.5-style 400 responses). Cross-checked against litellm's
/// `anthropic/common_utils.py::_supports_sampling_params`, which groups
/// Opus 4.7, Opus 4.8, and Fable 5 together for the same reason. hai-proxy
/// and LiteLLM track the same set; add new entries here when they ship.
fn requires_adaptive_thinking(model: &str) -> bool {
    matches!(model, CLAUDE_OPUS_4_7 | CLAUDE_OPUS_4_8)
}

/// Apply adaptive-thinking-model request body overrides:
///
/// * Strip `temperature`, `top_p`, `top_k`. These models control their own
///   sampling under adaptive thinking; clients that send these otherwise hit
///   upstream rejection.
/// * Convert `thinking: {type: "enabled", budget_tokens: N}` to `thinking: {type: "adaptive"}`.
///   Adaptive, disabled, or missing thinking is left unchanged.
///
/// Idempotent. Must run **after** `clamp_thinking` so any thinking-disabling caused by
/// insufficient `max_tokens` has already taken effect.
fn apply_adaptive_thinking_overrides(obj: &mut Map<String, Value>) {
    obj.remove("temperature");
    obj.remove("top_p");
    obj.remove("top_k");

    if let Some(thinking) = obj.get_mut("thinking").and_then(|t| t.as_object_mut())
        && thinking.get("type").and_then(|v| v.as_str()) == Some("enabled")
    {
        thinking.remove("budget_tokens");
        thinking.insert("type".to_string(), json!("adaptive"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_adaptive_thinking_predicate() {
        assert!(requires_adaptive_thinking("claude-opus-4-7"));
        assert!(requires_adaptive_thinking("claude-opus-4-8"));
        assert!(!requires_adaptive_thinking("claude-opus-4-6"));
        assert!(!requires_adaptive_thinking("claude-sonnet-4-6"));
        assert!(
            !requires_adaptive_thinking("anthropic--claude-4.7-opus"),
            "predicate keys on resolved client name, not internal"
        );
        assert!(
            !requires_adaptive_thinking("anthropic--claude-4.8-opus"),
            "predicate keys on resolved client name, not internal"
        );
        assert!(!requires_adaptive_thinking(""));
    }

    #[test]
    fn adaptive_thinking_overrides_strip_sampling_params() {
        let mut body = json!({
            "max_tokens": 2048,
            "temperature": 0.7,
            "top_p": 0.9,
            "top_k": 40,
            "messages": [{"role": "user", "content": "hi"}],
        });
        let obj = body.as_object_mut().unwrap();
        apply_adaptive_thinking_overrides(obj);

        assert!(!obj.contains_key("temperature"));
        assert!(!obj.contains_key("top_p"));
        assert!(!obj.contains_key("top_k"));
        assert_eq!(obj["max_tokens"], json!(2048));
        assert!(obj.contains_key("messages"));
    }

    #[test]
    fn adaptive_thinking_overrides_convert_enabled_thinking_to_adaptive() {
        let mut body = json!({
            "thinking": {"type": "enabled", "budget_tokens": 5000},
        });
        let obj = body.as_object_mut().unwrap();
        apply_adaptive_thinking_overrides(obj);

        let thinking = obj["thinking"].as_object().unwrap();
        assert_eq!(thinking["type"], json!("adaptive"));
        assert!(!thinking.contains_key("budget_tokens"));
    }

    #[test]
    fn adaptive_thinking_overrides_leave_adaptive_thinking_unchanged() {
        let mut body = json!({"thinking": {"type": "adaptive"}});
        let obj = body.as_object_mut().unwrap();
        apply_adaptive_thinking_overrides(obj);

        assert_eq!(obj["thinking"], json!({"type": "adaptive"}));
    }

    #[test]
    fn adaptive_thinking_overrides_handle_missing_thinking() {
        let mut body = json!({"max_tokens": 2048});
        let obj = body.as_object_mut().unwrap();
        apply_adaptive_thinking_overrides(obj);

        assert!(!obj.contains_key("thinking"));
    }

    #[test]
    fn adaptive_thinking_overrides_idempotent() {
        let mut body = json!({
            "temperature": 0.7,
            "thinking": {"type": "enabled", "budget_tokens": 5000},
        });
        let obj = body.as_object_mut().unwrap();
        apply_adaptive_thinking_overrides(obj);
        let after_first = obj.clone();
        apply_adaptive_thinking_overrides(obj);
        assert_eq!(*obj, after_first);
    }

    #[test]
    fn prepare_opus_4_7_full_overrides() {
        let mut body = json!({
            "max_tokens": 4096,
            "temperature": 0.7,
            "top_p": 0.9,
            "top_k": 40,
            "thinking": {"type": "enabled", "budget_tokens": 2000},
            "messages": [{"role": "user", "content": "hi"}],
        });
        prepare(&mut body, "claude-opus-4-7").unwrap();

        let obj = body.as_object().unwrap();
        assert!(!obj.contains_key("temperature"));
        assert!(!obj.contains_key("top_p"));
        assert!(!obj.contains_key("top_k"));
        let thinking = obj["thinking"].as_object().unwrap();
        assert_eq!(thinking["type"], json!("adaptive"));
        assert!(!thinking.contains_key("budget_tokens"));
    }

    #[test]
    fn prepare_opus_4_8_full_overrides() {
        let mut body = json!({
            "max_tokens": 4096,
            "temperature": 0.7,
            "top_p": 0.9,
            "top_k": 40,
            "thinking": {"type": "enabled", "budget_tokens": 2000},
            "messages": [{"role": "user", "content": "hi"}],
        });
        prepare(&mut body, "claude-opus-4-8").unwrap();

        let obj = body.as_object().unwrap();
        assert!(!obj.contains_key("temperature"));
        assert!(!obj.contains_key("top_p"));
        assert!(!obj.contains_key("top_k"));
        let thinking = obj["thinking"].as_object().unwrap();
        assert_eq!(thinking["type"], json!("adaptive"));
        assert!(!thinking.contains_key("budget_tokens"));
    }

    #[test]
    fn prepare_non_opus_4_7_preserves_request() {
        let mut body = json!({
            "max_tokens": 4096,
            "temperature": 0.7,
            "top_p": 0.9,
            "top_k": 40,
            "thinking": {"type": "enabled", "budget_tokens": 2000},
            "messages": [{"role": "user", "content": "hi"}],
        });
        prepare(&mut body, "claude-opus-4-6").unwrap();

        let obj = body.as_object().unwrap();
        assert_eq!(obj["temperature"], json!(0.7));
        assert_eq!(obj["top_p"], json!(0.9));
        assert_eq!(obj["top_k"], json!(40));
        let thinking = obj["thinking"].as_object().unwrap();
        assert_eq!(thinking["type"], json!("enabled"));
        assert_eq!(thinking["budget_tokens"], json!(2000));
    }

    #[test]
    fn strip_cache_control_scope_removes_field_from_block_array() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "x", "cache_control": {"type": "ephemeral", "scope": "session"}},
                    {"type": "text", "text": "y"}
                ]}
            ]
        });
        let obj = body.as_object_mut().unwrap();
        strip_cache_control_scope(obj);

        let block = &obj["messages"][0]["content"][0];
        assert_eq!(
            block["cache_control"],
            json!({"type": "ephemeral"}),
            "scope removed; other cache_control fields preserved"
        );
    }

    #[test]
    fn strip_scope_walks_system_and_messages() {
        let mut body = json!({
            "system": [
                {"type": "text", "text": "sys", "cache_control": {"type": "ephemeral", "scope": "x"}}
            ],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral", "scope": "y"}}
                ]}
            ]
        });
        let obj = body.as_object_mut().unwrap();
        strip_cache_control_scope(obj);

        assert!(
            !obj["system"][0]["cache_control"]
                .as_object()
                .unwrap()
                .contains_key("scope")
        );
        assert!(
            !obj["messages"][0]["content"][0]["cache_control"]
                .as_object()
                .unwrap()
                .contains_key("scope")
        );
    }

    #[test]
    fn validate_messages_rejects_empty_array() {
        let body = json!({"messages": []});
        assert!(validate_messages(&body).is_err());
    }

    #[test]
    fn validate_messages_allows_trailing_empty_assistant() {
        let body = json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": ""}
        ]});
        assert!(validate_messages(&body).is_ok());
    }

    #[test]
    fn validate_messages_rejects_empty_user() {
        let body = json!({"messages": [
            {"role": "user", "content": ""},
            {"role": "assistant", "content": "ok"}
        ]});
        assert!(validate_messages(&body).is_err());
    }

    #[test]
    fn extract_anthropic_beta_remaps_known_features() {
        let mut headers = HeaderMap::new();
        headers.insert(
            ANTHROPIC_BETA_HEADER,
            "context-1m-2025-08-07, advanced-tool-use-2025-11-20"
                .parse()
                .unwrap(),
        );
        let beta = extract_anthropic_beta(&headers);
        // `advanced-tool-use-*` is remapped to its Bedrock-equivalent name.
        assert!(beta.contains(&"context-1m-2025-08-07".to_string()));
        assert!(beta.contains(&"tool-search-tool-2025-10-19".to_string()));
    }

    #[test]
    fn extract_anthropic_beta_passes_through_unknown_features() {
        // Per the transparent-proxy principle: features not in the remap table
        // are emitted unchanged. Bedrock decides what it supports.
        let mut headers = HeaderMap::new();
        headers.insert(
            ANTHROPIC_BETA_HEADER,
            "made-up-future-beta-2099-01-01".parse().unwrap(),
        );
        let beta = extract_anthropic_beta(&headers);
        assert_eq!(beta, vec!["made-up-future-beta-2099-01-01".to_string()]);
    }

    #[test]
    fn extract_anthropic_beta_deduplicates_after_remap() {
        // Both names map to the same Bedrock name; result should appear once.
        let mut headers = HeaderMap::new();
        headers.insert(
            ANTHROPIC_BETA_HEADER,
            "advanced-tool-use-2025-11-20, tool-search-tool-2025-10-19"
                .parse()
                .unwrap(),
        );
        let beta = extract_anthropic_beta(&headers);
        assert_eq!(beta, vec!["tool-search-tool-2025-10-19".to_string()]);
    }

    // (note: previous "extract_anthropic_beta_drops_unknown_features" test was
    // removed — the function now passes through unknown betas. See
    // `extract_anthropic_beta_passes_through_unknown_features` above.)

    #[test]
    fn extract_anthropic_beta_drops_blacklisted_features() {
        // Blacklisted entries (mapped to None) are stripped silently. Other
        // features in the same header travel through normally — verifying the
        // drop is per-entry, not all-or-nothing.
        let mut headers = HeaderMap::new();
        headers.insert(
            ANTHROPIC_BETA_HEADER,
            "prompt-caching-scope-2026-01-05, context-1m-2025-08-07, prompt-caching-2024-07-31, interleaved-thinking-2025-05-14"
                .parse()
                .unwrap(),
        );
        let beta = extract_anthropic_beta(&headers);
        assert_eq!(
            beta,
            vec![
                "context-1m-2025-08-07".to_string(),
                "interleaved-thinking-2025-05-14".to_string(),
            ]
        );
    }

    #[test]
    fn clamp_thinking_disables_when_max_tokens_too_small() {
        let mut body = json!({
            "max_tokens": 100,
            "thinking": {"type": "enabled", "budget_tokens": 2000}
        });
        let obj = body.as_object_mut().unwrap();
        clamp_thinking(obj);
        assert!(!obj.contains_key("thinking"));
    }

    #[test]
    fn clamp_thinking_clamps_budget_below_max_tokens() {
        let mut body = json!({
            "max_tokens": 2000,
            "thinking": {"type": "enabled", "budget_tokens": 5000}
        });
        let obj = body.as_object_mut().unwrap();
        clamp_thinking(obj);
        assert_eq!(obj["thinking"]["budget_tokens"], json!(1999));
    }

    // -------------------------------------------------------------------------
    // Cache TTL extension
    // -------------------------------------------------------------------------

    #[test]
    fn inject_cache_ttl_writes_1h_into_ephemeral_blocks() {
        let mut body = json!({
            "system": [
                {"type": "text", "text": "sys", "cache_control": {"type": "ephemeral"}}
            ],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}
                ]}
            ]
        });
        let obj = body.as_object_mut().unwrap();
        inject_cache_ttl(obj);

        assert_eq!(obj["system"][0]["cache_control"]["ttl"], json!("1h"));
        assert_eq!(
            obj["messages"][0]["content"][0]["cache_control"]["ttl"],
            json!("1h")
        );
    }

    #[test]
    fn inject_cache_ttl_is_idempotent_when_ttl_already_set() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "hi",
                     "cache_control": {"type": "ephemeral", "ttl": "5m"}}
                ]}
            ]
        });
        let obj = body.as_object_mut().unwrap();
        inject_cache_ttl(obj);

        assert_eq!(
            obj["messages"][0]["content"][0]["cache_control"]["ttl"],
            json!("5m"),
            "preserves any existing ttl set by the client"
        );
    }

    #[test]
    fn inject_cache_ttl_skips_non_ephemeral_blocks() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "hi", "cache_control": {"type": "persistent"}}
                ]}
            ]
        });
        let obj = body.as_object_mut().unwrap();
        inject_cache_ttl(obj);

        let cc = obj["messages"][0]["content"][0]["cache_control"]
            .as_object()
            .unwrap();
        assert!(!cc.contains_key("ttl"));
    }

    // -------------------------------------------------------------------------
    // tools[] cache_control coverage
    // -------------------------------------------------------------------------

    #[test]
    fn strip_cache_control_scope_removes_scope_from_tools() {
        let mut body = json!({
            "tools": [{
                "name": "weather",
                "description": "Get the weather",
                "input_schema": {"type": "object", "properties": {}},
                "cache_control": {"type": "ephemeral", "scope": "session"}
            }],
            "messages": [{"role": "user", "content": "hi"}]
        });
        let obj = body.as_object_mut().unwrap();
        strip_cache_control_scope(obj);

        let cc = obj["tools"][0]["cache_control"].as_object().unwrap();
        assert!(
            !cc.contains_key("scope"),
            "scope must be stripped from tool cache_control"
        );
        assert_eq!(cc["type"], json!("ephemeral"));
    }

    #[test]
    fn inject_cache_ttl_writes_to_tool_cache_control() {
        let mut body = json!({
            "tools": [{
                "name": "weather",
                "description": "Get the weather",
                "input_schema": {"type": "object", "properties": {}},
                "cache_control": {"type": "ephemeral"}
            }],
            "messages": [{"role": "user", "content": "hi"}]
        });
        let obj = body.as_object_mut().unwrap();
        inject_cache_ttl(obj);

        assert_eq!(obj["tools"][0]["cache_control"]["ttl"], json!("1h"));
    }

    #[test]
    fn prepare_strips_and_extends_ttl_on_tool_cache_control() {
        // End-to-end through prepare(): a tool with both scope (must be stripped)
        // and no ttl (must be injected).
        let mut body = json!({
            "tools": [{
                "name": "weather",
                "description": "Get the weather",
                "input_schema": {"type": "object", "properties": {}},
                "cache_control": {"type": "ephemeral", "scope": "session"}
            }],
            "messages": [{"role": "user", "content": "hi"}]
        });
        prepare(&mut body, "claude-sonnet-4-6").unwrap();

        let cc = body["tools"][0]["cache_control"].as_object().unwrap();
        assert!(!cc.contains_key("scope"));
        assert_eq!(cc["ttl"], json!("1h"));
    }

    #[test]
    fn prepare_writes_ttl_to_ephemeral_blocks_unconditionally() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}
                ]}
            ]
        });
        prepare(&mut body, "claude-sonnet-4-6").unwrap();

        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["ttl"],
            json!("1h")
        );
    }
}
