use anyhow::Result;

use crate::{
    client::{AiCoreClient, DeploymentList, ResourceGroupList},
    config::Config,
};

pub struct CliClient {
    client: AiCoreClient,
}

impl CliClient {
    pub fn new(config: Config) -> Self {
        Self {
            client: AiCoreClient::from_config(config),
        }
    }

    pub async fn list_resource_groups(&self) -> Result<ResourceGroupList> {
        self.client.list_resource_groups().await
    }

    pub async fn list_deployments(&self, resource_group: Option<&str>) -> Result<DeploymentList> {
        self.client.list_deployments(resource_group).await
    }
}
