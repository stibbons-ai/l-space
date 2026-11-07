//! The Bridge binary: loads the operator's configuration and encryption key, searches for Sonos
//! players, and serves the App API and the media streams Groups play.

mod app_api;
mod browse;
mod config;
mod encryption_key;
mod shutdown;
mod stream;
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

fn main() -> ExitCode {
    let args = Args::parse();
    telemetry::init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("the Tokio runtime builds");
    let served = runtime.block_on(run(args));
    // Cancels the tasks left (connections still open after the drain, detached Handoff starts)
    // and gives an SQLite transaction already on a blocking thread up to
    // `shutdown::RUNTIME_SHUTDOWN` to commit; the store closes with the last `Arc`, unless a
    // transaction outlives that wait (see `shutdown::stop_runtime`).
    shutdown::stop_runtime(runtime);
    match served {
        Ok(()) => {
            tracing::info!("stopped");
            ExitCode::SUCCESS
        }
        Err(failed) => failed,
    }
}

/// Starts the Bridge and serves until a signal asks it to stop. Fails with the exit code when
/// it cannot start, or if the server fails while serving (axum 0.8's graceful server never does;
/// the path guards against a change there). A later slice whose teardown needs the App API
/// (final Progress, for example) keeps an `Arc<AppApi>` clone before building the router and
/// runs its teardown after `shutdown::serve` returns; `main` then stops the runtime.
async fn run(args: Args) -> Result<(), ExitCode> {
    let signals = match shutdown::Signals::install() {
        Ok(signals) => signals,
        Err(error) => {
            tracing::error!(%error, "cannot handle signals");
            return Err(ExitCode::FAILURE);
        }
    };

    let config = match Config::load(&args.config) {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(config = %args.config.display(), %error, "invalid configuration");
            return Err(ExitCode::FAILURE);
        }
    };

    let key = match encryption_key::load() {
        Ok(key) => key,
        Err(error) => {
            tracing::error!(%error, "cannot load the encryption key");
            return Err(ExitCode::FAILURE);
        }
    };

    let store = match SqliteStore::open(&config.database, key) {
        Ok(store) => store,
        Err(error) => {
            let error = error_chain(&error);
            tracing::error!(database = %config.database.display(), %error, "cannot open the database");
            return Err(ExitCode::FAILURE);
        }
    };

    let (listener, addr) = match listen(config.listen).await {
        Ok(bound) => bound,
        Err(error) => {
            tracing::error!(listen = %config.listen, %error, "cannot listen");
            return Err(ExitCode::FAILURE);
        }
    };
    if addr.ip().is_loopback() {
        // Groups are given stream URLs on this address, which no speaker can reach.
        tracing::warn!(
            %addr,
            "listening on a loopback address: Sonos Groups can't fetch the Bridge's streams"
        );
    }
    let discovery_address = config.sonos.discovery_address;
    tracing::info!(%discovery_address, "searching for Sonos players");
    let sonos = sonos::Upnp::start(discovery_address, Arc::new(clock::SystemClock));
    let app_api = Arc::new(AppApi::new(
        config.library_servers,
        store,
        Arc::new(sonos),
        addr,
    ));
    let router = api::server::router(app_api.clone()).merge(stream::router(app_api));

    tracing::info!(%addr, "listening");
    if let Err(error) = shutdown::serve(listener, router, signals).await {
        tracing::error!(%error, "server stopped");
        return Err(ExitCode::FAILURE);
    }
    Ok(())
}
