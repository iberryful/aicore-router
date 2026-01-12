use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::path::Path;

use crate::constants::config::*;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub uaa_token_url: String,
    pub uaa_client_id: String,
    pub uaa_client_secret: String,
    pub genai_api_url: String,
    pub api_key: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub models: Vec<Model>,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default = "default_resource_group")]
    pub resource_group: String,
    #[serde(default = "default_refresh_interval_secs")]
    pub refresh_interval_secs: u64,
    #[serde(default)]
    pub fallback_models: FallbackModels,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConfigFile {
    #[serde(default)]
    pub log_level: Option<String>,
    #[serde(default)]
    pub credentials: Option<Credentials>,
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
    pub deployment_id: Option<String>,
    pub aicore_model_name: Option<String>,
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

    pub fn get_deployment_id(&self, model_name: &str) -> Option<&str> {
        self.models
            .iter()
            .find(|m| m.name == model_name)?
            .deployment_id
            .as_deref()
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
            .context("uaa_client_secret is required in config file or UAA_CLIENT_SECRET env var")?;

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

        let api_key = env::var("API_KEY")
            .or_else(|_| {
                file_config
                    .credentials
                    .as_ref()
                    .and_then(|c| c.api_key.as_ref())
                    .cloned()
                    .ok_or(anyhow::anyhow!("api_key not found"))
            })
            .context("api_key is required in config file or API_KEY env var")?;

        let port = env::var("PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(file_config.port);

        let log_level = env::var("LOG_LEVEL")
            .ok()
            .or(file_config.log_level)
            .unwrap_or_else(default_log_level);

        let resource_group = env::var("RESOURCE_GROUP")
            .ok()
            .or(file_config.resource_group)
            .unwrap_or_else(default_resource_group);

        let refresh_interval_secs = env::var("REFRESH_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .or(file_config.refresh_interval_secs)
            .unwrap_or_else(default_refresh_interval_secs);

        let models = file_config.models;
        let fallback_models = file_config.fallback_models;

        Ok(Config {
            uaa_token_url,
            uaa_client_id,
            uaa_client_secret,
            genai_api_url,
            api_key,
            port,
            models,
            log_level,
            resource_group,
            refresh_interval_secs,
            fallback_models,
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
credentials:
  uaa_token_url: https://test.example.com/oauth/token
  uaa_client_id: test-client-id
  uaa_client_secret: test-client-secret
  aicore_api_url: https://api.test.example.com
  api_key: test-api-key
models:
  - name: gpt-4
    deployment_id: dep-123
  - name: claude-3
    deployment_id: dep-456
"#;

        let config_file: ConfigFile =
            serde_yaml::from_str(yaml_content).expect("Failed to parse YAML");

        assert_eq!(config_file.port, 9000);
        assert_eq!(config_file.log_level, Some("DEBUG".to_string()));
        assert_eq!(config_file.models.len(), 2);
        assert_eq!(config_file.models[0].name, "gpt-4");
        assert_eq!(
            config_file.models[0].deployment_id,
            Some("dep-123".to_string())
        );

        let creds = config_file.credentials.unwrap();
        assert_eq!(
            creds.uaa_token_url,
            Some("https://test.example.com/oauth/token".to_string())
        );
        assert_eq!(creds.uaa_client_id, Some("test-client-id".to_string()));
        assert_eq!(creds.api_key, Some("test-api-key".to_string()));
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
    deployment_id: test-deployment
"#;

        fs::write(&config_path, yaml_content).expect("Failed to write config file");

        let config =
            Config::load(Some(config_path.to_str().unwrap())).expect("Failed to load config");

        assert_eq!(config.port, 8080);
        assert_eq!(config.uaa_token_url, "https://test.example.com/oauth/token");
        assert_eq!(config.uaa_client_id, "test-client-id");
        assert_eq!(config.genai_api_url, "https://api.test.example.com");
        assert_eq!(config.api_key, "test-api-key");
        assert_eq!(config.models.len(), 1);
        assert_eq!(config.models[0].name, "test-model");
        assert_eq!(
            config.models[0].deployment_id,
            Some("test-deployment".to_string())
        );
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
            models: vec![Model {
                name: "model1".to_string(),
                deployment_id: Some("dep1".to_string()),
                aicore_model_name: None,
            }],
            resource_group: Some("test-group".to_string()),
            refresh_interval_secs: None,
            fallback_models: FallbackModels::default(),
        };

        let config = Config::from_file_and_env(config_file).expect("Failed to create config");

        assert_eq!(config.port, 3000);
        assert_eq!(config.uaa_token_url, "https://example.com/oauth/token");
        assert_eq!(config.models.len(), 1);
        assert_eq!(config.models[0].name, "model1");
        assert_eq!(config.models[0].deployment_id, Some("dep1".to_string()));
        assert_eq!(config.resource_group, "test-group");
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
    deployment_id: dep-claude
  - name: gpt-4o
    deployment_id: dep-gpt
  - name: gemini-1.5-pro
    deployment_id: dep-gemini
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
    deployment_id: dep-claude
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
    deployment_id: dep-123
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
}
