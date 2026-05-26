//! OpenAI Responses API (`/v1/responses`) request shaping for SAP AI Core
//! (Azure OpenAI deployments).
//!
//! Background: Codex CLI v0.130+ injects host-side tools (`custom` / `web_search`
//! / `tool_search` / `local_shell` / `image_generation` / `mcp`) into the `tools`
//! array on every request. AI Core's Azure deployments for gpt-5.x reject any
//! tool entry whose `type` is not `function`:
//!   HTTP 400: "The following tools are not allowed for model 'gpt-5.5':
//!              custom, tool_search and web_search."
//!
//! Codex offers no flag to suppress these. acr filters them here so the
//! transparent-proxy contract still holds for the user — the request would
//! otherwise unconditionally 400.
//!
//! Source-of-truth references:
//! * Responses API: <https://platform.openai.com/docs/api-reference/responses>
//! * Azure Responses API support matrix:
//!   <https://learn.microsoft.com/azure/ai-services/openai/how-to/responses>

use anyhow::Result;
use serde_json::{Map, Value};

/// Tool entry `type` values AI Core / Azure Responses API accepts.
///
/// **Last verified against gpt-5.5 on AI Core: 2026-05-26.** Direct probing
/// showed the upstream is itself an allowlist — every other type tested
/// (`custom`, `web_search`, `web_search_preview`, `tool_search`, `local_shell`,
/// `image_generation`, `mcp`, `code_interpreter`, `file_search`, plus a
/// fabricated `some_brand_new_tool_2027`) returned HTTP 400 with
/// `"The following tool is not allowed for model 'gpt-5.5': <type>."` Mirroring
/// upstream's allowlist here keeps Codex working without leaking 400s back to
/// the client.
///
/// **Re-probe periodically.** If AI Core later supports `web_search` /
/// `image_generation` / etc. natively, append the new type here. A quick way
/// to re-verify: `curl -X POST $ACR/v1/responses` with a tool entry of each
/// type and check whether the 400 message changes.
const ALLOWED_TOOL_TYPES: &[&str] = &["function"];

/// Prepare an OpenAI Responses-API request body.
///
/// 1. Filter `tools[]` to [`ALLOWED_TOOL_TYPES`] (drops Codex CLI's `custom`
///    / `web_search` / `tool_search` and other host-side tools that AI Core
///    would 400 on).
/// 2. Reset `tool_choice` to `"auto"` if it referenced a now-dropped tool —
///    Azure 400s when `tool_choice` points at a tool that's no longer present.
pub fn prepare(body: &mut Value) -> Result<()> {
    let Some(obj) = body.as_object_mut() else {
        return Ok(());
    };

    let kept_function_names = filter_tools(obj);
    fixup_tool_choice(obj, &kept_function_names);

    Ok(())
}

/// Drop tool entries whose `type` is not in [`ALLOWED_TOOL_TYPES`]. Returns
/// the surviving `function` tool names so [`fixup_tool_choice`] can validate
/// `tool_choice` references against them.
fn filter_tools(obj: &mut Map<String, Value>) -> Vec<String> {
    let Some(Value::Array(tools)) = obj.get_mut("tools") else {
        return Vec::new();
    };

    let original_len = tools.len();
    let mut kept_names = Vec::new();

    tools.retain(|t| {
        let ty = t.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let keep = ALLOWED_TOOL_TYPES.contains(&ty);
        if keep {
            if let Some(name) = t.get("name").and_then(|v| v.as_str()) {
                kept_names.push(name.to_string());
            }
        } else {
            tracing::debug!(
                "Dropping unsupported Responses-API tool entry type='{}'",
                ty
            );
        }
        keep
    });

    let dropped = original_len - tools.len();
    if dropped > 0 {
        tracing::debug!(
            "Filtered {} unsupported tool entries (kept {} function tools)",
            dropped,
            tools.len()
        );
    }

    // Empty `tools: []` is accepted by AI Core gpt-5.5 (verified 2026-05-26),
    // so leave it in place — removing the key is an extra modification with
    // no observed benefit. Re-probe if a future model rejects empty arrays.

    kept_names
}

