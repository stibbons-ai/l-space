//! Black-box tests: each starts the real Bridge, acts through the App API, and observes the
//! API, the exit status and the log events; tests that need a Library Server use a real
//! Audiobookshelf in a container. See [`support`] and [`audiobookshelf`] for the harness; add
//! new areas as modules here so they share one binary.

mod audiobookshelf;
mod forwarder;
mod library_servers;
mod logging;
mod login;
mod startup;
mod support;
