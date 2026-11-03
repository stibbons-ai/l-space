//! Black-box tests: each starts the real Bridge and talks to it only through the App API.
//! See [`support`] for the harness; add new areas as modules here so they share one binary.

mod library_servers;
mod startup;
mod support;
