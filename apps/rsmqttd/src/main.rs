#![forbid(unsafe_code)]
#![warn(clippy::default_trait_access)]

mod api;
mod client_loop;
mod config;
mod defaults;
mod error;
mod filter;
mod message;
mod metrics;
mod server;
mod storage;
mod sys_topics;
mod ws_transport;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use structopt::StructOpt;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use config::{Config, StorageConfig};
use server::ServerState;
use storage::Storage;

const DEFAULT_CONFIG_FILENAME: &str = ".rsmqttd";

#[derive(StructOpt)]
struct Options {
    /// Path of the config file
    pub config: Option<String>,
}

fn init_tracing() {
    tracing_subscriber::registry()
        .with(fmt::layer().compact().with_target(false))
        .with(
            EnvFilter::try_from_default_env()
                .or_else(|_| EnvFilter::try_new("info"))
                .unwrap(),
        )
        .init();
}

fn create_storage(config: &StorageConfig) -> Result<Box<dyn Storage>> {
    match &*config.r#type {
        "memory" => Ok(Box::new(storage::memory::StorageMemory::default())),
        _ => anyhow::bail!("unsupported storage type: {}", config.r#type),
    }
}

async fn run() -> Result<()> {
    let options: Options = Options::from_args();

    let config_filename = match options.config {
        Some(config_filename) => Some(PathBuf::from(config_filename)),
        None => dirs::home_dir()
            .map(|home_dir| home_dir.join(DEFAULT_CONFIG_FILENAME))
            .filter(|path| path.exists()),
    };

    let config = if let Some(config_filename) = config_filename {
        tracing::info!(filename = %config_filename.display(), "load config file");

        toml::from_str::<Config>(
            &std::fs::read_to_string(&config_filename)
                .with_context(|| format!("load config file '{}'.", config_filename.display()))?,
        )
        .with_context(|| format!("parse config file '{}'.", config_filename.display()))?
    } else {
        tracing::info!("use the default config");
        Config::default()
    };

    tracing::info!(r#type = %config.storage.r#type, "create storage");
    let storage = create_storage(&config.storage)?;
    let state = ServerState::new(config, storage).await?;

    if state.config.server.sys_update_interval > 0 {
        tokio::spawn(sys_topics::update_loop(
            state.clone(),
            Duration::from_secs(state.config.server.sys_update_interval),
        ));
    }

    server::run(state).await
}

#[tokio::main]
async fn main() {
    init_tracing();

    if let Err(err) = run().await {
        tracing::error!(
            error = %err,
            "failed to start server",
        );
    }
}
