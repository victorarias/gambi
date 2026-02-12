mod auth;
mod cli;
mod config;
mod execution;
mod fixture_progress;
mod logging;
mod namespacing;
mod server;
mod upstream;

use anyhow::Result;
use clap::Parser;
use cli::Cli;
use config::ConfigStore;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let store = ConfigStore::new_default()?;
    cli::run(cli, store).await
}

fn init_tracing() {
    let logs = logging::init_global_log_buffer(1000);
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .compact()
        .with_writer(logging::TeeMakeWriter::new(logs))
        .init();
}
