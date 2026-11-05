//! A TCP forwarder on loopback in front of another server, that a test can take down and
//! bring back: a Library Server outage, or a second Library Server URL for the same
//! Audiobookshelf. It can also cut answers short: a Library Server failing mid-body.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, copy, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::{JoinHandle, JoinSet};

/// How long to wait before accepting again after `accept` failed, so a persistent failure
/// (out of file descriptors) doesn't spin.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// Forwards connections on its own port to `target`; stops forwarding when dropped.
#[derive(Debug)]
pub struct Forwarder {
    port: u16,
    state: Arc<State>,
    serving: JoinHandle<()>,
}

#[derive(Debug)]
struct State {
    target: String,
    /// While false, connections are accepted and dropped at once.
    forwarding: AtomicBool,
    /// When not 0, a connection opened since passes at most this many bytes back from
    /// `target`, then closes.
    cut_after: AtomicUsize,
    connections: Mutex<JoinSet<()>>,
}

impl Forwarder {
    /// Starts forwarding a free loopback port to `target` (`host:port`).
    pub async fn to(target: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind forwarder");
        let port = listener.local_addr().expect("forwarder address").port();
        let state = Arc::new(State {
            target: target.to_owned(),
            forwarding: AtomicBool::new(true),
            cut_after: AtomicUsize::new(0),
            connections: Mutex::new(JoinSet::new()),
        });
        let serving = tokio::spawn(serve(listener, Arc::clone(&state)));
        Self {
            port,
            state,
            serving,
        }
    }

    /// The forwarder's URL, as an operator would configure it.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Closes every connection through the forwarder, and closes new ones as soon as they
    /// open, until [`Forwarder::resume`].
    pub async fn stop(&self) {
        self.state.forwarding.store(false, Ordering::SeqCst);
        self.close_all().await;
    }

    /// Forwards again.
    pub fn resume(&self) {
        self.state.forwarding.store(true, Ordering::SeqCst);
    }

    /// Closes every connection through the forwarder; from now on each new one passes back
    /// at most `bytes` of what `target` answers, then closes.
    pub async fn cut_after(&self, bytes: usize) {
        self.state.cut_after.store(bytes, Ordering::SeqCst);
        self.close_all().await;
    }

    async fn close_all(&self) {
        let mut connections = self.state.connections.lock().await;
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        self.serving.abort();
    }
}

/// Accepts until aborted. Open connections live in `state.connections`: `stop` aborts them,
/// and so does dropping the set once the forwarder and this task are both gone.
async fn serve(listener: TcpListener, state: Arc<State>) {
    loop {
        let mut inbound = match listener.accept().await {
            Ok((inbound, _)) => inbound,
            Err(error) => {
                eprintln!("[forwarder] accept failed: {error}");
                tokio::time::sleep(ACCEPT_BACKOFF).await;
                continue;
            }
        };
        // Checked under the lock `stop` holds, so no connection slips past a stop; while
        // stopped, `inbound` drops here, closing the connection.
        let mut connections = state.connections.lock().await;
        if !state.forwarding.load(Ordering::SeqCst) {
            continue;
        }
        // Reap connections that have ended, so the set holds only open ones.
        while connections.try_join_next().is_some() {}
        let target = state.target.clone();
        let cut_after = state.cut_after.load(Ordering::SeqCst);
        connections.spawn(async move {
            let mut outbound = match TcpStream::connect(&target).await {
                Ok(outbound) => outbound,
                Err(error) => {
                    eprintln!("[forwarder] connect to {target} failed: {error}");
                    return;
                }
            };
            if cut_after == 0 {
                let _ = copy_bidirectional(&mut inbound, &mut outbound).await;
                return;
            }
            let (mut from_client, mut to_client) = inbound.split();
            let (from_server, mut to_server) = outbound.split();
            let mut limited = from_server.take(cut_after as u64);
            // Whichever ends first ends the connection.
            tokio::select! {
                _ = copy(&mut from_client, &mut to_server) => {}
                _ = copy(&mut limited, &mut to_client) => {}
            }
            let _ = to_client.shutdown().await;
        });
    }
}
