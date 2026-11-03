//! The Bridge binary: loads the operator's configuration and serves the App API.

mod config;
mod telemetry;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use tokio::net::TcpListener;

use crate::config::Config;

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

impl api::server::Bridge for Bridge {
    fn library_servers(&self) -> Vec<api::LibraryServer> {
        self.library_servers.clone()
    }
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

    let bound = async {
        let listener = TcpListener::bind(config.listen).await?;
        let addr = listener.local_addr()?;
        std::io::Result::Ok((listener, addr))
    };
    let (listener, addr) = match bound.await {
        Ok(bound) => bound,
        Err(error) => {
            tracing::error!(listen = %config.listen, %error, "cannot listen");
            return ExitCode::FAILURE;
        }
    };

    let bridge = Bridge {
        library_servers: config
            .library_servers
            .into_iter()
            .map(|server| api::LibraryServer {
                base_url: server.url.as_str().to_owned(),
                name: server.name,
            })
            .collect(),
    };
    let router = api::server::router(Arc::new(bridge));

    tracing::info!(%addr, "listening");
    if let Err(error) = axum::serve(listener, router).await {
        tracing::error!(%error, "server stopped");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
