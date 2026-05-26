//! OpenAI (via Azure OpenAI on SAP AI Core) request shaping for the **Chat
//! Completions API** (`/chat/completions`) and **embeddings**.
//!
//! The **Responses API** (`/v1/responses`, used by Codex CLI v0.130+) is a
//! different shape and bypasses this module entirely — its request body is
//! forwarded transparently. See `proxy::prepare_body`'s
//! `LlmFamily::OpenAiResponses` arm.
//!
//! Source-of-truth references:
//! * Chat Completions API (`max_completion_tokens`, `stream_options.include_usage`,
//!   tool-call / tool-response message ordering):
//!   <https://platform.openai.com/docs/api-reference/chat/create>
//! * Responses API (handled outside this module):
//!   <https://platform.openai.com/docs/api-reference/responses>
//! * Azure OpenAI deployment URL shape:
//!   <https://learn.microsoft.com/azure/ai-services/openai/reference>

use anyhow::Result;
use serde_json::{Map, Value, json};

/// Prepare an OpenAI request body.
///
/// * Renames legacy `max_tokens` → `max_completion_tokens` (the canonical field since
///   GPT-4o 2024-08-06+; required for o-series and GPT-5 reasoning models).
/// * For streaming requests, sets `stream_options.include_usage = true` so the final
///   chunk carries token counts (merging into any client-provided `stream_options`).
/// * Normalizes the message array against a Codex CLI bug where a preamble assistant
///   message gets inserted between an `assistant(tool_calls)` and its `tool(response)`.
pub fn prepare(body: &mut Value, stream: bool) -> Result<()> {
    let Some(obj) = body.as_object_mut() else {
        return Ok(());
    };

    if obj.contains_key("max_tokens")
        && !obj.contains_key("max_completion_tokens")
        && let Some(max_tokens) = obj.remove("max_tokens")
    {
        obj.insert("max_completion_tokens".to_string(), max_tokens);
    }

    if stream {
        match obj.get_mut("stream_options") {
            Some(existing_options) => {
                if let Some(options_obj) = existing_options.as_object_mut() {
                    options_obj.insert("include_usage".to_string(), json!(true));
                }
            }
            None => {
                obj.insert("stream_options".to_string(), json!({"include_usage": true}));
            }
        }
    }

    normalize_messages(obj);

    Ok(())
}

/// Detect the Codex-CLI preamble pattern:
/// `assistant(tool_calls)` → `assistant(content preamble)` → `tool(response with matching id)`.
fn is_preamble_pattern(msg: &Value, preamble: &Value, tool_msg: &Value) -> bool {
    let is_assistant_with_tool_calls = msg.get("role").and_then(|v| v.as_str())
        == Some("assistant")
        && msg
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty());
    let is_preamble = preamble.get("role").and_then(|v| v.as_str()) == Some("assistant")
        && preamble
            .get("content")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty());
    let is_tool_response = tool_msg.get("role").and_then(|v| v.as_str()) == Some("tool");

    if !(is_assistant_with_tool_calls && is_preamble && is_tool_response) {
        return false;
    }
    let tool_call_id = tool_msg
        .get("tool_call_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    msg.get("tool_calls")
        .and_then(|v| v.as_array())
        .is_some_and(|calls| {
            calls
                .iter()
                .any(|c| c.get("id").and_then(|v| v.as_str()) == Some(tool_call_id))
        })
}

/// Merge the Codex-CLI preamble assistant message into the preceding `assistant(tool_calls)`
/// message and drop the duplicate. Preserves all other messages verbatim.
fn normalize_messages(obj: &mut Map<String, Value>) {
    let Some(Value::Array(messages)) = obj.get("messages") else {
        return;
    };
    if messages.len() < 3 {
        return;
    }

    let needs_normalization = (0..messages.len().saturating_sub(2))
        .any(|i| is_preamble_pattern(&messages[i], &messages[i + 1], &messages[i + 2]));

    if !needs_normalization {
        return;
    }

    let messages = match obj.remove("messages") {
        Some(Value::Array(msgs)) => msgs,
        _ => return,
    };

    let mut normalized: Vec<Value> = Vec::with_capacity(messages.len());
    let mut i = 0;

    while i < messages.len() {
        if i + 2 < messages.len()
            && is_preamble_pattern(&messages[i], &messages[i + 1], &messages[i + 2])
        {
            let preamble_content = messages[i + 1]
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let mut merged = messages[i].clone();

            let existing_content = messages[i]
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new_content = if existing_content.is_empty() {
                preamble_content.to_string()
            } else {
                format!("{}\n\n{}", existing_content, preamble_content)
            };
            if let Some(merged_obj) = merged.as_object_mut() {
                merged_obj.insert("content".to_string(), Value::String(new_content));
            }

            tracing::debug!("Normalized Codex CLI preamble message at index {}", i);
            normalized.push(merged);
            i += 2; // Skip the preamble; loop increment will advance past it
            continue;
        }

        normalized.push(messages[i].clone());
        i += 1;
    }

    obj.insert("messages".to_string(), Value::Array(normalized));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renames_max_tokens_to_max_completion_tokens() {
        let mut body = json!({"max_tokens": 1024, "messages": []});
        prepare(&mut body, false).unwrap();
        let obj = body.as_object().unwrap();
        assert!(!obj.contains_key("max_tokens"));
        assert_eq!(obj["max_completion_tokens"], json!(1024));
    }

    #[test]
    fn keeps_existing_max_completion_tokens() {
        let mut body = json!({
            "max_tokens": 1024,
            "max_completion_tokens": 2048,
            "messages": []
        });
        prepare(&mut body, false).unwrap();
        let obj = body.as_object().unwrap();
        // max_tokens left as-is (don't overwrite the canonical field)
        assert_eq!(obj["max_tokens"], json!(1024));
        assert_eq!(obj["max_completion_tokens"], json!(2048));
    }

    #[test]
    fn streaming_injects_include_usage_when_no_stream_options() {
        let mut body = json!({"messages": []});
        prepare(&mut body, true).unwrap();
        assert_eq!(body["stream_options"], json!({"include_usage": true}));
    }

    #[test]
    fn streaming_merges_into_existing_stream_options() {
        let mut body = json!({
            "messages": [],
            "stream_options": {"some_other": "value"}
        });
        prepare(&mut body, true).unwrap();
        assert_eq!(body["stream_options"]["some_other"], json!("value"));
        assert_eq!(body["stream_options"]["include_usage"], json!(true));
    }

    #[test]
    fn non_streaming_leaves_stream_options_alone() {
        let mut body = json!({"messages": []});
        prepare(&mut body, false).unwrap();
        assert!(!body.as_object().unwrap().contains_key("stream_options"));
    }

    #[test]
    fn normalize_merges_codex_preamble() {
        let mut body = json!({
            "messages": [
                {"role": "assistant", "content": "calling", "tool_calls": [{"id": "t1"}]},
                {"role": "assistant", "content": "preamble"},
                {"role": "tool", "tool_call_id": "t1", "content": "result"}
            ]
        });
        prepare(&mut body, false).unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["content"], json!("calling\n\npreamble"));
        assert_eq!(messages[1]["role"], json!("tool"));
    }

    #[test]
    fn normalize_leaves_unrelated_messages_alone() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello"},
                {"role": "user", "content": "again"}
            ]
        });
        let original = body.clone();
        prepare(&mut body, false).unwrap();
        assert_eq!(body, original);
    }
}
