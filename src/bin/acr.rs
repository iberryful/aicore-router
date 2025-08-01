use aicore_router::cli::Cli;
use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    Cli::run().await
}
