use anyhow::{Context, Result};
use clap::{Arg, Command};
use std::net::SocketAddr;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing_subscriber::{EnvFilter, fmt};

#[cfg(feature = "db")]
use crate::database::Database;
use crate::{
    balancer::LoadBalancer,
    commands::CommandHandler,
    config::Config,
    metrics::MetricsService,
    rate_limit::AuthRateLimiter,
    registry::ModelRegistry,
    routes::{AppState, create_router},
    token::TokenManager,
};

pub struct Cli;

impl Cli {
    pub async fn run() -> Result<()> {
        let matches = Self::build_command().get_matches();

        let config_path = matches.get_one::<String>("config").map(|s| s.as_str());
        #[allow(unused_mut)]
        let mut config = Config::load(config_path).context("Failed to load configuration")?;

        // Handle CLI commands
        if let Some(subcommand) = matches.subcommand() {
            let handler =
                CommandHandler::new(config.clone()).context("Failed to create command handler")?;

            match subcommand {
                ("resource-groups", _) => {
                    return handler.list_resource_groups().await;
                }
                ("deployments", deployments_matches) => {
                    let resource_group = deployments_matches
                        .get_one::<String>("resource-group")
                        .map(|s| s.as_str());
                    return handler.list_deployments(resource_group).await;
                }
                ("configure", configure_matches) => {
                    if let Some(("claude-code", _)) = configure_matches.subcommand() {
                        return handler.configure_claude_code();
                    }
                    if let Some(("opencode", _)) = configure_matches.subcommand() {
                        return handler.configure_opencode();
                    }
                    eprintln!(
                        "Unknown configure subcommand. Use 'acr configure claude-code' or 'acr configure opencode'"
                    );
                    std::process::exit(1);
                }
                ("diagnose", _) => {
                    return handler.diagnose(config_path);
                }
                #[cfg(feature = "db")]
                ("usage", usage_matches) => {
                    let api_key = usage_matches
                        .get_one::<String>("api-key")
                        .map(|s| s.as_str());
                    let daily = usage_matches.get_one::<u32>("daily").copied();
                    let weekly = usage_matches.get_one::<u32>("weekly").copied();
                    let monthly = usage_matches.get_one::<u32>("monthly").copied();
                    let show_cost = usage_matches.get_flag("cost");
                    return handler
                        .usage(api_key, daily, weekly, monthly, show_cost)
                        .await;
                }
                #[cfg(feature = "db")]
                ("logs", logs_matches) => {
                    if let Some(("clean", clean_matches)) = logs_matches.subcommand() {
                        let days = clean_matches.get_one::<u32>("days").copied();
                        return handler.logs_clean(days).await;
                    }
                    eprintln!("Unknown logs subcommand. Use 'acr logs clean'");
                    std::process::exit(1);
                }
                _ => {
                    eprintln!("Unknown command");
                    std::process::exit(1);
                }
            }
        }

        // Handle --log-requests flag: override config
        #[cfg(feature = "db")]
        if matches.get_flag("log-requests") {
            config.log_requests.enabled = true;
        }

        // Continue with server startup if no CLI command was provided
        Self::run_server(matches, config).await
    }

    fn build_command() -> Command {
        let cmd = Command::new("acr")
            .version(env!("CARGO_PKG_VERSION"))
            .about("AI Core Router - LLM API Proxy Service")
            .arg(
                Arg::new("bind")
                    .short('b')
                    .long("bind")
                    .value_name("ADDR")
                    .help("Bind address (e.g. 127.0.0.1, 0.0.0.0:9000)"),
            )
            .arg(
                Arg::new("config")
                    .short('c')
                    .long("config")
                    .value_name("FILE")
                    .help("Path to configuration file"),
            )
            .arg(
                Arg::new("log-level")
                    .short('l')
                    .long("log-level")
                    .value_name("LEVEL")
                    .help("Log level (trace, debug, info, warn, error)"),
            );

        #[cfg(feature = "tui")]
        let cmd = cmd.arg(
            Arg::new("tui")
                .long("tui")
                .help("Enable terminal UI dashboard")
                .action(clap::ArgAction::SetTrue),
        );

        #[cfg(feature = "db")]
        let cmd = cmd
            .arg(
                Arg::new("log-requests")
                    .long("log-requests")
                    .help("Enable request logging to SQLite database")
                    .action(clap::ArgAction::SetTrue),
            )
            .subcommand(
                Command::new("usage")
                    .about("Show token usage statistics from the request database")
                    .arg(
                        Arg::new("api-key")
                            .help("Filter usage to a specific API key")
                            .index(1),
                    )
                    .arg(
                        Arg::new("daily")
                            .short('D')
                            .long("daily")
                            .value_name("N")
                            .help("Show daily breakdown for past N days")
                            .value_parser(clap::value_parser!(u32)),
                    )
                    .arg(
                        Arg::new("weekly")
                            .short('W')
                            .long("weekly")
                            .value_name("N")
                            .help("Show weekly breakdown for past N weeks")
                            .value_parser(clap::value_parser!(u32)),
                    )
                    .arg(
                        Arg::new("monthly")
                            .short('M')
                            .long("monthly")
                            .value_name("N")
                            .help("Show monthly breakdown for past N months")
                            .value_parser(clap::value_parser!(u32)),
                    )
                    .arg(
                        Arg::new("cost")
                            .long("cost")
                            .help("Show estimated cost alongside token usage")
                            .action(clap::ArgAction::SetTrue),
                    ),
            )
            .subcommand(
                Command::new("logs")
                    .about("Manage request logs")
                    .subcommand(
                        Command::new("clean")
                            .about("Delete request logs older than N days")
                            .arg(
                                Arg::new("days")
                                    .long("days")
                                    .value_name("N")
                                    .help("Number of days to retain (default: from config)")
                                    .value_parser(clap::value_parser!(u32)),
                            ),
                    ),
            );

        cmd.subcommand(Command::new("resource-groups").about("List all resource groups"))
            .subcommand(
                Command::new("deployments").about("List deployments").arg(
                    Arg::new("resource-group")
                        .short('r')
                        .long("resource-group")
                        .value_name("RESOURCE_GROUP")
                        .help("Resource group to filter deployments"),
                ),
            )
            .subcommand(
                Command::new("configure")
                    .about("Configure coding tools to use this router")
                    .subcommand(
                        Command::new("claude-code")
                            .about("Auto-configure Claude Code to use this router"),
                    )
                    .subcommand(
                        Command::new("opencode")
                            .about("Auto-configure OpenCode to use this router"),
                    ),
            )
            .subcommand(
                Command::new("diagnose")
                    .about("Print diagnostic information about the router configuration"),
            )
    }

