use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::path::Path;

use crate::constants::config::*;

/// Runtime configuration for the router
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// List of AI Core providers for load balancing
    pub providers: Vec<Provider>,
    /// API keys for authenticating requests
    pub api_keys: Vec<String>,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub models: Vec<Model>,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default = "default_refresh_interval_secs")]
    pub refresh_interval_secs: u64,
    #[serde(default)]
    pub fallback_models: FallbackModels,
    /// Load balancing strategy for distributing requests across providers
    #[serde(default)]
    pub load_balancing: LoadBalancingStrategy,
    /// Optional maximum request body size in bytes.
    /// If not set, Axum's default (2 MiB) applies.
    #[serde(skip_serializing, skip_deserializing)]
    pub request_body_limit: Option<usize>,
}

/// A single AI Core provider configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Provider {
    /// Unique identifier for this provider
    pub name: String,
    /// UAA OAuth token URL
    pub uaa_token_url: String,
    /// UAA client ID
    pub uaa_client_id: String,
    /// UAA client secret
    pub uaa_client_secret: String,
    /// AI Core API base URL
    pub genai_api_url: String,
    /// Resource group for this provider
    #[serde(default = "default_resource_group")]
    pub resource_group: String,
    /// Weight for load balancing (higher = more traffic)
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// Whether this provider is enabled
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_weight() -> u32 {
    1
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConfigFile {
    #[serde(default)]
    pub log_level: Option<String>,
    /// Legacy single-provider credentials (for backward compatibility)
    #[serde(default)]
    pub credentials: Option<Credentials>,
    /// Multiple providers for load balancing
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub models: Vec<Model>,
    #[serde(default)]
    pub resource_group: Option<String>,
    #[serde(default)]
    pub refresh_interval_secs: Option<u64>,
    #[serde(default)]
    pub fallback_models: FallbackModels,
    /// API keys for authenticating requests (moved from credentials)
    #[serde(default)]
    pub api_keys: Vec<String>,
    /// Load balancing strategy
    #[serde(default)]
    pub load_balancing: LoadBalancingStrategy,
    /// Optional maximum request body size in bytes.
    #[serde(default)]
    pub request_body_limit: Option<usize>,
}

/// Provider configuration as read from config file
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderConfig {
    /// Unique identifier for this provider
    pub name: String,
    /// UAA OAuth token URL
    pub uaa_token_url: String,
    /// UAA client ID
    pub uaa_client_id: String,
    /// UAA client secret
    pub uaa_client_secret: String,
    /// AI Core API base URL
    pub genai_api_url: String,
    /// Resource group for this provider
    #[serde(default)]
    pub resource_group: Option<String>,
    /// Weight for load balancing (higher = more traffic)
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// Whether this provider is enabled
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Credentials {
    pub uaa_token_url: Option<String>,
    pub uaa_client_id: Option<String>,
    pub uaa_client_secret: Option<String>,
    pub aicore_api_url: Option<String>,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Model {
    pub name: String,
    /// The model name as it appears in AI Core deployments.
    /// If not specified, the `name` field is used to look up deployments.
    pub aicore_model_name: Option<String>,
    /// Alias patterns that should resolve to this model.
    /// Supports trailing wildcard (*) for prefix matching.
    /// Example: ["claude-sonnet-4-5-*", "claude-4-sonnet"]
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// Configuration for fallback models per model family.
/// When a requested model is not found, the router will fall back to the
/// configured model for that family (if available and configured).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FallbackModels {
    /// Fallback model for Claude family (models starting with "claude")
    #[serde(default)]
    pub claude: Option<String>,
    /// Fallback model for OpenAI family (models starting with "gpt" or "text")
    #[serde(default)]
    pub openai: Option<String>,
    /// Fallback model for Gemini family (models starting with "gemini")
    #[serde(default)]
    pub gemini: Option<String>,
}

/// Load balancing strategy for distributing requests across providers.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoadBalancingStrategy {
    /// Round-robin: Distribute requests evenly across providers.
    /// Each request goes to the next provider in rotation.
    /// If a provider returns 429, automatically falls back to the next provider.
    #[default]
    RoundRobin,
    /// Fallback: Always try the first provider first.
    /// Only switch to the next provider if the current one returns 429 (rate limited).
    /// This prioritizes a primary provider while using others as backup.
    Fallback,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_log_level() -> String {
    DEFAULT_LOG_LEVEL.to_string()
}

fn default_refresh_interval_secs() -> u64 {
    DEFAULT_REFRESH_INTERVAL_SECS
}

fn default_resource_group() -> String {
    DEFAULT_RESOURCE_GROUP.to_string()
}

fn normalize_oauth_token_url(url: String) -> String {
    if !url.contains("/oauth/token") && !url.ends_with('/') {
        format!("{url}/oauth/token")
    } else if url.ends_with('/') && !url.contains("/oauth/token") {
        format!("{url}oauth/token")
    } else {
        url
    }
}

impl Config {
    pub fn load(config_path: Option<&str>) -> Result<Self> {
        let config_file_path = match config_path {
            Some(path) => path.to_string(),
            None => {
                let home = env::var("HOME").context("HOME environment variable not set")?;
                format!("{home}/.aicore/config.yaml")
            }
        };

        if !Path::new(&config_file_path).exists() {
            return Err(anyhow::anyhow!(
                "Config file not found: {}. Please create a config file.",
                config_file_path
            ));
        }

        let config_content = std::fs::read_to_string(&config_file_path)
            .with_context(|| format!("Failed to read config file: {config_file_path}"))?;
        let file_config = serde_yaml::from_str::<ConfigFile>(&config_content)
            .with_context(|| format!("Failed to parse config file: {config_file_path}"))?;

        Self::from_file_and_env(file_config)
    }

    pub fn get_aicore_model_name(&self, model_name: &str) -> Option<&str> {
        self.models
            .iter()
            .find(|m| m.name == model_name)?
            .aicore_model_name
            .as_deref()
    }

    pub fn get_model_names(&self) -> Vec<&str> {
        self.models.iter().map(|m| m.name.as_str()).collect()
    }

    /// Get the fallback model for a given model family prefix
    pub fn get_fallback_model(&self, prefix: &str) -> Option<&str> {
        use crate::constants::models::*;
        match prefix {
            CLAUDE_PREFIX => self.fallback_models.claude.as_deref(),
            GPT_PREFIX | TEXT_PREFIX => self.fallback_models.openai.as_deref(),
            GEMINI_PREFIX => self.fallback_models.gemini.as_deref(),
            _ => None,
        }
    }

    fn from_file_and_env(file_config: ConfigFile) -> Result<Self> {
        // Build providers list from multiple sources
        let mut providers: Vec<Provider> = Vec::new();

        // First, add providers from the providers array in config file
        for p in file_config.providers {
            providers.push(Provider {
                name: p.name,
                uaa_token_url: normalize_oauth_token_url(p.uaa_token_url),
                uaa_client_id: p.uaa_client_id,
                uaa_client_secret: p.uaa_client_secret,
                genai_api_url: p.genai_api_url,
                resource_group: p.resource_group.unwrap_or_else(default_resource_group),
                weight: p.weight,
                enabled: p.enabled,
            });
        }

        // If no providers configured, try to build one from legacy credentials or env vars
        if providers.is_empty() {
            let uaa_token_url = env::var("UAA_TOKEN_URL")
                .or_else(|_| {
                    file_config
                        .credentials
                        .as_ref()
                        .and_then(|c| c.uaa_token_url.as_ref())
                        .cloned()
                        .ok_or(anyhow::anyhow!("uaa_token_url not found"))
                })
                .map(normalize_oauth_token_url)
                .context("uaa_token_url is required in config file or UAA_TOKEN_URL env var")?;

            let uaa_client_id = env::var("UAA_CLIENT_ID")
                .or_else(|_| {
                    file_config
                        .credentials
                        .as_ref()
                        .and_then(|c| c.uaa_client_id.as_ref())
                        .cloned()
                        .ok_or(anyhow::anyhow!("uaa_client_id not found"))
                })
                .context("uaa_client_id is required in config file or UAA_CLIENT_ID env var")?;

            let uaa_client_secret = env::var("UAA_CLIENT_SECRET")
                .or_else(|_| {
                    file_config
                        .credentials
                        .as_ref()
                        .and_then(|c| c.uaa_client_secret.as_ref())
                        .cloned()
                        .ok_or(anyhow::anyhow!("uaa_client_secret not found"))
                })
                .context(
                    "uaa_client_secret is required in config file or UAA_CLIENT_SECRET env var",
                )?;

            let genai_api_url = env::var("GENAI_API_URL")
                .or_else(|_| {
                    file_config
                        .credentials
                        .as_ref()
                        .and_then(|c| c.aicore_api_url.as_ref())
                        .cloned()
                        .ok_or(anyhow::anyhow!("genai_api_url not found"))
                })
                .context("aicore_api_url is required in config file or GENAI_API_URL env var")?;

            let resource_group = env::var("RESOURCE_GROUP")
                .ok()
                .or(file_config.resource_group.clone())
                .unwrap_or_else(default_resource_group);

            providers.push(Provider {
                name: "default".to_string(),
                uaa_token_url,
                uaa_client_id,
                uaa_client_secret,
                genai_api_url,
                resource_group,
                weight: 1,
                enabled: true,
            });
        }

        // Build api_keys list from multiple sources:
        // 1. API_KEY env var (single key, for backward compatibility)
        // 2. API_KEYS env var (comma-separated list)
        // 3. api_keys from config file root level
        // 4. credentials.api_key from config file (legacy, for backward compatibility)
        let mut api_keys: Vec<String> = Vec::new();

        // Add from API_KEY env var (backward compatibility)
        if let Ok(key) = env::var("API_KEY") {
            api_keys.push(key);
        }

        // Add from API_KEYS env var (comma-separated)
        if let Ok(keys) = env::var("API_KEYS") {
            api_keys.extend(
                keys.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
            );
        }

        // Add from config file root level api_keys
        api_keys.extend(file_config.api_keys);

        // Add from credentials.api_key (legacy backward compatibility)
        if let Some(ref creds) = file_config.credentials
            && let Some(ref key) = creds.api_key
            && !api_keys.contains(key)
        {
            api_keys.push(key.clone());
        }

        // Deduplicate while preserving order
        let mut seen = std::collections::HashSet::new();
        api_keys.retain(|k| seen.insert(k.clone()));

        if api_keys.is_empty() {
            return Err(anyhow::anyhow!(
                "At least one API key is required. Set via API_KEY/API_KEYS env var or api_keys in config file"
            ));
        }

        let port = env::var("PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(file_config.port);

        let log_level = env::var("LOG_LEVEL")
            .ok()
            .or(file_config.log_level)
            .unwrap_or_else(default_log_level);

        let refresh_interval_secs = env::var("REFRESH_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .or(file_config.refresh_interval_secs)
            .unwrap_or_else(default_refresh_interval_secs);

        let models = file_config.models;
        let fallback_models = file_config.fallback_models;
        let load_balancing = file_config.load_balancing;
        // REQUEST_BODY_LIMIT can override the file value. Accepts plain number of bytes.
        let request_body_limit = env::var("REQUEST_BODY_LIMIT")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .or(file_config.request_body_limit);

        Ok(Config {
            providers,
            api_keys,
            port,
            models,
            log_level,
            refresh_interval_secs,
            fallback_models,
            load_balancing,
            request_body_limit,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_config_parsing_with_all_fields() {
        let yaml_content = r#"
log_level: DEBUG
port: 9000
request_body_limit: 2097152
credentials:
  uaa_token_url: https://test.example.com/oauth/token
  uaa_client_id: test-client-id
  uaa_client_secret: test-client-secret
  aicore_api_url: https://api.test.example.com
  api_key: test-api-key
models:
  - name: gpt-4
    aicore_model_name: gpt-4-turbo
  - name: claude-3
    aicore_model_name: anthropic--claude-3
"#;

        let config_file: ConfigFile =
            serde_yaml::from_str(yaml_content).expect("Failed to parse YAML");

        assert_eq!(config_file.port, 9000);
        assert_eq!(config_file.log_level, Some("DEBUG".to_string()));
        assert_eq!(config_file.models.len(), 2);
        assert_eq!(config_file.models[0].name, "gpt-4");
        assert_eq!(
            config_file.models[0].aicore_model_name,
            Some("gpt-4-turbo".to_string())
        );

        let creds = config_file.credentials.unwrap();
        assert_eq!(
            creds.uaa_token_url,
            Some("https://test.example.com/oauth/token".to_string())
        );
        assert_eq!(creds.uaa_client_id, Some("test-client-id".to_string()));
        assert_eq!(creds.api_key, Some("test-api-key".to_string()));
        assert_eq!(config_file.request_body_limit, Some(2_097_152));
    }

    #[test]
    fn test_config_load_from_file() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let config_path = temp_dir.path().join("test_config.yaml");

        let yaml_content = r#"
port: 8080
credentials:
  uaa_token_url: https://test.example.com/oauth/token
  uaa_client_id: test-client-id
  uaa_client_secret: test-client-secret
  aicore_api_url: https://api.test.example.com
  api_key: test-api-key
models:
  - name: test-model
    aicore_model_name: test-aicore-model
"#;

        fs::write(&config_path, yaml_content).expect("Failed to write config file");

        let config =
            Config::load(Some(config_path.to_str().unwrap())).expect("Failed to load config");

        assert_eq!(config.port, 8080);
        // With legacy credentials, a default provider is created
        assert_eq!(config.providers.len(), 1);
        assert_eq!(config.providers[0].name, "default");
        assert_eq!(
            config.providers[0].uaa_token_url,
            "https://test.example.com/oauth/token"
        );
        assert_eq!(config.providers[0].uaa_client_id, "test-client-id");
        assert_eq!(
            config.providers[0].genai_api_url,
            "https://api.test.example.com"
        );
        assert_eq!(config.api_keys, vec!["test-api-key".to_string()]);
        assert_eq!(config.models.len(), 1);
        assert_eq!(config.models[0].name, "test-model");
        assert_eq!(
            config.models[0].aicore_model_name,
            Some("test-aicore-model".to_string())
        );
        assert_eq!(config.request_body_limit, None);
    }

    #[test]
    fn test_config_missing_required_fields() {
        let yaml_content = r#"
port: 8080
credentials:
  uaa_token_url: https://test.example.com/oauth/token
  # Missing required fields
"#;

        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let config_path = temp_dir.path().join("invalid_config.yaml");
        fs::write(&config_path, yaml_content).expect("Failed to write config file");

        let result = Config::load(Some(config_path.to_str().unwrap()));
        assert!(result.is_err());

        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("required"));
    }

    #[test]
    fn test_config_file_not_found() {
        let result = Config::load(Some("/nonexistent/path/config.yaml"));
        assert!(result.is_err());

        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("Config file not found"));
    }

    #[test]
    fn test_default_port() {
        assert_eq!(default_port(), 8900);
    }

    #[test]
    fn test_partial_config_merge() {
        let config_file = ConfigFile {
            log_level: Some("INFO".to_string()),
            port: 3000,
            credentials: Some(Credentials {
                uaa_token_url: Some("https://example.com".to_string()),
                uaa_client_id: Some("client123".to_string()),
                uaa_client_secret: Some("secret456".to_string()),
                aicore_api_url: Some("https://api.example.com".to_string()),
                api_key: Some("key789".to_string()),
            }),
            providers: vec![],
            models: vec![Model {
                name: "model1".to_string(),
                aicore_model_name: Some("aicore-model-1".to_string()),
                aliases: vec![],
            }],
            resource_group: Some("test-group".to_string()),
            refresh_interval_secs: None,
            fallback_models: FallbackModels::default(),
            api_keys: vec![],
            load_balancing: LoadBalancingStrategy::default(),
            request_body_limit: None,
        };

        let config = Config::from_file_and_env(config_file).expect("Failed to create config");

        assert_eq!(config.port, 3000);
        // Legacy credentials creates a default provider
        assert_eq!(config.providers.len(), 1);
        assert_eq!(
            config.providers[0].uaa_token_url,
            "https://example.com/oauth/token"
        );
        assert_eq!(config.models.len(), 1);
        assert_eq!(config.models[0].name, "model1");
        assert_eq!(
            config.models[0].aicore_model_name,
            Some("aicore-model-1".to_string())
        );
        assert_eq!(config.providers[0].resource_group, "test-group");
        // api_key from credentials.api_key should be picked up for backward compatibility
        assert_eq!(config.api_keys, vec!["key789".to_string()]);
    }

    #[test]
    fn test_token_url_automatic_oauth_token_suffix() {
        // Test case 1: URL without any path should get /oauth/token appended
        assert_eq!(
            normalize_oauth_token_url("https://auth.example.com".to_string()),
            "https://auth.example.com/oauth/token"
        );

        // Test case 2: URL ending with slash should get oauth/token appended
        assert_eq!(
            normalize_oauth_token_url("https://auth.example.com/".to_string()),
            "https://auth.example.com/oauth/token"
        );

        // Test case 3: URL already containing /oauth/token should remain unchanged
        assert_eq!(
            normalize_oauth_token_url("https://auth.example.com/oauth/token".to_string()),
            "https://auth.example.com/oauth/token"
        );

        // Test case 4: URL with custom path containing /oauth/token should remain unchanged
        assert_eq!(
            normalize_oauth_token_url("https://auth.example.com/uaa/oauth/token".to_string()),
            "https://auth.example.com/uaa/oauth/token"
        );
    }

    #[test]
    fn test_fallback_models_parsing() {
        let yaml_content = r#"
port: 8080
credentials:
  uaa_token_url: https://test.example.com/oauth/token
  uaa_client_id: test-client-id
  uaa_client_secret: test-client-secret
  aicore_api_url: https://api.test.example.com
  api_key: test-api-key
models:
  - name: claude-sonnet-4-5
    aicore_model_name: dep-claude
  - name: gpt-4o
    aicore_model_name: dep-gpt
  - name: gemini-1.5-pro
    aicore_model_name: dep-gemini
fallback_models:
  claude: claude-sonnet-4-5
  openai: gpt-4o
  gemini: gemini-1.5-pro
"#;

        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let config_path = temp_dir.path().join("fallback_config.yaml");
        fs::write(&config_path, yaml_content).expect("Failed to write config file");

        let config =
            Config::load(Some(config_path.to_str().unwrap())).expect("Failed to load config");

        assert_eq!(
            config.fallback_models.claude,
            Some("claude-sonnet-4-5".to_string())
        );
        assert_eq!(config.fallback_models.openai, Some("gpt-4o".to_string()));
        assert_eq!(
            config.fallback_models.gemini,
            Some("gemini-1.5-pro".to_string())
        );
    }

    #[test]
    fn test_fallback_models_partial_config() {
        let yaml_content = r#"
port: 8080
credentials:
  uaa_token_url: https://test.example.com/oauth/token
  uaa_client_id: test-client-id
  uaa_client_secret: test-client-secret
  aicore_api_url: https://api.test.example.com
  api_key: test-api-key
models:
  - name: claude-sonnet-4-5
    aicore_model_name: dep-claude
fallback_models:
  claude: claude-sonnet-4-5
"#;

        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let config_path = temp_dir.path().join("partial_fallback_config.yaml");
        fs::write(&config_path, yaml_content).expect("Failed to write config file");

        let config =
            Config::load(Some(config_path.to_str().unwrap())).expect("Failed to load config");

        assert_eq!(
            config.fallback_models.claude,
            Some("claude-sonnet-4-5".to_string())
        );
        assert_eq!(config.fallback_models.openai, None);
        assert_eq!(config.fallback_models.gemini, None);
    }

    #[test]
    fn test_fallback_models_default_empty() {
        let yaml_content = r#"
port: 8080
credentials:
  uaa_token_url: https://test.example.com/oauth/token
  uaa_client_id: test-client-id
  uaa_client_secret: test-client-secret
  aicore_api_url: https://api.test.example.com
  api_key: test-api-key
models:
  - name: gpt-4
    aicore_model_name: dep-123
"#;

        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let config_path = temp_dir.path().join("no_fallback_config.yaml");
        fs::write(&config_path, yaml_content).expect("Failed to write config file");

        let config =
            Config::load(Some(config_path.to_str().unwrap())).expect("Failed to load config");

        assert_eq!(config.fallback_models.claude, None);
        assert_eq!(config.fallback_models.openai, None);
        assert_eq!(config.fallback_models.gemini, None);
    }

    #[test]
    fn test_multiple_api_keys() {
        let yaml_content = r#"
port: 8080
credentials:
  uaa_token_url: https://test.example.com/oauth/token
  uaa_client_id: test-client-id
  uaa_client_secret: test-client-secret
  aicore_api_url: https://api.test.example.com
models:
  - name: gpt-4
    aicore_model_name: dep-123
api_keys:
  - key-one
  - key-two
  - key-three
"#;

        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let config_path = temp_dir.path().join("multi_api_keys_config.yaml");
        fs::write(&config_path, yaml_content).expect("Failed to write config file");

        let config =
            Config::load(Some(config_path.to_str().unwrap())).expect("Failed to load config");

        assert_eq!(config.api_keys.len(), 3);
        assert_eq!(config.api_keys[0], "key-one");
        assert_eq!(config.api_keys[1], "key-two");
        assert_eq!(config.api_keys[2], "key-three");
    }

    #[test]
    fn test_api_keys_deduplication() {
        let yaml_content = r#"
port: 8080
credentials:
  uaa_token_url: https://test.example.com/oauth/token
  uaa_client_id: test-client-id
  uaa_client_secret: test-client-secret
  aicore_api_url: https://api.test.example.com
  api_key: duplicate-key
models:
  - name: gpt-4
    aicore_model_name: dep-123
api_keys:
  - duplicate-key
  - another-key
"#;

        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let config_path = temp_dir.path().join("dedup_api_keys_config.yaml");
        fs::write(&config_path, yaml_content).expect("Failed to write config file");

        let config =
            Config::load(Some(config_path.to_str().unwrap())).expect("Failed to load config");

        // Should deduplicate, keeping the first occurrence
        assert_eq!(config.api_keys.len(), 2);
        assert!(config.api_keys.contains(&"duplicate-key".to_string()));
        assert!(config.api_keys.contains(&"another-key".to_string()));
    }

    #[test]
    fn test_legacy_credentials_api_key_backward_compat() {
        let yaml_content = r#"
port: 8080
credentials:
  uaa_token_url: https://test.example.com/oauth/token
  uaa_client_id: test-client-id
  uaa_client_secret: test-client-secret
  aicore_api_url: https://api.test.example.com
  api_key: legacy-key
models:
  - name: gpt-4
    aicore_model_name: dep-123
"#;

        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let config_path = temp_dir.path().join("legacy_api_key_config.yaml");
        fs::write(&config_path, yaml_content).expect("Failed to write config file");

        let config =
            Config::load(Some(config_path.to_str().unwrap())).expect("Failed to load config");

        // Legacy api_key in credentials should still work
        assert_eq!(config.api_keys, vec!["legacy-key".to_string()]);
    }

    #[test]
    fn test_multi_provider_config() {
        let yaml_content = r#"
port: 8080
api_keys:
  - shared-api-key
providers:
  - name: provider1
    uaa_token_url: https://provider1.example.com/oauth/token
    uaa_client_id: client1
    uaa_client_secret: secret1
    genai_api_url: https://api1.example.com
    resource_group: rg1
    weight: 2
    enabled: true
  - name: provider2
    uaa_token_url: https://provider2.example.com/oauth/token
    uaa_client_id: client2
    uaa_client_secret: secret2
    genai_api_url: https://api2.example.com
    resource_group: rg2
    weight: 1
    enabled: true
  - name: provider3-disabled
    uaa_token_url: https://provider3.example.com/oauth/token
    uaa_client_id: client3
    uaa_client_secret: secret3
    genai_api_url: https://api3.example.com
    resource_group: rg3
    enabled: false
models:
  - name: gpt-4
    aicore_model_name: dep-123
"#;

        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let config_path = temp_dir.path().join("multi_provider_config.yaml");
        fs::write(&config_path, yaml_content).expect("Failed to write config file");

        let config =
            Config::load(Some(config_path.to_str().unwrap())).expect("Failed to load config");

        assert_eq!(config.providers.len(), 3);

        // Check provider1
        assert_eq!(config.providers[0].name, "provider1");
        assert_eq!(
            config.providers[0].uaa_token_url,
            "https://provider1.example.com/oauth/token"
        );
        assert_eq!(config.providers[0].resource_group, "rg1");
        assert_eq!(config.providers[0].weight, 2);
        assert!(config.providers[0].enabled);

        // Check provider2
        assert_eq!(config.providers[1].name, "provider2");
        assert_eq!(config.providers[1].resource_group, "rg2");
        assert_eq!(config.providers[1].weight, 1);

        // Check disabled provider
        assert_eq!(config.providers[2].name, "provider3-disabled");
        assert!(!config.providers[2].enabled);

        assert_eq!(config.api_keys, vec!["shared-api-key".to_string()]);
    }

}
