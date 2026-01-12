//! Model registry that tracks deployments across multiple providers.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::client::AiCoreClient;
use crate::config::{FallbackModels, Model, Provider};
use crate::token::TokenManager;

/// Resolved deployment information including which provider hosts it
#[derive(Debug, Clone)]
pub struct ResolvedDeployment {
    pub deployment_id: String,
    pub provider_name: String,
}

/// Runtime model registry that manages resolved deployment IDs across multiple providers
#[derive(Debug, Clone)]
pub struct ModelRegistry {
    /// Resolved model name to deployment info mappings (model -> list of providers that have it)
    resolved_models: Arc<RwLock<HashMap<String, Vec<ResolvedDeployment>>>>,
    /// Original model configurations from config file
    config_models: Vec<Model>,
    /// Fallback models configuration for each family
    fallback_models: FallbackModels,
    /// Providers to query for deployments
    providers: Vec<Provider>,
    /// Token manager for authentication
    token_manager: TokenManager,
    /// Refresh interval for background updates
    refresh_interval: Duration,
}

impl ModelRegistry {
    /// Create a new model registry
    pub fn new(
        config_models: Vec<Model>,
        fallback_models: FallbackModels,
        providers: Vec<Provider>,
        token_manager: TokenManager,
        refresh_interval_secs: u64,
    ) -> Self {
        Self {
            resolved_models: Arc::new(RwLock::new(HashMap::new())),
            config_models,
            fallback_models,
            providers,
            token_manager,
            refresh_interval: Duration::from_secs(refresh_interval_secs),
        }
    }

    /// Start the registry with initial resolution and background refresh
    pub async fn start(&self) -> Result<()> {
        // Validate fallback models configuration
        self.validate_fallback_models();

        // Initial resolution
        self.refresh_deployments().await?;

        // Start background refresh task
        let registry = self.clone();
        tokio::spawn(async move {
            registry.background_refresh().await;
        });

        Ok(())
    }

    /// Validate that configured fallback models exist in the models list
    fn validate_fallback_models(&self) {
        let model_names: Vec<&str> = self.config_models.iter().map(|m| m.name.as_str()).collect();

        if let Some(ref claude_fallback) = self.fallback_models.claude
            && !model_names.contains(&claude_fallback.as_str())
        {
            warn!(
                "Fallback model '{}' for claude family is not configured in models list",
                claude_fallback
            );
        }

        if let Some(ref openai_fallback) = self.fallback_models.openai
            && !model_names.contains(&openai_fallback.as_str())
        {
            warn!(
                "Fallback model '{}' for openai family is not configured in models list",
                openai_fallback
            );
        }

        if let Some(ref gemini_fallback) = self.fallback_models.gemini
            && !model_names.contains(&gemini_fallback.as_str())
        {
            warn!(
                "Fallback model '{}' for gemini family is not configured in models list",
                gemini_fallback
            );
        }
    }

    /// Get deployment info for a model on a specific provider
    pub async fn get_deployment_for_provider(
        &self,
        model_name: &str,
        provider_name: &str,
    ) -> Option<String> {
        let resolved = self.resolved_models.read().await;
        resolved.get(model_name).and_then(|deployments| {
            deployments
                .iter()
                .find(|d| d.provider_name == provider_name)
                .map(|d| d.deployment_id.clone())
        })
    }

    /// Get all providers that have a specific model deployed
    pub async fn get_providers_for_model(&self, model_name: &str) -> Vec<ResolvedDeployment> {
        let resolved = self.resolved_models.read().await;
        resolved.get(model_name).cloned().unwrap_or_default()
    }

    /// Get deployment ID for a model (returns first available for backward compatibility)
    pub async fn get_deployment_id(&self, model_name: &str) -> Option<String> {
        let resolved = self.resolved_models.read().await;
        resolved
            .get(model_name)
            .and_then(|deployments| deployments.first())
            .map(|d| d.deployment_id.clone())
    }

    /// Get all available (resolved) model names
    pub async fn get_available_models(&self) -> Vec<String> {
        let resolved = self.resolved_models.read().await;
        let mut models: Vec<String> = resolved.keys().cloned().collect();
        models.sort();
        models
    }

    /// Check if a model is available (has been resolved) on any provider
    pub async fn is_model_available(&self, model_name: &str) -> bool {
        let resolved = self.resolved_models.read().await;
        resolved.contains_key(model_name)
    }

    /// Find model configuration by name
    pub fn find_model_config(&self, model_name: &str) -> Option<&Model> {
        self.config_models.iter().find(|m| m.name == model_name)
    }

    /// Get fallback model for a given model prefix/family
    pub fn get_fallback_model(&self, prefix: &str) -> Option<&str> {
        use crate::constants::models::*;
        match prefix {
            CLAUDE_PREFIX => self.fallback_models.claude.as_deref(),
            GPT_PREFIX | TEXT_PREFIX => self.fallback_models.openai.as_deref(),
            GEMINI_PREFIX => self.fallback_models.gemini.as_deref(),
            _ => None,
        }
    }

