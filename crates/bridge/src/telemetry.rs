//! The one place where the Bridge decides where its logs go and in what format.
//!
//! Everything else only emits `tracing` events and spans, so replacing this subscriber (for
//! example with an OpenTelemetry or Prometheus exporter) touches nothing outside this module.

use tracing_subscriber::EnvFilter;

/// Our directives, which the operator's `RUST_LOG` extends or overrides. Dioxus warns on every
/// start that the Bridge has no `index.html`; it serves no pages.
const DEFAULT_DIRECTIVES: &str = "info,dioxus_server::config=error";

/// Writes events to stdout as JSON, one object per line, with event fields at the top level.
///
/// `RUST_LOG` directives are appended to [`DEFAULT_DIRECTIVES`]. A later directive for the same
/// target replaces an earlier one, so `RUST_LOG=warn` lowers the global level and
/// `RUST_LOG=hyper=debug` leaves every other target at `info`.
pub fn init() {
    let directives = match std::env::var("RUST_LOG") {
        Ok(operator) if !operator.trim().is_empty() => format!("{DEFAULT_DIRECTIVES},{operator}"),
        _ => DEFAULT_DIRECTIVES.to_owned(),
    };
    tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_env_filter(EnvFilter::builder().parse_lossy(directives))
        .init();
}
