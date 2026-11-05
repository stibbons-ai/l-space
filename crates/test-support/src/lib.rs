//! The harness the Bridge's black-box tests and the App's flow tests share: a real Bridge
//! process ([`bridge`]) and a real Audiobookshelf in a container ([`audiobookshelf`]).

pub mod audiobookshelf;
pub mod bridge;

/// A loopback address nothing listens on.
pub fn closed_port() -> std::net::SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("bind a loopback port")
}
