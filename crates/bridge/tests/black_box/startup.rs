//! An invalid configuration stops the Bridge before it serves: a non-zero exit and one error
//! event naming what is wrong.

use crate::support::{Exited, config_file, run_until_exit};

/// Asserts the Bridge refused to start, and returns its single error event's `error` field.
fn refused(exited: &Exited) -> String {
    assert!(!exited.status.success(), "exit status {:?}", exited.status);
    assert!(
        !exited
            .events
            .iter()
            .any(|event| event["message"] == "listening"),
        "Bridge served despite invalid configuration: {:#?}",
        exited.events
    );
    let errors: Vec<_> = exited.errors().collect();
    assert_eq!(
        errors.len(),
        1,
        "expected one error event: {:#?}",
        exited.events
    );
    assert_eq!(errors[0]["message"], "invalid configuration");
    errors[0]["error"].as_str().expect("error field").to_owned()
}

fn refused_with(config: &str) -> String {
    let (_dir, path) = config_file(config);
    refused(&run_until_exit(&path))
}

const LISTEN: &str = "listen = \"127.0.0.1:0\"\n";

#[test]
fn malformed_library_server_url() {
    let error = refused_with(&format!(
        "{LISTEN}[[library_servers]]\nurl = \"https://abs lan\"\n"
    ));
    assert!(error.contains("library_servers[0].url"), "{error}");
    assert!(error.contains("https://abs lan"), "{error}");
}

#[test]
fn unsupported_scheme() {
    let error = refused_with(&format!(
        "{LISTEN}[[library_servers]]\nurl = \"https://ok.lan\"\n[[library_servers]]\nurl = \"ftp://abs.lan\"\n"
    ));
    assert!(error.contains("library_servers[1].url"), "{error}");
    assert!(error.contains("ftp://abs.lan"), "{error}");
}

#[test]
fn two_entries_for_the_same_library_server() {
    let error = refused_with(&format!(
        "{LISTEN}[[library_servers]]\nurl = \"https://abs.lan/\"\n[[library_servers]]\nurl = \"HTTPS://ABS.lan:443\"\n"
    ));
    assert!(error.contains("library_servers[1].url"), "{error}");
    assert!(error.contains("library_servers[0]"), "{error}");
    assert!(error.contains("https://abs.lan"), "{error}");
}

#[test]
fn url_with_a_query() {
    let error = refused_with(&format!(
        "{LISTEN}[[library_servers]]\nurl = \"https://abs.lan/?token=abc\"\n"
    ));
    assert!(error.contains("https://abs.lan/?token=abc"), "{error}");
}

#[test]
fn invalid_toml() {
    let error = refused_with(&format!("{LISTEN}[[library_servers]\n"));
    assert!(error.contains("TOML"), "{error}");
}

#[test]
fn missing_listen_address() {
    let error = refused_with("[[library_servers]]\nurl = \"https://abs.lan\"\n");
    assert!(error.contains("listen"), "{error}");
}

#[test]
fn unknown_setting() {
    let error = refused_with(&format!("{LISTEN}library_server = []\n"));
    assert!(error.contains("library_server"), "{error}");
}

#[test]
fn missing_file() {
    let (dir, _) = config_file("");
    let missing = dir.path().join("does-not-exist.toml");
    let exited = run_until_exit(&missing);
    refused(&exited);
    let event = exited.errors().next().unwrap();
    assert_eq!(event["config"], missing.display().to_string());
}
