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
use std::collections::HashSet;

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

    bridge_tool_search_request(obj);
    let kept_function_names = filter_tools(obj);
    fixup_tool_choice(obj, &kept_function_names);

    Ok(())
}

/// Convert Codex's client-executed `tool_search` tool into an upstream-safe
/// Responses `function` tool. AI Core accepts function tools but ignores or
/// rejects non-function hosted/client tool types. The downstream response path
/// converts function calls named `tool_search` back into Codex's
/// `tool_search_call` response item shape.
fn bridge_tool_search_request(obj: &mut Map<String, Value>) {
    let Some(Value::Array(tools)) = obj.get_mut("tools") else {
        return;
    };

    for tool in tools.iter_mut() {
        let Some(tool_obj) = tool.as_object_mut() else {
            continue;
        };
        if tool_obj.get("type").and_then(|v| v.as_str()) != Some("tool_search") {
            continue;
        }

        tool_obj.insert("type".to_string(), Value::String("function".to_string()));
        tool_obj.insert("name".to_string(), Value::String("tool_search".to_string()));
        tool_obj
            .entry("description".to_string())
            .or_insert_with(|| {
                Value::String(
                    "Search deferred Codex tools and expose matches to the next request."
                        .to_string(),
                )
            });
        tool_obj.insert(
            "parameters".to_string(),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "number" }
                },
                "required": ["query", "limit"],
                "additionalProperties": false
            }),
        );
        tool_obj.insert("strict".to_string(), Value::Bool(true));
        tool_obj.remove("execution");
    }
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
    let mut dropped_types: Vec<String> = Vec::new();

    tools.retain(|t| {
        let ty = t.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let keep = ALLOWED_TOOL_TYPES.contains(&ty);
        if keep {
            if let Some(name) = t.get("name").and_then(|v| v.as_str()) {
                kept_names.push(name.to_string());
            }
        } else {
            dropped_types.push(ty.to_string());
        }
        keep
    });

    let dropped = original_len - tools.len();
    if dropped > 0 {
        tracing::debug!(
            "Filtered {} unsupported Responses-API tool entries (types={:?}, kept {} function tools)",
            dropped,
            dropped_types,
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

#[derive(Default)]
pub struct ToolSearchStreamBridge {
    item_ids: HashSet<String>,
}

impl ToolSearchStreamBridge {
    /// Rewrite a streaming SSE `data:` payload for the tool_search bridge.
    ///
    /// Returns `Some(payload)` to forward and `None` to drop the line. To keep
    /// acr a transparent proxy, any payload that does not carry a `tool_search`
    /// item is forwarded **verbatim** — non-JSON lines (e.g. a `[DONE]`
    /// sentinel) and ordinary events are never round-tripped through
    /// serde_json, so their exact bytes and key order survive untouched.
    pub fn rewrite_sse_data(&mut self, data: &str) -> Option<String> {
        let Ok(mut event) = serde_json::from_str::<Value>(data) else {
            return Some(data.to_string());
        };
        let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match event_type {
            "response.output_item.added" | "response.output_item.done" => {
                let Some(item) = event.get_mut("item") else {
                    return Some(data.to_string());
                };
                let original_id = item.get("id").and_then(|v| v.as_str()).map(str::to_string);
                if !rewrite_tool_search_item(item) {
                    return Some(data.to_string());
                }
                if let Some(id) = original_id {
                    self.item_ids.insert(id);
                }
            }
            "response.function_call_arguments.delta" | "response.function_call_arguments.done" => {
                if event
                    .get("item_id")
                    .and_then(|v| v.as_str())
                    .is_some_and(|id| self.item_ids.contains(id))
                {
                    return None;
                }
                return Some(data.to_string());
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                let changed = event
                    .get_mut("response")
                    .and_then(|r| r.get_mut("output"))
                    .and_then(|o| o.as_array_mut())
                    .map(|output| {
                        output
                            .iter_mut()
                            .fold(false, |acc, item| acc | rewrite_tool_search_item(item))
                    })
                    .unwrap_or(false);
                if !changed {
                    return Some(data.to_string());
                }
            }
            _ => return Some(data.to_string()),
        }

        serde_json::to_string(&event).ok()
    }
}

pub fn rewrite_response_body(body: &[u8]) -> Option<Vec<u8>> {
    let mut value: Value = serde_json::from_slice(body).ok()?;
    let output = value.get_mut("output")?.as_array_mut()?;
    let mut changed = false;
    for item in output {
        changed |= rewrite_tool_search_item(item);
    }
    changed.then(|| serde_json::to_vec(&value).ok()).flatten()
}

fn rewrite_tool_search_item(item: &mut Value) -> bool {
    let Some(obj) = item.as_object_mut() else {
        return false;
    };
    if obj.get("type").and_then(|v| v.as_str()) != Some("function_call")
        || obj.get("name").and_then(|v| v.as_str()) != Some("tool_search")
    {
        return false;
    }

    let arguments = obj
        .get("arguments")
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    obj.insert(
        "type".to_string(),
        Value::String("tool_search_call".to_string()),
    );
    obj.insert("execution".to_string(), Value::String("client".to_string()));
    obj.insert("arguments".to_string(), arguments);
    if let Some(id) = obj.get("id").and_then(|v| v.as_str())
        && !id.starts_with("tsc_")
    {
        obj.insert("id".to_string(), Value::String(format!("tsc_{id}")));
    }
    obj.remove("name");
    true
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
    fn drops_web_search_and_bridges_tool_search() {
        let mut body = json!({
            "tools": [
                {"type": "web_search"},
                {"type": "tool_search"},
                {"type": "function", "name": "f"}
            ]
        });
        prepare(&mut body).unwrap();
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert!(tools.iter().any(|t| t["name"] == json!("tool_search")));
        assert!(tools.iter().all(|t| t["type"] == json!("function")));
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
        assert_eq!(tools.len(), 3);
        assert!(tools.iter().all(|t| t["type"] == json!("function")));
        assert!(tools.iter().any(|t| t["name"] == json!("tool_search")));
        assert_eq!(body["model"], json!("gpt-5.5"));
        assert_eq!(body["parallel_tool_calls"], json!(true));
        assert_eq!(body["store"], json!(false));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["tool_choice"], json!("auto"));
    }

    #[test]
    fn bridges_tool_search_request_to_function_tool() {
        let mut body = json!({
            "tools": [{
                "type": "tool_search",
                "execution": "client",
                "description": "search deferred tools",
                "parameters": {
                    "type": "object",
                    "properties": { "query": { "type": "string" } },
                    "required": ["query"],
                    "additionalProperties": false
                }
            }]
        });
        prepare(&mut body).unwrap();
        let tool = &body["tools"][0];
        assert_eq!(tool["type"], json!("function"));
        assert_eq!(tool["name"], json!("tool_search"));
        assert_eq!(tool["strict"], json!(true));
        assert!(tool.get("execution").is_none());
    }

    #[test]
    fn rewrites_non_streaming_tool_search_function_call() {
        let body = json!({
            "output": [{
                "type": "function_call",
                "name": "tool_search",
                "call_id": "call_1",
                "arguments": "{\"query\":\"node_repl\",\"limit\":8}"
            }]
        });
        let rewritten = rewrite_response_body(body.to_string().as_bytes()).unwrap();
        let value: Value = serde_json::from_slice(&rewritten).unwrap();
        let item = &value["output"][0];
        assert_eq!(item["type"], json!("tool_search_call"));
        assert_eq!(item["execution"], json!("client"));
        assert_eq!(item["arguments"], json!({"query":"node_repl","limit":8}));
        assert!(item.get("name").is_none());
    }

    #[test]
    fn rewrites_stream_tool_search_call_and_suppresses_argument_deltas() {
        let mut bridge = ToolSearchStreamBridge::default();
        let added = json!({
            "type": "response.output_item.added",
            "item": {
                "id": "fc_1",
                "type": "function_call",
                "name": "tool_search",
                "call_id": "call_1",
                "arguments": ""
            }
        });
        let rewritten = bridge.rewrite_sse_data(&added.to_string()).unwrap();
        let value: Value = serde_json::from_str(&rewritten).unwrap();
        assert_eq!(value["item"]["type"], json!("tool_search_call"));
        assert_eq!(value["item"]["execution"], json!("client"));

        let delta = json!({
            "type": "response.function_call_arguments.delta",
            "item_id": "fc_1",
            "delta": "{\"query\":"
        });
        assert!(bridge.rewrite_sse_data(&delta.to_string()).is_none());
    }

    #[test]
    fn passes_through_non_tool_search_stream_events_verbatim() {
        let mut bridge = ToolSearchStreamBridge::default();
        // Keys are deliberately NOT in alphabetical order. A transparent proxy
        // must forward the exact bytes, not round-trip them through serde_json
        // (which, with no preserve_order feature, re-sorts object keys).
        let added = r#"{"type":"response.output_item.added","item":{"id":"fc_1","type":"function_call","name":"shell","arguments":""}}"#;
        assert_eq!(bridge.rewrite_sse_data(added), Some(added.to_string()));

        // A function-call delta for an item the bridge never rewrote must also
        // pass through untouched (item_id is not a registered tool_search id).
        let delta =
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"abc"}"#;
        assert_eq!(bridge.rewrite_sse_data(delta), Some(delta.to_string()));

        // A terminal event with no tool_search items in its output is verbatim.
        let completed = r#"{"type":"response.completed","response":{"id":"resp_1","output":[{"id":"fc_1","type":"function_call","name":"shell","arguments":"{}"}]}}"#;
        assert_eq!(
            bridge.rewrite_sse_data(completed),
            Some(completed.to_string())
        );
    }

    #[test]
    fn passes_through_non_json_data_lines_verbatim() {
        let mut bridge = ToolSearchStreamBridge::default();
        // An opaque / non-JSON data line (e.g. a `[DONE]` sentinel) must be
        // forwarded, not swallowed by the JSON gate.
        assert_eq!(
            bridge.rewrite_sse_data("[DONE]"),
            Some("[DONE]".to_string())
        );
    }
}
