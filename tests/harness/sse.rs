//! Server-Sent Events parser for e2e wire-level assertions.
//!
//! Chats at `/v1/messages`, `/v1/chat/completions`, `/v1/responses`, and
//! `/gemini/.../streamGenerateContent` all reply with `text/event-stream` (or
//! `application/x-ndjson` for Gemini-native), with `data: <json>\n\n` framing.
//! This parser surfaces both the event name (Claude streams emit explicit
//! `event:` lines, see `proxy::format_sse_event`) and the JSON payload.

#![cfg(feature = "e2e")]

use aicore_router::proxy::{LlmFamily, TokenStats, extract_token_stats};
use futures::StreamExt;
use reqwest::Response;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub json: Option<Value>,
}

/// Drain a streaming response and split it into discrete SSE events.
/// Returns even on transport error so callers can inspect what arrived
/// before the failure (useful for debugging).
pub async fn parse_sse(response: Response) -> Vec<SseEvent> {
    let mut buf = Vec::<u8>::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break };
        buf.extend_from_slice(&chunk);
    }
    parse_buffer(&buf)
}

fn parse_buffer(buf: &[u8]) -> Vec<SseEvent> {
    let text = String::from_utf8_lossy(buf);
    let mut events = Vec::new();
    let mut current_event: Option<String> = None;
    let mut current_data: Vec<String> = Vec::new();

    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            // Event boundary
            if !current_data.is_empty() {
                let data = current_data.join("\n");
                if data != "[DONE]" {
                    let json = serde_json::from_str::<Value>(&data).ok();
                    events.push(SseEvent {
                        event: current_event.take(),
                        data,
                        json,
                    });
                } else {
                    current_event = None;
                }
                current_data.clear();
            }
            continue;
        }
        if let Some(name) = line.strip_prefix("event:") {
            current_event = Some(name.trim().to_string());
        } else if let Some(payload) = line.strip_prefix("data:") {
            current_data.push(payload.trim_start().to_string());
        }
    }

    // Flush a final partial event if the stream ended without a trailing
    // blank line.
    if !current_data.is_empty() {
        let data = current_data.join("\n");
        if data != "[DONE]" {
            let json = serde_json::from_str::<Value>(&data).ok();
            events.push(SseEvent {
                event: current_event,
                data,
                json,
            });
        }
    }

    events
}

/// Walk events end-to-back looking for one that carries usage info.
/// Reuses `aicore_router::proxy::extract_token_stats` so the field-name
/// logic lives in exactly one place.
pub fn final_usage(events: &[SseEvent], family: LlmFamily) -> Option<TokenStats> {
    events
        .iter()
        .rev()
        .find_map(|ev| extract_token_stats(&ev.data, &family))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_data_lines() {
        let raw = "data: {\"a\":1}\n\ndata: {\"a\":2}\n\ndata: [DONE]\n\n";
        let events = parse_buffer(raw.as_bytes());
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].json.as_ref().unwrap()["a"], 1);
        assert_eq!(events[1].json.as_ref().unwrap()["a"], 2);
    }

    #[test]
    fn parses_event_lines() {
        let raw = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n";
        let events = parse_buffer(raw.as_bytes());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
    }
}
