//! The Bridge binary: loads the operator's configuration and serves the App API.

mod config;
mod telemetry;

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use tokio::net::TcpListener;

use crate::config::{Config, LibraryServerConfig};

#[derive(Debug, Parser)]
#[command(version, about = "The L-Space Bridge (Clacks)")]
struct Args {
    /// Path to the configuration file (TOML).
    #[arg(long, env = "BRIDGE_CONFIG")]
    config: PathBuf,
}

/// The App API's view of this Bridge.
#[derive(Debug)]
struct Bridge {
    library_servers: Vec<api::LibraryServer>,
}

impl Bridge {
    fn new(library_servers: Vec<LibraryServerConfig>) -> Self {
        let library_servers = library_servers
            .into_iter()
            .map(|server| api::LibraryServer {
                base_url: server.url.into_string(),
                name: server.name,
            })
            .collect();
        Self { library_servers }
    }
}

impl api::server::Bridge for Bridge {
    fn library_servers(&self) -> Vec<api::LibraryServer> {
        self.library_servers.clone()
    }
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

    let (listener, addr) = match listen(config.listen).await {
        Ok(bound) => bound,
        Err(error) => {
            tracing::error!(listen = %config.listen, %error, "cannot listen");
            return ExitCode::FAILURE;
        }
    };
    let router = api::server::router(Arc::new(Bridge::new(config.library_servers)));

    tracing::info!(%addr, "listening");
    if let Err(error) = axum::serve(listener, router).await {
        tracing::error!(%error, "server stopped");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
