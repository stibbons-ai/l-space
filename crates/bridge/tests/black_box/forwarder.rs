//! A TCP forwarder on loopback in front of another server, that a test can take down and
//! bring back on the same port: a Library Server outage, or a second Library Server URL for
//! the same Audiobookshelf.

use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};

/// Forwards connections on its own port to `target`; stops forwarding when dropped.
#[derive(Debug)]
pub struct Forwarder {
    port: u16,
    target: String,
    serving: Option<JoinHandle<()>>,
}

impl Forwarder {
    /// Starts forwarding a free loopback port to `target` (`host:port`).
    pub async fn to(target: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind forwarder");
        let port = listener.local_addr().expect("forwarder address").port();
        let target = target.to_owned();
        let serving = Some(tokio::spawn(serve(listener, target.clone())));
        Self {
            port,
            target,
            serving,
        }
    }

    /// The forwarder's URL, as an operator would configure it.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Closes the port and every connection through it: connecting is refused until
    /// [`Forwarder::resume`].
    pub async fn stop(&mut self) {
        if let Some(serving) = self.serving.take() {
            serving.abort();
            let _ = serving.await;
        }
    }

    /// Forwards again, on the same port.
    pub async fn resume(&mut self) {
        let listener = TcpListener::bind(("127.0.0.1", self.port))
            .await
            .expect("rebind forwarder");
        self.serving = Some(tokio::spawn(serve(listener, self.target.clone())));
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        if let Some(serving) = &self.serving {
            serving.abort();
        }
    }
}

/// Accepts and forwards until aborted; aborting drops every open connection with it.
async fn serve(listener: TcpListener, target: String) {
    let mut connections = JoinSet::new();
    loop {
        let Ok((mut inbound, _)) = listener.accept().await else {
            continue;
        };
        let target = target.clone();
        connections.spawn(async move {
            if let Ok(mut outbound) = TcpStream::connect(&target).await {
                let _ = copy_bidirectional(&mut inbound, &mut outbound).await;
            }
        });
    }
}
