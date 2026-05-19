//! Domain constants for the AI Core Router

pub mod deployment {
    pub const RUNNING_STATUS: &str = "RUNNING";
}

pub mod models {
    pub const CLAUDE_PREFIX: &str = "claude";
    pub const GEMINI_PREFIX: &str = "gemini";
    pub const GPT_PREFIX: &str = "gpt";
    pub const TEXT_PREFIX: &str = "text";
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
    pub const MODELS_PATH: &str = "/models";

    // AI-Client-Type header
    pub const AI_CLIENT_TYPE_HEADER: &str = "ai-client-type";
    pub const AI_CLIENT_TYPE_VALUE: &str = "aicore-router";

    // Anthropic-Beta header and allowed Bedrock beta features
    pub const ANTHROPIC_BETA_HEADER: &str = "anthropic-beta";

    /// Maps Anthropic beta feature names to Bedrock-supported equivalents.
    /// Features not in this list are silently dropped.
    pub const ALLOWED_BETA_FEATURES: &[(&str, &str)] = &[
        (
            "fine-grained-tool-streaming-2025-05-14",
            "fine-grained-tool-streaming-2025-05-14",
        ),
        (
            "tool-search-tool-2025-10-19",
            "tool-search-tool-2025-10-19",
        ),
        ("tool-examples-2025-10-29", "tool-examples-2025-10-29"),
        (
            "advanced-tool-use-2025-11-20",
            "tool-search-tool-2025-10-19",
        ),
        ("context-1m-2025-08-07", "context-1m-2025-08-07"),
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

    // Streaming response timeout (matches hai-proxy's 5-minute timeout)
    pub const STREAMING_TIMEOUT_SECS: u64 = 300;
}

/// Returns the known context window size (input tokens) for a model, based on its name.
/// Uses prefix matching to handle versioned model names (e.g. "claude-sonnet-4-5-20250929").
/// Returns `None` for unrecognized models.
pub fn get_context_length(model: &str) -> Option<u64> {
    static CONTEXT_LENGTHS: &[(&str, u64)] = &[
        // --- Anthropic Claude (via AWS Bedrock) ---
        // Claude Opus 4.6+ and Sonnet 4.6: native 1M context
        ("claude-opus-4-6", 1_000_000),
        ("claude-opus-4-7", 1_000_000),
        ("claude-sonnet-4-6", 1_000_000),
        // Claude Sonnet 4.5 and older, Haiku: 200k (1M only via [1m] beta)
        ("claude-opus-4", 200_000),
        ("claude-sonnet-4", 200_000),
        ("claude-haiku-4", 200_000),
        ("claude-3-haiku", 200_000),
        // --- OpenAI (via Azure) ---
        // GPT-5.5 / GPT-5.5-pro / GPT-5.4: 1.05M context
        ("gpt-5.5", 1_050_000),
        ("gpt-5.4-mini", 400_000),
        ("gpt-5.4-nano", 400_000),
        ("gpt-5.4", 1_050_000),
        // GPT-5 through GPT-5.3: 400k context
        ("gpt-5", 400_000),
        // GPT-4.1 family (including mini/nano): ~1M (1,047,576)
        ("gpt-4.1", 1_047_576),
        // GPT-4o / GPT-4o-mini: 128k
        ("gpt-4o", 128_000),
        // --- OpenAI o-series reasoning ---
        ("o4-mini", 200_000),
        ("o3-mini", 200_000),
        ("o3", 200_000),
        ("o1", 200_000),
        // --- Google Gemini (via GCP Vertex AI) ---
        // All Gemini 2.0+ models: 1M context
        ("gemini-3", 1_048_576),
        ("gemini-2.5", 1_048_576),
        ("gemini-2.0", 1_048_576),
        // --- Embedding models ---
        ("text-embedding-3-large", 8_192),
        ("text-embedding-3-small", 8_192),
        ("text-embedding", 8_192),
    ];

    // Try prefix matching — entries are ordered so more specific prefixes come first
    for &(prefix, length) in CONTEXT_LENGTHS {
        if model.starts_with(prefix) {
            return Some(length);
        }
    }
    None
}

pub mod config {
    pub const DEFAULT_BIND: &str = "127.0.0.1:8900";
    pub const DEFAULT_LOG_LEVEL: &str = "info";
    pub const DEFAULT_RESOURCE_GROUP: &str = "default";
    pub const DEFAULT_REFRESH_INTERVAL_SECS: u64 = 300; // 5 minutes
}
