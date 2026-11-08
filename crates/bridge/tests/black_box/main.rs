//! Black-box tests: each starts the real Bridge, acts through the App API, and observes the
//! API, the exit status and the log events; tests that need a Library Server use a real
//! Audiobookshelf in a container. The harness is the `test-support` crate; add new areas as
//! modules here so they share one binary.

mod browse;
mod control;
mod encryption;
mod finished;
mod forwarder;
mod groups;
mod handoff;
mod home;
mod length_hider;
mod library_servers;
mod logging;
mod login;
mod shutdown;
mod startup;
