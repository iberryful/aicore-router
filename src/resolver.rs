use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::client::AiCoreClient;
use crate::config::{Config, Model};

pub struct DeploymentResolver {
    client: AiCoreClient,
    resource_group: String,
    refresh_interval: Duration,
    resolved_models: Arc<RwLock<HashMap<String, String>>>,
    model_configs: Vec<Model>,
}

impl DeploymentResolver {
    pub fn new(config: &Config, client: AiCoreClient) -> Self {
        Self {
            client,
            resource_group: config.resource_group.clone(),
            refresh_interval: Duration::from_secs(config.refresh_interval_secs),
            resolved_models: Arc::clone(&config.resolved_models),
            model_configs: config.models.clone(),
        }
    }

    pub async fn start(&self) -> Result<()> {
        // Initial resolution
        self.refresh_deployments().await?;

        // Start background refresh task
        let resolver = self.clone();
        tokio::spawn(async move {
            resolver.background_refresh().await;
        });

        Ok(())
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

        for model_config in &self.model_configs {
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

impl Clone for DeploymentResolver {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            resource_group: self.resource_group.clone(),
            refresh_interval: self.refresh_interval,
            resolved_models: Arc::clone(&self.resolved_models),
            model_configs: self.model_configs.clone(),
        }
    }
}
