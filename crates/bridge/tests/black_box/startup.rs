//! A Bridge that cannot start properly stops before it serves: a non-zero exit and one error
//! event saying what is wrong, without repeating any secret from the configuration.

use std::net::TcpListener;

use crate::support::{Exited, LISTEN, RUST_LOG, config_file, is_listening, run_until_exit};

/// Asserts the Bridge refused to start with one error event `message`, and returns that
/// event's `error` field.
fn refused(exited: &Exited, message: &str) -> String {
    assert!(!exited.status.success(), "exit status {:?}", exited.status);
    assert!(
        !exited.events.iter().any(is_listening),
        "Bridge served anyway: {:#?}",
        exited.events
    );
    let errors: Vec<_> = exited.errors().collect();
    assert_eq!(
        errors.len(),
        1,
        "expected one error event: {:#?}",
        exited.events
    );
    assert_eq!(errors[0]["message"], message);
    errors[0]["error"].as_str().expect("error field").to_owned()
}

fn run_with(config: &str) -> Exited {
    let (_dir, path) = config_file(config);
    run_until_exit(&path, RUST_LOG)
}

fn refused_with(config: &str) -> String {
    refused(&run_with(config), "invalid configuration")
}

/// Asserts the Bridge refused `config` and never wrote `secret` anywhere.
fn refused_without_leaking(config: &str, secret: &str) -> String {
    let exited = run_with(config);
    let error = refused(&exited, "invalid configuration");
    assert!(
        !exited.output_contains(secret),
        "{secret:?} leaked: {:#?}",
        exited.events
    );
    error
}

#[test]
fn malformed_library_server_url_is_named_by_entry_only() {
    let error = refused_with(&format!(
        "{LISTEN}[[library_servers]]\nurl = \"https://abs lan\"\n"
    ));
    assert!(error.contains("library_servers[0].url"), "{error}");
    assert!(!error.contains("abs lan"), "{error}");
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
fn url_with_a_query_is_named_without_its_value() {
    let error = refused_without_leaking(
        &format!("{LISTEN}[[library_servers]]\nurl = \"https://abs.lan/?token=s3cret\"\n"),
        "s3cret",
    );
    assert!(error.contains("library_servers[0].url"), "{error}");
    assert!(error.contains("https://abs.lan/?REDACTED"), "{error}");
}

#[test]
fn url_with_an_at_sign_is_named_by_entry_only() {
    for url in [
        "https://alice:hunter2@abs lan",
        "https://alice:hunter2@abs.lan:99999",
        "https://abs.lan/?user=a@b.c&token=hunter2",
        // The url crate would read host `alice`, port 123 and path `/hunter2@abs.lan`.
        "https://alice:123/hunter2@abs.lan",
    ] {
        let error = refused_without_leaking(
            &format!("{LISTEN}[[library_servers]]\nurl = \"{url}\"\n"),
            "hunter2",
        );
        assert!(error.contains("library_servers[0].url:"), "{error}");
    }
}

#[test]
fn invalid_toml_is_positioned_without_quoting_the_line() {
    let error = refused_without_leaking(
        &format!("{LISTEN}[[library_servers]]\nurl = \"https://alice:hunter2@abs.lan\n"),
        "hunter2",
    );
    assert!(error.contains("TOML at line 3"), "{error}");
}

#[test]
fn unknown_setting_is_named_without_its_value() {
    let error = refused_without_leaking(&format!("{LISTEN}token = \"s3cret\"\n"), "s3cret");
    assert!(error.contains("token"), "{error}");
}

#[test]
fn missing_listen_address() {
    let error = refused_with("[[library_servers]]\nurl = \"https://abs.lan\"\n");
    assert!(error.contains("listen"), "{error}");
}

#[test]
fn missing_file() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let missing = dir.path().join("does-not-exist.toml");
    let exited = run_until_exit(&missing, RUST_LOG);
    refused(&exited, "invalid configuration");
    let event = exited.errors().next().unwrap();
    assert_eq!(event["config"], missing.display().to_string());
}

#[test]
fn rust_log_for_other_targets_keeps_the_startup_error() {
    let (_dir, path) = config_file(&format!(
        "{LISTEN}[[library_servers]]\nurl = \"ftp://abs.lan\"\n"
    ));
    let exited = run_until_exit(&path, "hyper=debug");
    let error = refused(&exited, "invalid configuration");
    assert!(error.contains("ftp://abs.lan"), "{error}");
}

#[test]
fn listen_address_in_use() {
    let taken = TcpListener::bind("127.0.0.1:0").expect("bind a port");
    let addr = taken.local_addr().expect("local addr");
    let exited = run_with(&format!("listen = \"{addr}\"\n"));
    refused(&exited, "cannot listen");
}

#[test]
fn wrongly_typed_value_is_named_without_its_value() {
    let error =
        refused_without_leaking(&format!("{LISTEN}library_servers = \"s3cret\"\n"), "s3cret");
    assert!(error.contains("TOML at line 2"), "{error}");
}
