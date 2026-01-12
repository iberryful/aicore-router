use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;

use crate::{
    config::Config,
    token::{OAuthConfig, TokenManager},
};

#[derive(Debug, Deserialize)]
pub struct ResourceGroup {
    #[serde(rename = "resourceGroupId")]
    pub resource_group_id: String,
    #[serde(rename = "tenantId")]
    pub tenant_id: String,
    #[serde(rename = "zoneId")]
    pub zone_id: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    pub status: String,
    #[serde(rename = "statusMessage")]
    pub status_message: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ResourceGroupList {
    pub count: i32,
    pub resources: Vec<ResourceGroup>,
}

#[derive(Debug, Deserialize)]
pub struct DeploymentDetails {
    pub resources: Option<serde_json::Value>,
    pub scaling: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct Deployment {
    pub id: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "modifiedAt")]
    pub modified_at: String,
    pub status: String,
    pub details: Option<DeploymentDetails>,
    #[serde(rename = "scenarioId")]
    pub scenario_id: String,
    #[serde(rename = "configurationId")]
    pub configuration_id: String,
    #[serde(rename = "latestRunningConfigurationId")]
    pub latest_running_configuration_id: Option<String>,
    #[serde(rename = "lastOperation")]
    pub last_operation: Option<String>,
    #[serde(rename = "targetStatus")]
    pub target_status: Option<String>,
    #[serde(rename = "submissionTime")]
    pub submission_time: Option<String>,
    #[serde(rename = "startTime")]
    pub start_time: Option<String>,
    #[serde(rename = "configurationName")]
    pub configuration_name: Option<String>,
    #[serde(rename = "deploymentUrl")]
    pub deployment_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DeploymentList {
    pub count: i32,
    pub resources: Vec<Deployment>,
}

impl Deployment {
    pub fn get_model_info(&self) -> (Option<String>, Option<String>) {
        if let Some(details) = &self.details
            && let Some(resources) = &details.resources
            && let Some(backend_details) = resources.get("backendDetails")
            && let Some(model) = backend_details.get("model")
        {
            let name = model
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let version = model
                .get("version")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            return (name, version);
        }
        (None, None)
    }

    pub fn get_aicore_model_name(&self) -> Option<String> {
        if let Some(details) = &self.details
            && let Some(resources) = &details.resources
            && let Some(backend_details) = resources.get("backendDetails")
            && let Some(model) = backend_details.get("model")
        {
            return model
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
        }
        None
    }
}

#[derive(Debug, Clone)]
pub struct AiCoreClientConfig {
    pub genai_api_url: String,
    pub resource_group: String,
    pub oauth_config: OAuthConfig,
}

impl From<Config> for AiCoreClientConfig {
    fn from(config: Config) -> Self {
        Self {
            genai_api_url: config.genai_api_url,
            resource_group: config.resource_group,
            oauth_config: OAuthConfig {
                api_keys: config.api_keys,
                token_url: config.uaa_token_url,
                client_id: config.uaa_client_id,
                client_secret: config.uaa_client_secret,
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct AiCoreClient {
    client: Client,
    config: AiCoreClientConfig,
    token_manager: TokenManager,
}

impl AiCoreClient {
    pub fn new(config: AiCoreClientConfig) -> Self {
        let token_manager = TokenManager::with_oauth_config(config.oauth_config.clone());

        Self {
            client: Client::new(),
            config,
            token_manager,
        }
    }

    pub fn from_config(config: Config) -> Self {
        Self::new(config.into())
    }

    async fn get_token(&self) -> Result<String> {
        // Use the first api_key for internal API calls
        let api_key = self
            .config
            .oauth_config
            .api_keys
            .first()
            .ok_or_else(|| anyhow::anyhow!("No API keys configured"))?;

        self.token_manager
            .get_token(api_key)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Failed to get authentication token"))
    }

    pub async fn list_resource_groups(&self) -> Result<ResourceGroupList> {
        let token = self.get_token().await?;
        let url = format!("{}/v2/admin/resourceGroups", self.config.genai_api_url);

        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .send()
            .await
            .context("Failed to request resource groups")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "Failed to list resource groups: {} - {}",
                status,
                text
            ));
        }

        let resource_groups: ResourceGroupList = response
            .json()
            .await
            .context("Failed to parse resource groups response")?;

        Ok(resource_groups)
    }

    pub async fn list_deployments(&self, resource_group: Option<&str>) -> Result<DeploymentList> {
        let token = self.get_token().await?;
        let url = format!("{}/v2/lm/deployments", self.config.genai_api_url);

        let mut request = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json");

        let rg = resource_group.unwrap_or(&self.config.resource_group);
        request = request.header("AI-Resource-Group", rg);

        let response = request
            .send()
            .await
            .context("Failed to request deployments")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "Failed to list deployments: {} - {}",
                status,
                text
            ));
        }

        let deployments: DeploymentList = response
            .json()
            .await
            .context("Failed to parse deployments response")?;

        Ok(deployments)
    }

    pub async fn get_deployment(
        &self,
        deployment_id: &str,
        resource_group: Option<&str>,
    ) -> Result<Deployment> {
        let token = self.get_token().await?;
        let url = format!(
            "{}/v2/lm/deployments/{}",
            self.config.genai_api_url, deployment_id
        );

        let mut request = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json");

        let rg = resource_group.unwrap_or(&self.config.resource_group);
        request = request.header("AI-Resource-Group", rg);

        let response = request
            .send()
            .await
            .context("Failed to request deployment")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "Failed to get deployment: {} - {}",
                status,
                text
            ));
        }

        let deployment: Deployment = response
            .json()
            .await
            .context("Failed to parse deployment response")?;

        Ok(deployment)
    }

    pub fn get_config(&self) -> &AiCoreClientConfig {
        &self.config
    }

    pub fn get_client(&self) -> &Client {
        &self.client
    }

    pub async fn build_model_to_deployment_mapping(
        &self,
        resource_group: Option<&str>,
    ) -> Result<std::collections::HashMap<String, String>> {
        let deployments = self.list_deployments(resource_group).await?;

        let mut mapping = std::collections::HashMap::new();

        for deployment in &deployments.resources {
            if deployment.status == "RUNNING"
                && let Some(model_name) = deployment.get_aicore_model_name()
            {
                mapping.insert(model_name, deployment.id.clone());
            }
        }

        Ok(mapping)
    }
}
