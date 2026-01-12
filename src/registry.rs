use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::client::AiCoreClient;
use crate::config::{FallbackModels, Model};

/// Runtime model registry that manages resolved deployment IDs and handles resolution
#[derive(Debug, Clone)]
pub struct ModelRegistry {
    /// Resolved model name to deployment ID mappings
    resolved_models: Arc<RwLock<HashMap<String, String>>>,
    /// Original model configurations from config file
    config_models: Vec<Model>,
    /// Fallback models configuration for each family
    fallback_models: FallbackModels,
    /// AI Core client for fetching deployments
    client: AiCoreClient,
    /// Resource group for deployment queries
    resource_group: String,
    /// Refresh interval for background updates
    refresh_interval: Duration,
}

impl ModelRegistry {
    /// Create a new model registry
    pub fn new(
        config_models: Vec<Model>,
        fallback_models: FallbackModels,
        client: AiCoreClient,
        resource_group: String,
        refresh_interval_secs: u64,
    ) -> Self {
        Self {
            resolved_models: Arc::new(RwLock::new(HashMap::new())),
            config_models,
            fallback_models,
            client,
            resource_group,
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

    /// Get deployment ID for a resolved model
    pub async fn get_deployment_id(&self, model_name: &str) -> Option<String> {
        let resolved = self.resolved_models.read().await;
        resolved.get(model_name).cloned()
    }

    /// Get all available (resolved) model names
    pub async fn get_available_models(&self) -> Vec<String> {
        let resolved = self.resolved_models.read().await;
        let mut models: Vec<String> = resolved.keys().cloned().collect();
        models.sort();
        models
    }

    /// Check if a model is available (has been resolved)
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
        info!("Refreshing deployment mappings...");

        // Get all running deployments
        let aicore_deployments = self
            .client
            .build_model_to_deployment_mapping(Some(&self.resource_group))
            .await?;

        let mut resolved = HashMap::new();

        for model_config in &self.config_models {
            if let Some(deployment_id) = &model_config.deployment_id {
                // Direct deployment ID mapping
                resolved.insert(model_config.name.clone(), deployment_id.clone());
                info!(
                    "Model '{}' -> deployment_id: {} (direct)",
                    model_config.name, deployment_id
                );
            } else {
                // Use aicore_model_name if specified, otherwise use the model name itself
                let aicore_model_name = model_config
                    .aicore_model_name
                    .as_ref()
                    .unwrap_or(&model_config.name);

                // Resolve from AI Core model name
                if let Some(deployment_id) = aicore_deployments.get(aicore_model_name) {
                    resolved.insert(model_config.name.clone(), deployment_id.clone());
                    info!(
                        "Model '{}' -> aicore_model_name: '{}' -> deployment_id: {}",
                        model_config.name, aicore_model_name, deployment_id
                    );
                } else {
                    warn!(
                        "Model '{}' -> aicore_model_name: '{}' -> no running deployment found",
                        model_config.name, aicore_model_name
                    );
                }
            }
        }

        let resolved_count = resolved.len();

        // Update the resolved models
        {
            let mut resolved_models = self.resolved_models.write().await;
            *resolved_models = resolved;
        }

        info!(
            "Deployment refresh complete: {} models resolved",
            resolved_count
        );

        Ok(())
    }
}
