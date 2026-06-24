//! Domain constants for the AI Core Router

pub mod deployment {
    pub const RUNNING_STATUS: &str = "RUNNING";
}

pub mod models {
    pub const CLAUDE_PREFIX: &str = "claude";
    pub const GEMINI_PREFIX: &str = "gemini";
    pub const GPT_PREFIX: &str = "gpt";
    pub const TEXT_PREFIX: &str = "text";

    /// Resolved client-facing name for Claude Opus 4.7. Used by
    /// `transforms::anthropic::requires_adaptive_thinking` to gate request-shape
    /// overrides specific to adaptive-thinking models (sampling-param strip,
    /// `thinking: enabled` → `adaptive` conversion).
    pub const CLAUDE_OPUS_4_7: &str = "claude-opus-4-7";

    /// Resolved client-facing name for Claude Opus 4.8. Same overrides as 4.7 —
    /// Bedrock rejects `thinking.type.enabled` and non-1 sampling params on this
    /// model. See `transforms::anthropic::requires_adaptive_thinking`.
    pub const CLAUDE_OPUS_4_8: &str = "claude-opus-4-8";
}

pub mod api {
    pub const ANTHROPIC_VERSION: &str = "bedrock-2023-05-31";
    pub const STREAM_DATA_PREFIX: &str = "data: ";
    pub const DEFAULT_API_VERSION: &str = "2025-04-01-preview";

    // API endpoints
    pub const INVOKE_ACTION: &str = "invoke";
    pub const INVOKE_STREAM_ACTION: &str = "invoke-with-response-stream";
    pub const GENERATE_CONTENT_ACTION: &str = "generateContent";
    pub const STREAM_GENERATE_CONTENT_ACTION: &str = "streamGenerateContent";

    // API paths
    pub const INFERENCE_DEPLOYMENTS_PATH: &str = "/v2/inference/deployments";
    pub const EMBEDDINGS_PATH: &str = "/embeddings";
    pub const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
    pub const RESPONSES_PATH: &str = "/responses";
    pub const RESPONSES_COMPACT_PATH: &str = "/responses/compact";
    pub const MODELS_PATH: &str = "/models";

    // AI-Client-Type header
    pub const AI_CLIENT_TYPE_HEADER: &str = "ai-client-type";
    pub const AI_CLIENT_TYPE_VALUE: &str = "aicore-router";

    // Anthropic-Beta header and Anthropic→Bedrock beta-name remap
    pub const ANTHROPIC_BETA_HEADER: &str = "anthropic-beta";

    /// Anthropic→Bedrock beta-name policy table.
    ///
    /// * `(name, Some("other"))` — rename to a Bedrock-equivalent flag.
    /// * `(name, None)` — drop. Bedrock rejects with `"invalid beta flag"`.
    /// * Not in table — passed through (Bedrock decides; lets new Anthropic
    ///   betas work without an acr release).
    ///
    /// **TODO (revisit on every edit):** Bedrock's accepted-beta surface drifts.
    /// Re-probe every `None` entry — any that now return 200 should move to
    /// passthrough (delete the row).
    pub const ANTHROPIC_TO_BEDROCK_BETA_REMAP: &[(&str, Option<&str>)] = &[
        // Renames.
        // Same capability, different name on Bedrock.
        (
            "advanced-tool-use-2025-11-20",
            Some("tool-search-tool-2025-10-19"),
        ),
        // GA on Bedrock via cache_control body fields; the opt-in flag itself
        // is no longer accepted.
        ("prompt-caching-2024-07-31", None),
        // Claude Code 2.1.x sends this by default. acr already strips the
        // matching `cache_control.scope` body field (see
        // `transforms::anthropic::strip_cache_control_scope`).
        ("prompt-caching-scope-2026-01-05", None),
        // `inject_cache_ttl` already writes `ttl: "1h"` and Bedrock honors it
        // (responses report `ephemeral_1h_input_tokens`).
        ("extended-cache-ttl-2025-04-11", None),
        // Anthropic-hosted features with no Bedrock equivalent.
        ("redact-thinking-2026-02-12", None),
        ("mcp-servers-2025-12-04", None),
        ("skills-2025-10-02", None),
        // Belong to surfaces acr does not expose (Files API, Message Batches,
        // Managed Agents — see `routes.rs`).
        ("files-api-2025-04-14", None),
        ("message-batches-2024-09-24", None),
        ("managed-agents-2026-04-01", None),
        // Anthropic-hosted features added in Claude Code 2.1.x; Bedrock
        // rejects each with `400 invalid beta flag`. Empirically probed
        // 2026-06-22 against AWS Bedrock Anthropic. Claude Code 2.1.185
        // sends `thinking-token-count-2026-05-13` by default; without this
        // entry, the request 400s and the client surfaces
        // "API Error: 400 invalid beta flag".
        ("code-execution-2025-05-22", None),
        ("fast-mode-2026-02-01", None),
        ("output-300k-2026-03-24", None),
        ("user-profiles-2026-03-24", None),
        ("advisor-tool-2026-03-01", None),
        ("cache-diagnosis-2026-04-07", None),
        ("thinking-token-count-2026-05-13", None),
        ("server-side-fallback-2026-06-01", None),
        ("fallback-credit-2026-06-01", None),
    ];