    async fn run_server(matches: clap::ArgMatches, mut config: Config) -> Result<()> {
        // Apply CLI overrides before tracing init
        if let Some(bind) = matches.get_one::<String>("bind") {
            config.bind = bind.clone();
        }
        if let Some(log_level) = matches.get_one::<String>("log-level") {
            config.log_level = log_level.clone();
        }

        // Initialize tracing
        let filter_directive = format!(
            "aicore_router={},acr={},info",
            config.log_level, config.log_level
        );
        let env_filter = EnvFilter::try_new(&filter_directive).with_context(|| {
            format!(
                "Invalid log_level '{}'. Valid options: trace, debug, info, warn, error",
                config.log_level
            )
        })?;

        #[cfg(feature = "tui")]
        let tui_log_tx = if matches.get_flag("tui") {
            let (tx, rx) = tokio::sync::mpsc::channel(1024);
            // TUI path: use custom layer
            use tracing_subscriber::layer::SubscriberExt;
            let tui_layer = crate::tui::TuiLogLayer::new(tx.clone());
            let subscriber = tracing_subscriber::registry()
                .with(env_filter)
                .with(tui_layer);
            tracing::subscriber::set_global_default(subscriber)
                .context("Failed to set tracing subscriber")?;
            Some((tx, rx))
        } else {
            fmt().with_env_filter(env_filter).init();
            None
        };

        #[cfg(not(feature = "tui"))]
        fmt().with_env_filter(env_filter).init();

        tracing::info!("Starting AI Core Router on {}", config.bind);
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
        let token_manager = TokenManager::new(config.api_key_strings());

        // Create load balancer with providers and configured strategy.
        // Construction fails fast when no enabled providers remain — the
        // binary refuses to start in a non-functional state.
        let load_balancer =
            LoadBalancer::new(config.providers.clone(), config.load_balancing.clone())
                .context("Failed to construct load balancer")?;
        tracing::info!("Load balancing strategy: {:?}", config.load_balancing);

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300)) // 5 min timeout for long LLM responses
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .context("Failed to build HTTP client")?;

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
        let _registry_handle = model_registry
            .start()
            .await
            .context("Failed to start model registry")?;

        // Create metrics service
        let metrics = MetricsService::new();

        // Create database for request logging
        #[cfg(feature = "db")]
        let database = if config.log_requests.enabled {
            tracing::info!("Request logging enabled: {}", config.log_requests.db_path);
            let db = Database::open(config.log_requests.db_path.clone().into())
                .await
                .context("Failed to open database")?;
            Some(db)
        } else {
            None
        };

        #[cfg(not(feature = "db"))]
        if config.log_requests.enabled {
            tracing::warn!(
                "log_requests enabled in config but 'db' feature not compiled; request logging unavailable"
            );
        }

        let rate_limiter = AuthRateLimiter::new();

        // Spawn lazy cleanup of old logs (after service is up)
        #[cfg(feature = "db")]
        if config.log_requests.enabled && config.log_requests.retention_days > 0 {
            let cleanup_db = database.clone();
            let retention_days = config.log_requests.retention_days;
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                if let Some(ref db) = cleanup_db {
                    match db.cleanup_old_requests(retention_days).await {
                        Ok(0) => {}
                        Ok(n) => tracing::info!(
                            "Cleaned up {} old log entries (>{} days)",
                            n,
                            retention_days
                        ),
                        Err(e) => tracing::warn!("Failed to clean up old logs: {}", e),
                    }
                }
            });
        }

        // Spawn rate limiter cleanup task (every 60 seconds)
        let cleanup_limiter = rate_limiter.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                cleanup_limiter.cleanup().await;
            }
        });

        // Create quota manager if enabled
        let quota_manager = if config.quotas.enabled {
            #[cfg(feature = "db")]
            let qm =
                crate::quota::QuotaManager::new(&config.api_keys, &config.quotas, database.clone());
            #[cfg(not(feature = "db"))]
            let qm = crate::quota::QuotaManager::new(&config.api_keys, &config.quotas);

            // Load baseline usage from requests table
            #[cfg(feature = "db")]
            if let Some(ref db) = database
                && let Err(e) = qm.load_baselines(db).await
            {
                tracing::warn!("Failed to load quota baselines from database: {}", e);
            }

            tracing::info!(
                "Token quotas enabled (daily: {}, monthly: {})",
                config
                    .quotas
                    .daily_token_limit
                    .map(|l| l.to_string())
                    .unwrap_or_else(|| "unlimited".to_string()),
                config
                    .quotas
                    .monthly_token_limit
                    .map(|l| l.to_string())
                    .unwrap_or_else(|| "unlimited".to_string()),
            );

            #[cfg(not(feature = "db"))]
            tracing::warn!(
                "Quotas running in-memory only (no 'db' feature); usage resets on restart"
            );

            #[cfg(feature = "db")]
            if !config.log_requests.enabled {
                tracing::warn!(
                    "Quotas running in-memory only (log_requests disabled); usage resets on restart"
                );
            }

            Some(qm)
        } else {
            None
        };

        // Build per-API-key request-rate limiter (separate from token quotas above).
        // Returns None if no requests_per_minute is configured anywhere.
        let request_limiter =
            crate::request_limiter::RequestLimiter::from_config(&config.api_keys, &config.quotas)
                .map(std::sync::Arc::new);
        if let Some(ref rl) = request_limiter {
            tracing::info!(
                "Per-key request rate limiting enabled (default: {})",
                config
                    .quotas
                    .requests_per_minute
                    .map(|n| format!("{n} req/min"))
                    .unwrap_or_else(|| "unlimited".to_string()),
            );
            let _ = rl; // suppress unused-variable warning when feature combos exclude usage
        }

        let state = AppState {
            config: config.clone(),
            model_registry: model_registry.clone(),
            token_manager,
            load_balancer,
            client,
            metrics: metrics.clone(),
            #[cfg(feature = "db")]
            database,
            rate_limiter,
            quota_manager: quota_manager.clone(),
            request_limiter,
        };

        let app = create_router(state)
            .layer(axum::extract::DefaultBodyLimit::max(10 * 1024 * 1024)) // 10 MB
            .layer(CorsLayer::permissive())
            .layer(TraceLayer::new_for_http());

        let addr = crate::config::parse_bind_address(&config.bind)?;
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .context("Failed to bind to address")?;

        tracing::info!("Server listening on {}", addr);

        // TUI mode: run server in background, TUI in foreground
        #[cfg(feature = "tui")]
        if let Some((_tx, rx)) = tui_log_tx {
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
            let tui_quota_manager = quota_manager.clone();

            tokio::spawn(async move {
                axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                    tracing::info!("TUI exited, shutting down server gracefully...");
                })
                .await
                .inspect_err(|e| tracing::error!("Server error during TUI shutdown: {}", e))
                .ok();
            });

            let api_keys = config.api_key_strings();

            let mut tui_app = crate::tui::TuiApp::new(
                format!("http://{}", addr),
                api_keys,
                tui_quota_manager,
                model_registry.clone(),
                config.models.clone(),
                metrics,
                rx,
            );

            tui_app.run()?;
            let _ = shutdown_tx.send(());
            // Give the server a moment to finish in-flight requests
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            return Ok(());
        }

        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(Self::shutdown_signal())
        .await
        .context("Server error")?;

        tracing::info!("Server shut down gracefully");
        Ok(())
    }

    async fn shutdown_signal() {
        let ctrl_c = async {
            if let Err(e) = tokio::signal::ctrl_c().await {
                tracing::error!("Failed to listen for Ctrl+C: {}", e);
            }
        };

        #[cfg(unix)]
        let terminate = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut sig) => {
                    sig.recv().await;
                }
                Err(e) => {
                    tracing::error!("Failed to listen for SIGTERM: {}", e);
                    std::future::pending::<()>().await;
                }
            }
        };

        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            _ = ctrl_c => { tracing::info!("Received Ctrl+C, shutting down..."); }
            _ = terminate => { tracing::info!("Received SIGTERM, shutting down..."); }
        }
    }
}
