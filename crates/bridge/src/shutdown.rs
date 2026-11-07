//! How the Bridge stops when the operator asks it to (`docker stop`'s SIGTERM, or Ctrl-C's
//! SIGINT): it stops accepting connections, lets the requests in flight finish for at most
//! [`DRAIN`], and leaves whatever is still open after that (a Group's media stream can stay
//! open for hours) for the caller to drop.

use std::io;
use std::time::Duration;

use axum::Router;
use tokio::net::TcpListener;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::oneshot;

/// How long requests in flight get to finish once a stop is requested: well under the 10 s
/// `docker stop` waits before it kills the Bridge.
pub const DRAIN: Duration = Duration::from_secs(5);

/// The signals that ask the Bridge to stop. Installed before the Bridge serves, so a signal
/// that arrives as soon as it listens no longer ends it at once.
#[derive(Debug)]
pub struct Signals {
    terminate: Signal,
    interrupt: Signal,
}

impl Signals {
    pub fn install() -> io::Result<Self> {
        Ok(Self {
            terminate: signal(SignalKind::terminate())?,
            interrupt: signal(SignalKind::interrupt())?,
        })
    }

    /// Resolves at the first SIGTERM or SIGINT, logging `shutting down`.
    async fn requested(mut self) {
        let name = tokio::select! {
            _ = self.terminate.recv() => "SIGTERM",
            _ = self.interrupt.recv() => "SIGINT",
        };
        tracing::info!(signal = name, "shutting down");
    }
}

/// Serves `router` on `listener` until `signals` asks the Bridge to stop, then lets the requests
/// in flight finish for at most [`DRAIN`]. Connections still open after that stay behind; the
/// caller ends them by dropping the runtime.
pub async fn serve(listener: TcpListener, router: Router, signals: Signals) -> io::Result<()> {
    let (stop, stopping) = oneshot::channel::<()>();
    let server = axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = stopping.await;
        })
        .into_future();
    tokio::pin!(server);
    tokio::select! {
        served = &mut server => return served,
        () = signals.requested() => {
            let _ = stop.send(());
        }
    }
    match tokio::time::timeout(DRAIN, server).await {
        Ok(served) => served,
        Err(_) => {
            tracing::warn!(drain = ?DRAIN, "connections still open after the drain: dropping them");
            Ok(())
        }
    }
}