    /// Beta feature name for 1M extended context window
    pub const CONTEXT_1M_BETA: &str = "context-1m-2025-08-07";

    /// Model name suffix that triggers auto-injection of the 1M context beta feature
    pub const EXTENDED_CONTEXT_SUFFIX: &str = "[1m]";

    // Claude thinking budget constraints (matching Bedrock requirements)
    pub const MIN_BUDGET_TOKENS_FOR_THINKING: u64 = 1024;
    pub const BUDGET_RESERVE_MARGIN: u64 = 1;

    // Default max_tokens for Anthropic if not provided (Bedrock requires this field)
    pub const ANTHROPIC_DEFAULT_MAX_TOKENS: u64 = 4096;

    // Peek window for classifying upstream's first SSE chunks before
    // committing to forwarding the response to the client. If the first
    // parseable `data:` line indicates a rate-limit / throttling failure,
    // acr fails over to the next provider instead of streaming the error
    // back. 10s is generous: every supported family emits its first frame
    // (`response.created` / `message_start` / chat-completion role chunk /
    // first Gemini candidate) well under 1s in practice.
    pub const STREAM_PEEK_TIMEOUT_SECS: u64 = 10;
}

/// Per-model context-window capabilities. `max` is the largest input-token count
/// the model can accept; `beta` is the `Anthropic-Beta` header value (if any) that
/// must be set on the request to unlock that maximum. `beta: None` means `max` is
/// the native default — no header required.
struct ContextCaps {
    max: u64,
    beta: Option<&'static str>,
}

/// Prefix-matched table of model context capabilities.
/// Entries ordered most-specific-first so longer prefixes win.
static MODEL_CONTEXT_CAPS: &[(&str, ContextCaps)] = &[
    // --- Anthropic Claude (via AWS Bedrock) ---
    // Native 1M context (no beta needed):
    (
        "claude-opus-4-8",
        ContextCaps {
            max: 1_000_000,
            beta: None,
        },
    ),
    (
        "claude-opus-4-7",
        ContextCaps {
            max: 1_000_000,
            beta: None,
        },
    ),
    (
        "claude-opus-4-6",
        ContextCaps {
            max: 1_000_000,
            beta: None,
        },
    ),
    (
        "claude-sonnet-4-6",
        ContextCaps {
            max: 1_000_000,
            beta: None,
        },
    ),
    // 1M via context-1m-2025-08-07 beta (200k native, beta unlocks 1M):
    (
        "claude-sonnet-4-5",
        ContextCaps {
            max: 1_000_000,
            beta: Some(api::CONTEXT_1M_BETA),
        },
    ),
    (
        "claude-sonnet-4",
        ContextCaps {
            max: 1_000_000,
            beta: Some(api::CONTEXT_1M_BETA),
        },
    ),
    // 200k models (no extended-context beta available):
    (
        "claude-opus-4-5",
        ContextCaps {
            max: 200_000,
            beta: None,
        },
    ),
    (
        "claude-opus-4-1",
        ContextCaps {
            max: 200_000,
            beta: None,
        },
    ),
    (
        "claude-opus-4",
        ContextCaps {
            max: 200_000,
            beta: None,
        },
    ), // catch-all for older Opus 4 variants
    (
        "claude-haiku-4",
        ContextCaps {
            max: 200_000,
            beta: None,
        },
    ), // includes claude-haiku-4-5
    (
        "claude-3-haiku",
        ContextCaps {
            max: 200_000,
            beta: None,
        },
    ),
    // --- OpenAI (via Azure) ---
    // GPT-5.5 / GPT-5.5-pro / GPT-5.4: 1.05M context
    (
        "gpt-5.5",
        ContextCaps {
            max: 1_050_000,
            beta: None,
        },
    ),
    (
        "gpt-5.4-mini",
        ContextCaps {
            max: 400_000,
            beta: None,
        },
    ),
    (
        "gpt-5.4-nano",
        ContextCaps {
            max: 400_000,
            beta: None,
        },
    ),
    (
        "gpt-5.4",
        ContextCaps {
            max: 1_050_000,
            beta: None,
        },
    ),
    // GPT-5 through GPT-5.3: 400k context
    (
        "gpt-5",
        ContextCaps {
            max: 400_000,
            beta: None,
        },
    ),
    // GPT-4.1 family (including mini/nano): ~1M (1,047,576)
    (
        "gpt-4.1",
        ContextCaps {
            max: 1_047_576,
            beta: None,
        },
    ),
    // GPT-4o / GPT-4o-mini: 128k
    (
        "gpt-4o",
        ContextCaps {
            max: 128_000,
            beta: None,
        },
    ),
    // OpenAI o-series reasoning: all 200k
    (
        "o4-mini",
        ContextCaps {
            max: 200_000,
            beta: None,
        },
    ),
    (
        "o3-mini",
        ContextCaps {
            max: 200_000,
            beta: None,
        },
    ),
    (
        "o3",
        ContextCaps {
            max: 200_000,
            beta: None,
        },
    ),
    (
        "o1",
        ContextCaps {
            max: 200_000,
            beta: None,
        },
    ),
    // --- Google Gemini (via GCP Vertex AI) ---
    // All Gemini 2.0+ models: 1M context
    (
        "gemini-3",
        ContextCaps {
            max: 1_048_576,
            beta: None,
        },
    ),
    (
        "gemini-2.5",
        ContextCaps {
            max: 1_048_576,
            beta: None,
        },
    ),
    (
        "gemini-2.0",
        ContextCaps {
            max: 1_048_576,
            beta: None,
        },
    ),
    // --- Embedding models ---
    (
        "text-embedding-3-large",
        ContextCaps {
            max: 8_192,
            beta: None,
        },
    ),
    (
        "text-embedding-3-small",
        ContextCaps {
            max: 8_192,
            beta: None,
        },
    ),
    (
        "text-embedding",
        ContextCaps {
            max: 8_192,
            beta: None,
        },
    ),
];