    /// Get model names from configuration (not necessarily resolved)
    pub fn get_configured_model_names(&self) -> Vec<&str> {
        self.config_models.iter().map(|m| m.name.as_str()).collect()
    }

    /// Find a model config by checking aliases with glob pattern matching.
    /// Returns the model with the most specific matching pattern.
    /// Specificity is determined by the length of the literal prefix before the `*`.
    pub fn find_model_by_alias(&self, requested_model: &str) -> Option<&Model> {
        let mut best_match: Option<(&Model, usize)> = None;

        for model in &self.config_models {
            for alias in &model.aliases {
                if let Some(specificity) = glob_matches(alias, requested_model) {
                    match &best_match {
                        None => {
                            best_match = Some((model, specificity));
                        }
                        Some((_, current_specificity)) if specificity > *current_specificity => {
                            best_match = Some((model, specificity));
                        }
                        _ => {} // Keep current best match
                    }
                }
            }
        }

        best_match.map(|(model, _)| model)
    }

    async fn background_refresh(&self) {
        let mut interval = tokio::time::interval(self.refresh_interval);

        // Skip the first tick since we already did initial refresh
        interval.tick().await;

        loop {
            interval.tick().await;
            if let Err(e) = self.refresh_deployments().await {
                error!("Failed to refresh deployments: {}", e);
            }
        }
    }

    async fn refresh_deployments(&self) -> Result<()> {
        info!(
            "Refreshing deployment mappings for {} providers...",
            self.providers.len()
        );

        let mut all_resolved: HashMap<String, Vec<ResolvedDeployment>> = HashMap::new();

        // Query each provider for deployments
        for provider in &self.providers {
            if !provider.enabled {
                continue;
            }

            info!(
                "Querying provider '{}' (resource_group: {})...",
                provider.name, provider.resource_group
            );

            // Create a client for this provider
            let client = AiCoreClient::from_provider(provider.clone(), self.token_manager.clone());

            match client
                .build_model_to_deployment_mapping(Some(&provider.resource_group))
                .await
            {
                Ok(aicore_deployments) => {
                    for model_config in &self.config_models {
                        // Use aicore_model_name if specified, otherwise use the model name itself
                        let aicore_model_name = model_config
                            .aicore_model_name
                            .as_ref()
                            .unwrap_or(&model_config.name);

                        // Resolve from AI Core model name
                        if let Some(deployment_id) = aicore_deployments.get(aicore_model_name) {
                            all_resolved
                                .entry(model_config.name.clone())
                                .or_default()
                                .push(ResolvedDeployment {
                                    deployment_id: deployment_id.clone(),
                                    provider_name: provider.name.clone(),
                                });
                            info!(
                                "Provider '{}': Model '{}' -> aicore_model_name: '{}' -> deployment_id: {}",
                                provider.name, model_config.name, aicore_model_name, deployment_id
                            );
                        }
                    }
                }
                Err(e) => {
                    error!(
                        "Failed to query provider '{}': {}. Skipping this provider.",
                        provider.name, e
                    );
                }
            }
        }

        let resolved_count = all_resolved.len();
        let total_deployments: usize = all_resolved.values().map(|v| v.len()).sum();

        // Update the resolved models
        {
            let mut resolved_models = self.resolved_models.write().await;
            *resolved_models = all_resolved;
        }

        info!(
            "Deployment refresh complete: {} models resolved across {} provider deployments",
            resolved_count, total_deployments
        );

        Ok(())
    }
}

