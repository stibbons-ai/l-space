//! How the Bridge stops when the operator asks it to (`docker stop`'s SIGTERM, or Ctrl-C's
//! SIGINT): it stops accepting connections, lets the requests in flight finish for at most
//! [`DRAIN`], and leaves whatever is still open after that (a Group's media stream can stay
//! open for hours) for [`stop_runtime`] to cancel.

use std::io;
use std::time::Duration;

use axum::Router;
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::oneshot;

/// How long requests in flight get to finish once a stop is requested. With
/// [`RUNTIME_SHUTDOWN`] on top, still well under the 10 s `docker stop` waits before it kills
/// the Bridge.
pub const DRAIN: Duration = Duration::from_secs(5);

/// How long [`stop_runtime`] waits for blocking work already running (an SQLite transaction, a
/// DNS lookup for a Library Server or a Sonos player) after the drain. `DRAIN` plus this stays
/// well under `docker stop`'s 10 s.
pub const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(2);

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

    /// Resolves at the first SIGTERM or SIGINT, logging `shutting down`. A second signal during
    /// the drain has no effect: the drain bound ends it.
    async fn requested(mut self) {
        let name = tokio::select! {
            _ = self.terminate.recv() => "SIGTERM",
            _ = self.interrupt.recv() => "SIGINT",
        };
        tracing::info!(signal = name, "shutting down");
    }
}

/// Serves `router` on `listener` until `signals` asks the Bridge to stop, then lets the requests
/// in flight finish for at most [`DRAIN`]. A second signal during the drain has no effect.
/// Connections still open after that stay behind; [`stop_runtime`] cancels them.
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

/// Shuts the runtime down: cancels every task left (connections still open after the drain,
/// detached Handoff starts) and waits at most [`RUNTIME_SHUTDOWN`] for blocking work already
/// running. Tokio doesn't report whether that wait ran out. The store closes when the last task
/// holding its `Arc` is dropped; an SQLite transaction still running when the process exits
/// rolls back through its journal, so the database stays consistent. A detached Handoff start
/// cut between storing the Handoff and asking Sonos to play it is left for restart recovery
/// (#22).
pub fn stop_runtime(runtime: Runtime) {
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN);
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn stop_runtime_does_not_wait_for_blocking_work_past_its_bound() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.spawn_blocking(|| std::thread::sleep(Duration::from_secs(30)));

        let asked = Instant::now();
        stop_runtime(runtime);

        let took = asked.elapsed();
        assert!(
            took < RUNTIME_SHUTDOWN + Duration::from_secs(3),
            "took {took:?}"
        );
    }
}
