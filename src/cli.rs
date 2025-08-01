use anyhow::{Context, Result};
use clap::{Arg, Command};
use std::net::SocketAddr;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing_subscriber::{EnvFilter, fmt};

use crate::{
    client::AiCoreClient,
    commands::CommandHandler,
    config::Config,
    resolver::DeploymentResolver,
    routes::{AppState, create_router},
    token::{OAuthConfig, TokenManager},
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

        // Create AI Core client for deployment resolution
        let aicore_client = AiCoreClient::from_config(config.clone());

        // Create and start deployment resolver
        tracing::info!(
            "Initializing deployment resolver with refresh interval: {}s",
            config.refresh_interval_secs
        );
        let resolver = DeploymentResolver::new(&config, aicore_client);
        resolver
            .start()
            .await
            .context("Failed to start deployment resolver")?;

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
}
