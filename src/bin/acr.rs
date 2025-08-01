use anyhow::{Context, Result};
use clap::{Arg, Command};
use std::net::SocketAddr;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing_subscriber::{EnvFilter, fmt};

use aicore_router::{
    cli::CliClient,
    config::Config,
    routes::{AppState, create_router},
    token::{OAuthConfig, TokenManager},
};

#[tokio::main]
async fn main() -> Result<()> {
    let matches = Command::new("acr")
        .version(env!("CARGO_PKG_VERSION"))
        .about("AI Core Router - LLM API Proxy Service")
        .arg(
            Arg::new("port")
                .short('p')
                .long("port")
                .value_name("PORT")
                .help("Port to bind the server to")
                .value_parser(clap::value_parser!(u16)),
        )
        .arg(
            Arg::new("config")
                .short('c')
                .long("config")
                .value_name("FILE")
                .help("Path to configuration file"),
        )
        .subcommand(
            Command::new("resource-group")
                .about("Manage resource groups")
                .subcommand(Command::new("list").about("List all resource groups")),
        )
        .subcommand(
            Command::new("deployments")
                .about("Manage deployments")
                .subcommand(
                    Command::new("list").about("List deployments").arg(
                        Arg::new("resource-group")
                            .short('r')
                            .long("resource-group")
                            .value_name("RESOURCE_GROUP")
                            .help("Resource group to filter deployments"),
                    ),
                ),
        )
        .get_matches();

    let config_path = matches.get_one::<String>("config").map(|s| s.as_str());
    let config = Config::load(config_path).context("Failed to load configuration")?;

    // Handle CLI commands
    match matches.subcommand() {
        Some(("resource-group", resource_group_matches)) => {
            match resource_group_matches.subcommand() {
                Some(("list", _)) => {
                    return handle_resource_group_list(&config).await;
                }
                _ => {
                    eprintln!("Unknown resource-group subcommand. Use 'acr resource-group list'");
                    std::process::exit(1);
                }
            }
        }
        Some(("deployments", deployments_matches)) => match deployments_matches.subcommand() {
            Some(("list", list_matches)) => {
                let resource_group = list_matches
                    .get_one::<String>("resource-group")
                    .map(|s| s.as_str());
                return handle_deployments_list(&config, resource_group).await;
            }
            _ => {
                eprintln!("Unknown deployments subcommand. Use 'acr deployments list'");
                std::process::exit(1);
            }
        },
        None => {
            // No subcommand provided, run as server
        }
        _ => {
            eprintln!("Unknown command");
            std::process::exit(1);
        }
    }

    // Continue with server startup if no CLI command was provided
    run_server(matches, config).await
}

async fn handle_resource_group_list(config: &Config) -> Result<()> {
    let client = CliClient::new(config.clone());

    println!("Fetching resource groups...");
    let resource_groups = client.list_resource_groups().await?;

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

async fn handle_deployments_list(config: &Config, resource_group: Option<&str>) -> Result<()> {
    let client = CliClient::new(config.clone());

    let rg_name = resource_group.unwrap_or(&config.resource_group);
    println!("Fetching deployments for resource group '{rg_name}'...");

    let deployments = client.list_deployments(resource_group).await?;

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

async fn run_server(matches: clap::ArgMatches, mut config: Config) -> Result<()> {
    // Initialize tracing with the configured log level
    // Only show debug logs from our application code, limit external libraries to info
    let filter_directive = format!(
        "aicore_router={},acr={},info",
        config.log_level, config.log_level
    );
    let env_filter =
        EnvFilter::try_new(&filter_directive).unwrap_or_else(|_| EnvFilter::new("info"));

    fmt().with_env_filter(env_filter).init();

    if let Some(port) = matches.get_one::<u16>("port") {
        config.port = *port;
    }

    tracing::info!("Starting AI Core Router on port {}", config.port);
    tracing::info!("GenAI API URL: {}", config.genai_api_url);
    tracing::info!("UAA Token URL: {}", config.uaa_token_url);
    tracing::info!("UAA Client ID: {}", config.uaa_client_id);

    let token_manager = TokenManager::with_oauth_config(OAuthConfig {
        api_key: config.api_key.clone(),
        token_url: config.uaa_token_url.clone(),
        client_id: config.uaa_client_id.clone(),
        client_secret: config.uaa_client_secret.clone(),
    });
    let client = reqwest::Client::new();

    let state = AppState {
        config: config.clone(),
        token_manager,
        client,
    };

    let app = create_router(state)
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http());

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("Failed to bind to address")?;

    tracing::info!("Server listening on {}", addr);

    axum::serve(listener, app).await.context("Server error")?;

    Ok(())
}