fn get_context_caps(model: &str) -> Option<&'static ContextCaps> {
    for (prefix, caps) in MODEL_CONTEXT_CAPS {
        if model.starts_with(prefix) {
            return Some(caps);
        }
    }
    None
}

/// Returns the maximum context window (in input tokens) the model can accept.
/// Includes capacity unlocked by an extended-context beta — see
/// [`get_extended_context_beta`] for whether a header is required to actually
/// reach the returned value. Returns `None` for unrecognized models.
pub fn get_context_length(model: &str) -> Option<u64> {
    get_context_caps(model).map(|c| c.max)
}

/// Returns the `Anthropic-Beta` header value required to unlock this model's
/// maximum context window, or `None` when the maximum is native (no beta needed)
/// or the model is unrecognized.
pub fn get_extended_context_beta(model: &str) -> Option<&'static str> {
    get_context_caps(model).and_then(|c| c.beta)
}

pub mod config {
    pub const DEFAULT_BIND: &str = "127.0.0.1:8900";
    pub const DEFAULT_LOG_LEVEL: &str = "info";
    pub const DEFAULT_RESOURCE_GROUP: &str = "default";
    pub const DEFAULT_REFRESH_INTERVAL_SECS: u64 = 300; // 5 minutes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_length_returns_max_for_known_models() {
        assert_eq!(get_context_length("claude-opus-4-8"), Some(1_000_000));
        assert_eq!(get_context_length("claude-opus-4-7"), Some(1_000_000));
        assert_eq!(get_context_length("claude-sonnet-4-6"), Some(1_000_000));
        assert_eq!(get_context_length("claude-sonnet-4-5"), Some(1_000_000));
        assert_eq!(get_context_length("claude-haiku-4-5"), Some(200_000));
        assert_eq!(get_context_length("gpt-4o"), Some(128_000));
        assert_eq!(get_context_length("gemini-2.5-pro"), Some(1_048_576));
        assert_eq!(get_context_length("nova-lite"), None);
    }

    #[test]
    fn context_length_uses_prefix_match_for_versioned_names() {
        // Versioned/dated model names match the most specific prefix.
        assert_eq!(
            get_context_length("claude-sonnet-4-6-20260101"),
            Some(1_000_000)
        );
        assert_eq!(
            get_context_length("claude-haiku-4-5-20260101"),
            Some(200_000)
        );
    }

    #[test]
    fn extended_context_beta_returns_beta_only_for_models_that_need_it() {
        // Models reaching 1M via the beta:
        assert_eq!(
            get_extended_context_beta("claude-sonnet-4-5"),
            Some(api::CONTEXT_1M_BETA)
        );
        assert_eq!(
            get_extended_context_beta("claude-sonnet-4"),
            Some(api::CONTEXT_1M_BETA)
        );
        // Native-1M models — no beta needed:
        assert_eq!(get_extended_context_beta("claude-sonnet-4-6"), None);
        assert_eq!(get_extended_context_beta("claude-opus-4-8"), None);
        assert_eq!(get_extended_context_beta("claude-opus-4-7"), None);
        assert_eq!(get_extended_context_beta("claude-opus-4-6"), None);
        // 200k models — no extended-context beta available:
        assert_eq!(get_extended_context_beta("claude-haiku-4-5"), None);
        assert_eq!(get_extended_context_beta("claude-opus-4-5"), None);
        // Non-Claude families:
        assert_eq!(get_extended_context_beta("gpt-5.4"), None);
        assert_eq!(get_extended_context_beta("gemini-2.5-pro"), None);
        // Unknown model:
        assert_eq!(get_extended_context_beta("nova-lite"), None);
    }
}
