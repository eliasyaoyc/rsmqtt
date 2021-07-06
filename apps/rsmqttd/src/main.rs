#![forbid(unsafe_code)]
#![warn(clippy::default_trait_access)]

mod acl;
mod api;
mod config;
mod server;
mod ws_transport;

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_yaml::Value;
use service::auth::{Auth, BasicAuth};
use service::storage::{MemoryStorage, Storage};
use service::ServiceState;
use structopt::StructOpt;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use config::Config;

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

fn create_storage(config: &Value) -> Result<Box<dyn Storage>> {
    anyhow::ensure!(
        config.is_mapping(),
        "invalid storage config, expect mapping"
    );

    let storage_type = match config.get("type") {
        Some(Value::String(ty)) => ty.as_str(),
        Some(_) => anyhow::bail!("invalid storage type, expect string"),
        None => "memory",
    };

    tracing::info!(r#type = storage_type, "create storage");

    match storage_type {
        "memory" => Ok(Box::new(MemoryStorage::default())),
        _ => anyhow::bail!("unsupported storage type: {}", storage_type),
    }
}

fn create_auth(config: &Value) -> Result<Option<Box<dyn Auth>>> {
    if config.is_null() {
        return Ok(None);
    }

    anyhow::ensure!(config.is_mapping(), "invalid auth config, expect mapping");

    let auth_type = match config.get("type") {
        Some(Value::String(ty)) => ty.as_str(),
        Some(_) => anyhow::bail!("invalid storage type, expect string"),
        None => return Ok(None),
    };

    match auth_type {
        "basic" => Ok(Some(Box::new(BasicAuth::try_new(config)?))),
        _ => anyhow::bail!("unsupported auth type: {}", auth_type),
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

        serde_yaml::from_str::<Config>(
            &std::fs::read_to_string(&config_filename)
                .with_context(|| format!("load config file '{}'.", config_filename.display()))?,
        )
        .with_context(|| format!("parse config file '{}'.", config_filename.display()))?
    } else {
        tracing::info!("use the default config");
        Config::default()
    };

    let storage = create_storage(&config.storage)?;
    let auth = create_auth(&config.auth)?;
    let state = ServiceState::try_new(config.service, storage, auth).await?;

    tokio::spawn(service::sys_topics_update_loop(state.clone()));
    server::run(state, config.network).await
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
