//! How `RUST_LOG` shapes what an operator sees.

use crate::support::TestBridge;

fn logged_by(bridge: &TestBridge, target: &str) -> Vec<String> {
    bridge
        .startup_output()
        .iter()
        .filter(|event| event["target"] == target)
        .map(|event| event["level"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[test]
fn default_level_is_info_without_dioxus_index_html_noise() {
    let bridge = TestBridge::start("");

    // Control for the tests below: at the default level Dioxus's info event shows.
    assert_eq!(
        logged_by(&bridge, "dioxus_server::server"),
        ["INFO"],
        "{:#?}",
        bridge.startup_output()
    );
    assert_eq!(
        logged_by(&bridge, "dioxus_server::config"),
        Vec::<String>::new(),
        "{:#?}",
        bridge.startup_output()
    );
}

#[test]
fn rust_log_directives_may_be_padded_with_spaces() {
    let bridge = TestBridge::start_logging("", " warn , bridge=info ");

    // `warn` took effect: Dioxus's info event about registering routes is gone.
    assert_eq!(
        logged_by(&bridge, "dioxus_server::server"),
        Vec::<String>::new(),
        "{:#?}",
        bridge.startup_output()
    );
}

#[test]
fn operator_rust_log_overrides_the_dioxus_suppression() {
    let bridge = TestBridge::start_logging("", "dioxus_server::config=warn");

    assert_eq!(
        logged_by(&bridge, "dioxus_server::config"),
        ["WARN"],
        "{:#?}",
        bridge.startup_output()
    );
}