/// Check if a glob pattern matches a string.
/// Only supports trailing `*` wildcard (prefix matching).
/// Returns the specificity (length of literal prefix) if matches, None otherwise.
/// Higher specificity = more specific match.
fn glob_matches(pattern: &str, input: &str) -> Option<usize> {
    if let Some(prefix) = pattern.strip_suffix('*') {
        // Pattern ends with *: prefix match
        if input.starts_with(prefix) {
            Some(prefix.len()) // Specificity = length of literal part
        } else {
            None
        }
    } else {
        // No wildcard: exact match only
        if pattern == input {
            Some(pattern.len())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glob_matches_trailing_wildcard() {
        // Prefix match with trailing *
        assert_eq!(glob_matches("claude-*", "claude-sonnet"), Some(7));
        assert_eq!(
            glob_matches("claude-sonnet-4-5-*", "claude-sonnet-4-5-20250929"),
            Some(18)
        );
        assert_eq!(glob_matches("gpt-4o-*", "gpt-4o-mini"), Some(7));

        // Non-matching prefix
        assert_eq!(glob_matches("claude-*", "gpt-4"), None);
        assert_eq!(
            glob_matches("claude-sonnet-4-5-*", "claude-sonnet-4-0"),
            None
        );
    }

    #[test]
    fn test_glob_matches_exact() {
        // Exact match (no wildcard)
        assert_eq!(glob_matches("claude-sonnet", "claude-sonnet"), Some(13));
        assert_eq!(glob_matches("gpt-4o", "gpt-4o"), Some(6));

        // Non-matching exact
        assert_eq!(glob_matches("claude-sonnet", "claude-sonnet-4"), None);
        assert_eq!(glob_matches("gpt-4o", "gpt-4o-mini"), None);
    }

    #[test]
    fn test_glob_matches_empty_pattern() {
        // Edge case: wildcard only matches anything
        assert_eq!(glob_matches("*", "anything"), Some(0));
        assert_eq!(glob_matches("*", ""), Some(0));

        // Empty pattern only matches empty string
        assert_eq!(glob_matches("", ""), Some(0));
        assert_eq!(glob_matches("", "something"), None);
    }

    #[test]
    fn test_glob_specificity() {
        // More specific patterns have higher specificity
        let specific = glob_matches("claude-sonnet-4-5-*", "claude-sonnet-4-5-20250929");
        let general = glob_matches("claude-*", "claude-sonnet-4-5-20250929");

        assert!(specific.is_some());
        assert!(general.is_some());
        assert!(specific.unwrap() > general.unwrap());
        assert_eq!(specific.unwrap(), 18);
        assert_eq!(general.unwrap(), 7);
    }

    fn create_test_registry(models: Vec<Model>) -> ModelRegistry {
        ModelRegistry::new(
            models,
            FallbackModels::default(),
            vec![],
            TokenManager::new(vec!["test".to_string()]),
            600,
        )
    }

    #[test]
    fn test_find_model_by_alias_exact() {
        let models = vec![Model {
            name: "claude-sonnet-4-5".to_string(),
            aicore_model_name: None,
            aliases: vec!["claude-4-sonnet".to_string()],
        }];
        let registry = create_test_registry(models);

        let found = registry.find_model_by_alias("claude-4-sonnet");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "claude-sonnet-4-5");
    }

    #[test]
    fn test_find_model_by_alias_wildcard() {
        let models = vec![Model {
            name: "claude-sonnet-4-5".to_string(),
            aicore_model_name: None,
            aliases: vec!["claude-sonnet-4-5-*".to_string()],
        }];
        let registry = create_test_registry(models);

        let found = registry.find_model_by_alias("claude-sonnet-4-5-20250929");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "claude-sonnet-4-5");
    }

    #[test]
    fn test_find_model_by_alias_most_specific_wins() {
        let models = vec![
            Model {
                name: "claude-general".to_string(),
                aicore_model_name: None,
                aliases: vec!["claude-*".to_string()],
            },
            Model {
                name: "claude-sonnet-4-5".to_string(),
                aicore_model_name: None,
                aliases: vec!["claude-sonnet-4-5-*".to_string()],
            },
        ];
        let registry = create_test_registry(models);

        // Should match the more specific pattern
        let found = registry.find_model_by_alias("claude-sonnet-4-5-20250929");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "claude-sonnet-4-5");

        // Should match the general pattern for non-specific requests
        let found2 = registry.find_model_by_alias("claude-opus");
        assert!(found2.is_some());
        assert_eq!(found2.unwrap().name, "claude-general");
    }

    #[test]
    fn test_find_model_by_alias_no_match() {
        let models = vec![Model {
            name: "claude-sonnet-4-5".to_string(),
            aicore_model_name: None,
            aliases: vec!["claude-sonnet-4-5-*".to_string()],
        }];
        let registry = create_test_registry(models);

        let found = registry.find_model_by_alias("gpt-4o-mini");
        assert!(found.is_none());
    }

    #[test]
    fn test_find_model_by_alias_multiple_aliases() {
        let models = vec![Model {
            name: "claude-sonnet-4-5".to_string(),
            aicore_model_name: None,
            aliases: vec![
                "claude-sonnet-4-5-*".to_string(),
                "claude-4-sonnet".to_string(),
                "sonnet-4.5".to_string(),
            ],
        }];
        let registry = create_test_registry(models);

        // Test all aliases match the same model
        assert_eq!(
            registry
                .find_model_by_alias("claude-sonnet-4-5-20250929")
                .map(|m| &m.name),
            Some(&"claude-sonnet-4-5".to_string())
        );
        assert_eq!(
            registry
                .find_model_by_alias("claude-4-sonnet")
                .map(|m| &m.name),
            Some(&"claude-sonnet-4-5".to_string())
        );
        assert_eq!(
            registry.find_model_by_alias("sonnet-4.5").map(|m| &m.name),
            Some(&"claude-sonnet-4-5".to_string())
        );
    }
}
