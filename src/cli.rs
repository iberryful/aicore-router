use anyhow::{Context, Result};
use clap::{Arg, Command};
use std::net::SocketAddr;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing_subscriber::{EnvFilter, fmt};
use axum::extract::DefaultBodyLimit;

use crate::{
    balancer::LoadBalancer,
    commands::CommandHandler,
    config::Config,
    registry::ModelRegistry,
    routes::{AppState, create_router},
    token::TokenManager,
};

pub struct Cli;

impl Cli {
    pub async fn run() -> Result<()> {
        let matches = Self::build_command().get_matches();

        let config_path = matches.get_one::<String>("config").map(|s| s.as_str());
        let config = Config::load(config_path).context("Failed to load configuration")?;

        // Handle CLI commands
        if let Some(subcommand) = matches.subcommand() {
            let handler = CommandHandler::new(config);

            match subcommand {
                ("resource-group", resource_group_matches) => {
                    if let Some(("list", _)) = resource_group_matches.subcommand() {
                        return handler.list_resource_groups().await;
                    } else {
                        eprintln!(
                            "Unknown resource-group subcommand. Use 'acr resource-group list'"
                        );
                        std::process::exit(1);
                    }
                }
                ("deployments", deployments_matches) => {
                    if let Some(("list", list_matches)) = deployments_matches.subcommand() {
                        let resource_group = list_matches
                            .get_one::<String>("resource-group")
                            .map(|s| s.as_str());
                        return handler.list_deployments(resource_group).await;
                    } else {
                        eprintln!("Unknown deployments subcommand. Use 'acr deployments list'");
                        std::process::exit(1);
                    }
                }
                _ => {
                    eprintln!("Unknown command");
                    std::process::exit(1);
                }
            }
        }

        // Continue with server startup if no CLI command was provided
        Self::run_server(matches, config).await
    }

    fn build_command() -> Command {
        Command::new("acr")
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
    }

    async fn run_server(matches: clap::ArgMatches, mut config: Config) -> Result<()> {
        // Initialize tracing with the configured log level
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
        tracing::info!("Configured providers: {}", config.providers.len());
        for provider in &config.providers {
            tracing::info!(
                "  Provider '{}': {} (resource_group: {}, enabled: {})",
                provider.name,
                provider.genai_api_url,
                provider.resource_group,
                provider.enabled
            );
        }
        tracing::info!("Configured API keys: {}", config.api_keys.len());

        // Create token manager with API keys
        let token_manager = TokenManager::new(config.api_keys.clone());

        // Create load balancer with providers and configured strategy
        let load_balancer =
            LoadBalancer::new(config.providers.clone(), config.load_balancing.clone());
        tracing::info!("Load balancing strategy: {:?}", config.load_balancing);

        if load_balancer.is_empty() {
            return Err(anyhow::anyhow!("No enabled providers configured"));
        }

        let client = reqwest::Client::new();

        // Create and start model registry
        tracing::info!(
            "Initializing model registry with refresh interval: {}s",
            config.refresh_interval_secs
        );
        let model_registry = ModelRegistry::new(
            config.models.clone(),
            config.fallback_models.clone(),
            config.providers.clone(),
            token_manager.clone(),
            config.refresh_interval_secs,
        );
        model_registry
            .start()
            .await
            .context("Failed to start model registry")?;

        let state = AppState {
            config: config.clone(),
            model_registry,
            token_manager,
            load_balancer,
            client,
        };

        // Build base app with common layers
        let base_app = create_router(state)
            .layer(CorsLayer::permissive())
            .layer(TraceLayer::new_for_http());

        // Apply configurable body limit if provided; otherwise use Axum's default (2 MiB for Json)
        let app = match config.request_body_limit {
            Some(limit) => {
                tracing::info!("Request body limit set to {} bytes", limit);
                base_app.layer(DefaultBodyLimit::max(limit))
            }
            None => {
                tracing::info!("Request body limit not set; using Axum default (2 MiB for Json)");
                base_app
            }
        };

        let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .context("Failed to bind to address")?;

        tracing::info!("Server listening on {}", addr);

        axum::serve(listener, app).await.context("Server error")?;

        Ok(())
    }
}
