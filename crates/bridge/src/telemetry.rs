//! The one place where the Bridge decides where its logs go and in what format.
//!
//! Everything else only emits `tracing` events and spans, so replacing this subscriber (for
//! example with an OpenTelemetry or Prometheus exporter) touches nothing outside this module.

use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;

/// Writes events to stdout as JSON, one object per line, with event fields at the top level.
/// `RUST_LOG` sets the level; without it, `info`.
pub fn init() {
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy()
        // Dioxus warns on every start that the Bridge has no `index.html`; it serves no pages.
        .add_directive(
            "dioxus_server::config=error"
                .parse()
                .expect("valid directive"),
        );
    tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_env_filter(filter)
        .init();
}
