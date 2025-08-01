use crate::{client::AiCoreClient, config::Config};
use anyhow::Result;

pub struct CommandHandler {
    client: AiCoreClient,
    config: Config,
}

impl CommandHandler {
    pub fn new(config: Config) -> Self {
        let client = AiCoreClient::from_config(config.clone());
        Self { client, config }
    }

    pub async fn list_resource_groups(&self) -> Result<()> {
        println!("Fetching resource groups...");
        let resource_groups = self.client.list_resource_groups().await?;

        if resource_groups.resources.is_empty() {
            println!("No resource groups found.");
            return Ok(());
        }

        println!("\nResource Groups ({} total):", resource_groups.count);
        println!(
            "{:<30} {:<20} {:<15} {:<20}",
            "RESOURCE GROUP ID", "STATUS", "ZONE ID", "CREATED AT"
        );
        println!("{}", "-".repeat(90));

        for rg in &resource_groups.resources {
            println!(
                "{:<30} {:<20} {:<15} {:<20}",
                rg.resource_group_id,
                rg.status,
                rg.zone_id.as_deref().unwrap_or("N/A"),
                rg.created_at.split('T').next().unwrap_or(&rg.created_at)
            );
        }

        Ok(())
    }

    pub async fn list_deployments(&self, resource_group: Option<&str>) -> Result<()> {
        let rg_name = resource_group.unwrap_or(&self.config.resource_group);
        println!("Fetching deployments for resource group '{rg_name}'...");

        let deployments = self.client.list_deployments(resource_group).await?;

        if deployments.resources.is_empty() {
            println!("No deployments found in resource group '{rg_name}'.");
            return Ok(());
        }

        println!("\nDeployments ({} total):", deployments.count);
        println!(
            "{:<18} {:<12} {:<25} {:<20} {:<20}",
            "ID", "STATUS", "CONFIG NAME", "MODEL", "START TIME"
        );
        println!("{}", "-".repeat(100));

        for deployment in &deployments.resources {
            let (model_name, model_version) = deployment.get_model_info();
            let model_display = match (model_name, model_version) {
                (Some(name), Some(version)) => format!("{name}:{version}"),
                (Some(name), None) => name,
                _ => "N/A".to_string(),
            };

            println!(
                "{:<18} {:<12} {:<25} {:<20} {:<20}",
                &deployment.id[..std::cmp::min(deployment.id.len(), 16)],
                deployment.status,
                deployment.configuration_name.as_deref().unwrap_or("N/A"),
                &model_display[..std::cmp::min(model_display.len(), 18)],
                deployment
                    .start_time
                    .as_deref()
                    .and_then(|t| t.split('T').next())
                    .unwrap_or("N/A")
            );
        }

        Ok(())
    }
}
