//! Google Gemini (via SAP AI Core / Vertex) request shaping.
//!
//! Source-of-truth references:
//! * Generate Content API (request shape, `functionResponse`, `generationConfig`):
//!   <https://ai.google.dev/api/generate-content>

use anyhow::Result;
use serde_json::{Map, Value, json};

/// Prepare a Gemini request body for AI Core.
///
/// Drops fields the upstream wrapper doesn't expect (`model`, `stream`),
/// strips IDs from `functionResponse` parts (AI Core rejects them), and
/// rewrites `thinkingBudget: 0` → `-1` so a "let the model decide" intent
/// isn't read by Google's API as "thinking disabled".
pub fn prepare(body: &mut Value) -> Result<()> {
    let Some(obj) = body.as_object_mut() else {
        return Ok(());
    };

    obj.remove("model");
    obj.remove("stream");

    strip_function_response_ids(obj);
    fix_thinking_budget(obj);

    Ok(())
}

/// Strip `id` from every `functionResponse` part (AI Core wrapper rejects it).
fn strip_function_response_ids(obj: &mut Map<String, Value>) {
    if let Some(Value::Array(contents)) = obj.get_mut("contents") {
        for content in contents.iter_mut() {
            if let Some(Value::Array(parts)) = content.get_mut("parts") {
                for part in parts.iter_mut() {
                    if let Some(func_response) = part.get_mut("functionResponse")
                        && let Some(fr_obj) = func_response.as_object_mut()
                    {
                        fr_obj.remove("id");
                    }
                }
            }
        }
    }
}

/// Convert `thinkingConfig.thinkingBudget: 0` → `-1`. Some clients send `0` to mean
/// "dynamic / model decides", but Google's API treats `0` as "thinking disabled".
/// The `-1` value is the documented dynamic-budget sentinel.
fn fix_thinking_budget(obj: &mut Map<String, Value>) {
    if let Some(config) = obj.get_mut("generationConfig")
        && let Some(thinking_config) = config.get_mut("thinkingConfig")
        && let Some(budget) = thinking_config.get("thinkingBudget")
        && budget.as_i64() == Some(0)
    {
        tracing::debug!("Gemini thinking budget 0 changed to -1 (dynamic)");
        if let Some(tc_obj) = thinking_config.as_object_mut() {
            tc_obj.insert("thinkingBudget".to_string(), json!(-1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_drops_model_and_stream() {
        let mut body = json!({
            "model": "gemini-2.5-pro",
            "stream": true,
            "contents": [],
        });
        prepare(&mut body).unwrap();
        let obj = body.as_object().unwrap();
        assert!(!obj.contains_key("model"));
        assert!(!obj.contains_key("stream"));
        assert!(obj.contains_key("contents"));
    }

    #[test]
    fn strip_function_response_ids_removes_id_only() {
        let mut body = json!({
            "contents": [
                {
                    "role": "user",
                    "parts": [
                        {"functionResponse": {"id": "abc", "name": "foo", "response": {"x": 1}}}
                    ]
                }
            ]
        });
        let obj = body.as_object_mut().unwrap();
        strip_function_response_ids(obj);
        let fr = &obj["contents"][0]["parts"][0]["functionResponse"];
        assert!(!fr.as_object().unwrap().contains_key("id"));
        assert_eq!(fr["name"], json!("foo"));
        assert_eq!(fr["response"], json!({"x": 1}));
    }

    #[test]
    fn fix_thinking_budget_zero_becomes_negative_one() {
        let mut body = json!({
            "generationConfig": {
                "thinkingConfig": {"thinkingBudget": 0}
            }
        });
        let obj = body.as_object_mut().unwrap();
        fix_thinking_budget(obj);
        assert_eq!(
            obj["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            json!(-1)
        );
    }

    #[test]
    fn fix_thinking_budget_nonzero_unchanged() {
        let mut body = json!({
            "generationConfig": {
                "thinkingConfig": {"thinkingBudget": 1024}
            }
        });
        let obj = body.as_object_mut().unwrap();
        fix_thinking_budget(obj);
        assert_eq!(
            obj["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            json!(1024)
        );
    }
}
