//! A TCP forwarder on loopback in front of another server, that a test can take down and
//! bring back: a Library Server outage, or a second Library Server URL for the same
//! Audiobookshelf.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::copy_bidirectional;
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
        let mut connections = self.state.connections.lock().await;
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }

    /// Forwards again.
    pub fn resume(&self) {
        self.state.forwarding.store(true, Ordering::SeqCst);
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
        connections.spawn(async move {
            match TcpStream::connect(&target).await {
                Ok(mut outbound) => {
                    let _ = copy_bidirectional(&mut inbound, &mut outbound).await;
                }
                Err(error) => eprintln!("[forwarder] connect to {target} failed: {error}"),
            }
        });
    }
}
