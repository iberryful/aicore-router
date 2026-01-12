/// Domain constants for the AI Core Router
pub mod deployment {
    pub const RUNNING_STATUS: &str = "RUNNING";
    pub const STOPPED_STATUS: &str = "STOPPED";
    pub const UNKNOWN_STATUS: &str = "UNKNOWN";
}

pub mod models {
    pub const CLAUDE_PREFIX: &str = "claude";
    pub const GEMINI_PREFIX: &str = "gemini";
    pub const GPT_PREFIX: &str = "gpt";
    pub const TEXT_PREFIX: &str = "text";
}

pub mod http {
    pub const DEFAULT_API_VERSION: &str = "2025-04-01-preview";
    pub const BEARER_PREFIX: &str = "Bearer ";
    pub const AUTHORIZATION_HEADER: &str = "authorization";
    pub const AI_RESOURCE_GROUP_HEADER: &str = "ai-resource-group";
    pub const CONTENT_TYPE_HEADER: &str = "content-type";
    pub const APPLICATION_JSON: &str = "application/json";
    pub const TEXT_EVENT_STREAM: &str = "text/event-stream";
}

pub mod api {
    pub const ANTHROPIC_VERSION: &str = "bedrock-2023-05-31";
    pub const STREAM_DATA_PREFIX: &str = "data: ";
    pub const STREAM_EVENT_PREFIX: &str = "event: ";
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
}

pub mod config {
    pub const DEFAULT_PORT: u16 = 8900;
    pub const DEFAULT_LOG_LEVEL: &str = "info";
    pub const DEFAULT_RESOURCE_GROUP: &str = "default";
    pub const DEFAULT_REFRESH_INTERVAL_SECS: u64 = 300; // 5 minutes
}