/// Reset `tool_choice` to `"auto"` if it references a dropped tool.
///
/// Cases:
/// * String forms (`"auto"` / `"none"` / `"required"`) — left untouched.
/// * `{"type":"function","name":N}` where N survived — left untouched.
/// * `{"type":<not in allowlist>}` (e.g. `"custom"`, `"web_search"`) — reset.
/// * `{"type":"function","name":N}` where N was filtered out — reset.
fn fixup_tool_choice(obj: &mut Map<String, Value>, kept_function_names: &[String]) {
    let Some(tc) = obj.get("tool_choice") else {
        return;
    };
    let needs_reset = match tc {
        Value::String(_) => false,
        Value::Object(o) => {
            let ty = o.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if !ALLOWED_TOOL_TYPES.contains(&ty) {
                true
            } else {
                let name = o.get("name").and_then(|v| v.as_str()).unwrap_or("");
                !kept_function_names.iter().any(|n| n == name)
            }
        }
        _ => true,
    };

    if needs_reset {
        tracing::debug!("Resetting tool_choice to \"auto\" because it referenced a dropped tool");
        obj.insert("tool_choice".to_string(), Value::String("auto".to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn drops_custom_tool_entry() {
        let mut body = json!({
            "tools": [
                {"type": "custom", "name": "apply_patch", "format": {"type": "grammar"}},
                {"type": "function", "name": "shell", "parameters": {}}
            ]
        });
        prepare(&mut body).unwrap();
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], json!("shell"));
    }

    #[test]
    fn drops_web_search_and_tool_search() {
        let mut body = json!({
            "tools": [
                {"type": "web_search"},
                {"type": "tool_search"},
                {"type": "function", "name": "f"}
            ]
        });
        prepare(&mut body).unwrap();
        assert_eq!(body["tools"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn empty_after_filter_leaves_empty_array() {
        // AI Core gpt-5.5 accepts `tools: []` (verified 2026-05-26), so we
        // intentionally do NOT remove the key — fewer modifications.
        let mut body = json!({"tools": [{"type": "custom", "name": "x"}]});
        prepare(&mut body).unwrap();
        assert_eq!(body["tools"], json!([]));
    }

    #[test]
    fn no_tools_field_is_noop() {
        let mut body = json!({"input": "hi", "model": "gpt-5.5"});
        let original = body.clone();
        prepare(&mut body).unwrap();
        assert_eq!(body, original);
    }

    #[test]
    fn tool_choice_for_dropped_tool_reset_to_auto() {
        let mut body = json!({
            "tools": [{"type": "function", "name": "shell"}],
            "tool_choice": {"type": "custom", "name": "apply_patch"}
        });
        prepare(&mut body).unwrap();
        assert_eq!(body["tool_choice"], json!("auto"));
    }

    #[test]
    fn tool_choice_string_left_alone() {
        let mut body = json!({
            "tools": [{"type": "function", "name": "shell"}],
            "tool_choice": "auto"
        });
        prepare(&mut body).unwrap();
        assert_eq!(body["tool_choice"], json!("auto"));
    }

    #[test]
    fn tool_choice_function_with_kept_name_left_alone() {
        let mut body = json!({
            "tools": [{"type": "function", "name": "shell"}],
            "tool_choice": {"type": "function", "name": "shell"}
        });
        prepare(&mut body).unwrap();
        assert_eq!(
            body["tool_choice"],
            json!({"type": "function", "name": "shell"})
        );
    }

    #[test]
    fn tool_choice_function_with_dropped_name_reset() {
        let mut body = json!({
            "tools": [{"type": "function", "name": "shell"}],
            "tool_choice": {"type": "function", "name": "missing_tool"}
        });
        prepare(&mut body).unwrap();
        assert_eq!(body["tool_choice"], json!("auto"));
    }

    #[test]
    fn full_codex_shape_smoke_test() {
        let mut body = json!({
            "model": "gpt-5.5",
            "input": [{"role": "user", "content": "say hi"}],
            "tools": [
                {"type": "custom", "name": "apply_patch", "format": {"type": "grammar"}},
                {"type": "web_search"},
                {"type": "tool_search"},
                {"type": "function", "name": "shell", "parameters": {"type": "object"}},
                {"type": "function", "name": "unified_exec", "parameters": {"type": "object"}}
            ],
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "store": false,
            "stream": true
        });
        prepare(&mut body).unwrap();
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert!(tools.iter().all(|t| t["type"] == json!("function")));
        assert_eq!(body["model"], json!("gpt-5.5"));
        assert_eq!(body["parallel_tool_calls"], json!(true));
        assert_eq!(body["store"], json!(false));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["tool_choice"], json!("auto"));
    }
}
