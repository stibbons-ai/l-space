//! The Bridge binary: loads the operator's configuration and encryption key, searches for Sonos
//! players, and serves the App API.

mod app_api;
mod browse;
mod config;
mod encryption_key;
mod telemetry;

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use store_sqlite::SqliteStore;
use tokio::net::TcpListener;

use crate::app_api::AppApi;
use crate::config::Config;

#[derive(Debug, Parser)]
#[command(version, about = "The L-Space Bridge (Clacks)")]
struct Args {
    /// Path to the configuration file (TOML).
    #[arg(long, env = "BRIDGE_CONFIG")]
    config: PathBuf,
}

/// `error` followed by each error that caused it, joined by `: `, for a log line.
pub(crate) fn error_chain(error: &dyn std::error::Error) -> String {
    let mut chain = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        chain.push_str(": ");
        chain.push_str(&cause.to_string());
        source = cause.source();
    }
    chain
}

/// Binds the App API's address, resolving port 0 to the port actually bound.
async fn listen(addr: SocketAddr) -> io::Result<(TcpListener, SocketAddr)> {
    let listener = TcpListener::bind(addr).await?;
    let addr = listener.local_addr()?;
    Ok((listener, addr))
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    telemetry::init();

    let config = match Config::load(&args.config) {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(config = %args.config.display(), %error, "invalid configuration");
            return ExitCode::FAILURE;
        }
    };

    let key = match encryption_key::load() {
        Ok(key) => key,
        Err(error) => {
            tracing::error!(%error, "cannot load the encryption key");
            return ExitCode::FAILURE;
        }
    };

    let store = match SqliteStore::open(&config.database, key) {
        Ok(store) => store,
        Err(error) => {
            let error = error_chain(&error);
            tracing::error!(database = %config.database.display(), %error, "cannot open the database");
            return ExitCode::FAILURE;
        }
    };

    let (listener, addr) = match listen(config.listen).await {
        Ok(bound) => bound,
        Err(error) => {
            tracing::error!(listen = %config.listen, %error, "cannot listen");
            return ExitCode::FAILURE;
        }
    };
    let discovery_address = config.sonos.discovery_address;
    tracing::info!(%discovery_address, "searching for Sonos players");
    let sonos = sonos::Upnp::start(discovery_address, Arc::new(clock::SystemClock));
    let app_api = AppApi::new(config.library_servers, store, Arc::new(sonos));
    let router = api::server::router(Arc::new(app_api));

    tracing::info!(%addr, "listening");
    if let Err(error) = axum::serve(listener, router).await {
        tracing::error!(%error, "server stopped");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
