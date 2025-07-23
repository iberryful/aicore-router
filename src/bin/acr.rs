use anyhow::{Context, Result};
use clap::{Arg, Command};
use std::net::SocketAddr;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing_subscriber::{EnvFilter, fmt};

use aicore_router::{
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
        .get_matches();

    let config_path = matches.get_one::<String>("config").map(|s| s.as_str());
    let mut config = Config::load(config_path).context("Failed to load configuration")?;

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
