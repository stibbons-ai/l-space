//! The Bridge binary: loads the operator's configuration and serves the App API.

mod app_api;
mod config;
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

    let store = match SqliteStore::open(&config.database) {
        Ok(store) => store,
        Err(error) => {
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
    let router = api::server::router(Arc::new(AppApi::new(config.library_servers, store)));

    tracing::info!(%addr, "listening");
    if let Err(error) = axum::serve(listener, router).await {
        tracing::error!(%error, "server stopped");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
