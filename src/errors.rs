use thiserror::Error;

#[derive(Debug, Error)]
pub enum DeploymentError {
    #[error("Model '{model}' not found or not resolved. Available models: {available}")]
    ModelNotFound { model: String, available: String },

    #[error("No running deployment found for model '{model}' with AI Core name '{aicore_name}'")]
    NoRunningDeployment { model: String, aicore_name: String },

    #[error("Failed to fetch deployments from AI Core: {source}")]
    FetchFailed {
        #[from]
        source: anyhow::Error,
    },

    #[error("Failed to refresh deployment mappings: {details}")]
    RefreshFailed { details: String },

    #[error("Deployment resolver not initialized")]
    NotInitialized,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Config file not found: {path}. Please create a config file.")]
    FileNotFound { path: String },

    #[error("Failed to read config file '{path}': {source}")]
    ReadFailed {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("Failed to parse config file '{path}': {source}")]
    ParseFailed {
        path: String,
        #[source]
        source: serde_yaml::Error,
    },

    #[error("Invalid config format: {details}")]
    InvalidFormat { details: String },

    #[error("Missing required field: {field}")]
    MissingField { field: String },

    #[error("Invalid model configuration for '{model}': {reason}")]
    InvalidModelConfig { model: String, reason: String },
}

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("Model '{model}' not found in configuration")]
    NotFound { model: String },

    #[error("Model '{model}' configuration is invalid: {reason}")]
    InvalidConfig { model: String, reason: String },

    #[error("Failed to resolve deployment for model '{model}': {reason}")]
    ResolutionFailed { model: String, reason: String },

    #[error("Model '{model}' has no running deployment")]
    NoRunningDeployment { model: String },
}

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("Missing API key in request headers")]
    MissingApiKey,

    #[error("Invalid API key")]
    InvalidApiKey,

    #[error("Bad request: {message}")]
    BadRequest { message: String },

    #[error("Model resolution failed: {source}")]
    ModelResolution {
        #[from]
        source: ModelError,
    },

    #[error("Failed to build request URL: {details}")]
    UrlBuildFailed { details: String },

    #[error("Upstream request failed: {source}")]
    UpstreamFailed {
        #[from]
        source: anyhow::Error,
    },
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("Authentication failed: {details}")]
    AuthenticationFailed { details: String },

    #[error("HTTP request failed: {source}")]
    RequestFailed {
        #[from]
        source: reqwest::Error,
    },

    #[error("API response error {status}: {message}")]
    ApiError { status: u16, message: String },

    #[error("Failed to parse response: {source}")]
    ParseError {
        #[from]
        source: serde_json::Error,
    },
}

// Note: These error types automatically implement Into<anyhow::Error> via thiserror
// so no manual From implementations are needed
