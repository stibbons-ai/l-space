//! How the Bridge stops when the operator asks it to (`docker stop`'s SIGTERM, or Ctrl-C's
//! SIGINT): it stops accepting connections, lets the requests in flight finish for at most
//! [`DRAIN`], and leaves whatever is still open after that (a Group's media stream can stay
//! open for hours) to [`teardown`], which closes the store and cancels the rest.

use std::io;
use std::time::{Duration, Instant};

use axum::Router;
use store_sqlite::{Closer, Closing};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::oneshot;

/// How long requests in flight get to finish once a stop is requested. With
/// [`RUNTIME_SHUTDOWN`] on top, still well under the 10 s `docker stop` waits before it kills
/// the Bridge.
pub const DRAIN: Duration = Duration::from_secs(5);

/// How long [`teardown`] waits, after the drain, for work already running: an SQLite
/// transaction to commit, then other blocking work (a DNS lookup for a Library Server configured
/// by hostname). `DRAIN` plus this stays well under `docker stop`'s 10 s.
pub const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(2);

/// The signals that ask the Bridge to stop. Installed first thing at startup: a signal that
/// arrives while the Bridge is still starting is kept, and the Bridge stops cleanly as soon as
/// it serves.
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
/// Connections still open after that stay behind; [`teardown`] cancels them.
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

/// Tears the Bridge down once [`serve`] has returned (or startup failed): closes the store
/// (`store`; `None` if the Bridge never opened it), then stops the runtime with what is left of
/// [`RUNTIME_SHUTDOWN`]. The store is closed first because App API handlers don't run on
/// `runtime`: Dioxus runs each on a local runtime of its own, so stopping `runtime` doesn't wait
/// for their transactions; closing the store does, whichever runtime they run on.
pub fn teardown(runtime: Runtime, store: Option<&Closer>) {
    let started = Instant::now();
    if let Some(store) = store {
        close_store(store, RUNTIME_SHUTDOWN);
    }
    stop_runtime(runtime, RUNTIME_SHUTDOWN.saturating_sub(started.elapsed()));
}

/// Closes the store, letting a transaction already running commit for at most `bound`, and
/// logs the outcome. A transaction still running then is abandoned: its hot journal is rolled
/// back when the Bridge next opens the database, so the database stays consistent.
fn close_store(store: &Closer, bound: Duration) {
    match store.close(bound) {
        Closing::Closed => tracing::debug!("database closed"),
        Closing::Failed(error) => {
            let error = crate::error_chain(&error);
            tracing::warn!(%error, "cannot close the database");
        }
        Closing::TransactionStillRunning => tracing::warn!(
            ?bound,
            "database transaction still running at exit: abandoned, rolled back when the database is next opened"
        ),
    }
}

/// Shuts the runtime down: cancels every task left (connections still open after the drain,
/// detached Handoff starts) and waits at most `bound` for blocking work already running,
/// logging a warning when that wait runs out. The store is closed, or its last transaction
/// abandoned, by then. A detached Handoff start cut between storing the Handoff and asking
/// Sonos to play it stays stored but unplayed until restart recovery (#22) exists.
fn stop_runtime(runtime: Runtime, bound: Duration) {
    let asked = Instant::now();
    runtime.shutdown_timeout(bound);
    // Tokio doesn't report whether the wait ran out; it returns before the bound only once
    // every blocking thread has finished. No bound left means closing the store used it up,
    // and warned.
    if !bound.is_zero() && asked.elapsed() >= bound {
        tracing::warn!(?bound, "blocking work still running at exit: abandoned");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[test]
    fn stop_runtime_lets_running_blocking_work_finish_within_its_bound_and_no_longer() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        // Blocking work spawned but not yet started is cancelled at shutdown; wait until both
        // tasks run, so the test is about work already running.
        let (started, running) = std::sync::mpsc::channel();
        let finished = Arc::new(AtomicBool::new(false));
        let short = (finished.clone(), started.clone());
        runtime.spawn_blocking(move || {
            let (finished, started) = short;
            started.send(()).expect("test waits");
            std::thread::sleep(Duration::from_millis(300));
            finished.store(true, Ordering::SeqCst);
        });
        runtime.spawn_blocking(move || {
            started.send(()).expect("test waits");
            std::thread::sleep(Duration::from_secs(30));
        });
        for _ in 0..2 {
            running.recv().expect("blocking work starts");
        }

        let asked = Instant::now();
        stop_runtime(runtime, RUNTIME_SHUTDOWN);

        let took = asked.elapsed();
        assert!(
            finished.load(Ordering::SeqCst),
            "blocking work shorter than the bound was cut off"
        );
        assert!(
            took < RUNTIME_SHUTDOWN + Duration::from_secs(3),
            "took {took:?}"
        );
    }
}
