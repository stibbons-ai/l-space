//! Black-box tests: each starts the real Bridge, acts through the App API, and observes the
//! API, the exit status and the log events. See [`support`] for the harness; add new areas as
//! modules here so they share one binary.

mod library_servers;
mod logging;
mod startup;
mod support;
