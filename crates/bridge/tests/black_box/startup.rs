//! A Bridge that cannot start properly stops before it serves: a non-zero exit and one error
//! event saying what is wrong, without repeating any secret from the configuration or the
//! encryption key.

use std::net::TcpListener;

use crate::support::{
    DATABASE, Exited, KEY, KeyEnv, KeyFile, LISTEN, RUST_LOG, config_file, is_listening,
    library_server_toml, run_until_exit, run_until_exit_with_key,
};

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
    run_with_key(config, &KeyEnv::value(KEY))
}

fn run_with_key(config: &str, key: &KeyEnv) -> Exited {
    let (_dir, path) = config_file(config);
    run_until_exit_with_key(&path, RUST_LOG, key)
}

fn refused_with(config: &str) -> String {
    refused(&run_with(config), "invalid configuration")
}

/// Asserts the Bridge refused `config` and never wrote `secret` anywhere.
fn refused_without_leaking(config: &str, secret: &str) -> String {
    let exited = run_with(config);
    let error = refused(&exited, "invalid configuration");
    // On failure, the output isn't printed: it would repeat the secret.
    assert!(
        !exited.output_contains(secret),
        "a configuration secret leaked"
    );
    error
}

#[test]
fn malformed_library_server_url_is_named_by_entry_only() {
    let error = refused_with(&format!(
        "{LISTEN}{DATABASE}{}",
        library_server_toml("https://abs lan")
    ));
    assert!(error.contains("library_servers[0].url"), "{error}");
    assert!(!error.contains("abs lan"), "{error}");
}

#[test]
fn unsupported_scheme() {
    let error = refused_with(&format!(
        "{LISTEN}{DATABASE}{}{}",
        library_server_toml("https://ok.lan"),
        library_server_toml("ftp://abs.lan")
    ));
    assert!(error.contains("library_servers[1].url"), "{error}");
    assert!(error.contains("ftp://abs.lan"), "{error}");
}

#[test]
fn two_entries_for_the_same_library_server() {
    let error = refused_with(&format!(
        "{LISTEN}{DATABASE}{}{}",
        library_server_toml("https://abs.lan/"),
        library_server_toml("HTTPS://ABS.lan:443")
    ));
    assert!(error.contains("library_servers[1].url"), "{error}");
    assert!(error.contains("library_servers[0]"), "{error}");
    assert!(error.contains("https://abs.lan"), "{error}");
}

#[test]
fn url_with_a_query_is_named_without_its_value() {
    let error = refused_without_leaking(
        &format!(
            "{LISTEN}{DATABASE}{}",
            library_server_toml("https://abs.lan/?token=s3cret")
        ),
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
            &format!("{LISTEN}{DATABASE}{}", library_server_toml(url)),
            "hunter2",
        );
        assert!(error.contains("library_servers[0].url:"), "{error}");
    }
}

#[test]
fn invalid_toml_is_positioned_without_quoting_the_line() {
    let error = refused_without_leaking(
        &format!("{LISTEN}{DATABASE}[[library_servers]]\nurl = \"https://alice:hunter2@abs.lan\n"),
        "hunter2",
    );
    assert!(error.contains("TOML at line 4"), "{error}");
}

#[test]
fn unknown_setting_is_named_without_its_value() {
    let error =
        refused_without_leaking(&format!("{LISTEN}{DATABASE}token = \"s3cret\"\n"), "s3cret");
    assert!(error.contains("token"), "{error}");
}

#[test]
fn missing_listen_address() {
    let error = refused_with(DATABASE);
    assert!(error.contains("listen"), "{error}");
}

#[test]
fn missing_database() {
    let error = refused_with(LISTEN);
    assert!(error.contains("database"), "{error}");
}

#[test]
fn database_that_cannot_be_opened() {
    let exited = run_with(&format!("{LISTEN}database = \"missing-dir/bridge.db\"\n"));
    let error = refused(&exited, "cannot open the database");
    // The step that failed, then its cause.
    assert!(error.starts_with("create the database file: "), "{error}");
    assert!(error.len() > "create the database file: ".len(), "{error}");
    let event = exited.errors().next().unwrap();
    assert_eq!(event["database"], "missing-dir/bridge.db");
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
        "{LISTEN}{DATABASE}{}",
        library_server_toml("ftp://abs.lan")
    ));
    let exited = run_until_exit(&path, "hyper=debug");
    let error = refused(&exited, "invalid configuration");
    assert!(error.contains("ftp://abs.lan"), "{error}");
}

#[test]
fn listen_address_in_use() {
    let taken = TcpListener::bind("127.0.0.1:0").expect("bind a port");
    let addr = taken.local_addr().expect("local addr");
    let exited = run_with(&format!("listen = \"{addr}\"\n{DATABASE}"));
    refused(&exited, "cannot listen");
}

#[test]
fn wrongly_typed_value_is_named_without_its_value() {
    let error = refused_without_leaking(
        &format!("{LISTEN}{DATABASE}library_servers = \"s3cret\"\n"),
        "s3cret",
    );
    assert!(error.contains("TOML at line 3"), "{error}");
}

const KEY_HINT: &str = "generate a key with `openssl rand -base64 32`";

/// Asserts the Bridge refused to start with the encryption key variables `key`, writing none
/// of `secrets`, and returns its error.
fn refused_key(key: &KeyEnv, secrets: &[&str]) -> String {
    let exited = run_with_key(&format!("{LISTEN}{DATABASE}"), key);
    let error = refused(&exited, "cannot load the encryption key");
    // On failure, the output isn't printed: it would repeat the secret.
    for secret in secrets {
        assert!(!exited.output_contains(secret), "a key secret leaked");
    }
    assert!(error.ends_with(KEY_HINT), "{error}");
    error
}

fn named_file(value: &str) -> Option<KeyFile> {
    Some(KeyFile::Named(value.into()))
}

/// A variable set to the empty string (as compose's `${VAR:-}` does) counts as unset.
#[test]
fn no_encryption_key() {
    for key in [
        KeyEnv::default(),
        KeyEnv::value(""),
        KeyEnv {
            file: named_file(""),
            value: None,
        },
        KeyEnv {
            file: named_file(""),
            value: Some(String::new()),
        },
    ] {
        let error = refused_key(&key, &[]);
        assert!(
            error.starts_with(
                "neither BRIDGE_ENCRYPTION_KEY_FILE nor BRIDGE_ENCRYPTION_KEY is set (or both \
                 are empty); "
            ),
            "{key:?}: {error}"
        );
    }
}

#[test]
fn unusable_encryption_key_value() {
    for (value, problem) in [
        (" \n", "is empty"),
        ("s3cret-is-not-base64!", "is not base64"),
        ("i6n08ZhvCMQSMfsPMru0Tw==", "decodes to 16 bytes, not 32"),
        (
            "XI5RQJRCfpAnYDpji+U84BcbVNSWHdXV0RL2qChwW5Qj",
            "decodes to 33 bytes, not 32",
        ),
    ] {
        let secrets: &[&str] = if value.trim().is_empty() {
            &[]
        } else {
            &[value, &value[..8]]
        };
        let error = refused_key(&KeyEnv::value(value), secrets);
        assert!(
            error.starts_with(&format!("BRIDGE_ENCRYPTION_KEY {problem}; ")),
            "{error}"
        );
    }
}

#[test]
fn unusable_encryption_key_file() {
    let too_long = format!("{KEY}{}", " ".repeat(1024));
    for (contents, problem) in [
        ("", "is empty"),
        ("\n\n", "is empty"),
        ("s3cret-is-not-base64!\n", "is not base64"),
        ("i6n08ZhvCMQSMfsPMru0Tw==\n", "decodes to 16 bytes, not 32"),
        (
            "XI5RQJRCfpAnYDpji+U84BcbVNSWHdXV0RL2qChwW5Qj\n",
            "decodes to 33 bytes, not 32",
        ),
        (&too_long, "is longer than 1024 bytes, too long for a key"),
    ] {
        let secrets: &[&str] = if contents.trim().is_empty() {
            &[]
        } else {
            &[contents.trim(), &contents[..8]]
        };
        let error = refused_key(&KeyEnv::file(contents), secrets);
        assert!(
            error.starts_with(&format!(
                "BRIDGE_ENCRYPTION_KEY_FILE: the file it names {problem}; "
            )),
            "{error}"
        );
    }
}

/// An endless file, like `/dev/zero`, is refused rather than read forever.
#[cfg(unix)]
#[test]
fn endless_encryption_key_file() {
    let error = refused_key(
        &KeyEnv {
            file: named_file("/dev/zero"),
            value: None,
        },
        &[],
    );
    assert!(error.contains("too long for a key"), "{error}");
}

#[test]
fn missing_encryption_key_file() {
    let error = refused_key(
        &KeyEnv {
            file: named_file("no-such-key-file"),
            value: None,
        },
        &[],
    );
    assert!(
        error.starts_with("BRIDGE_ENCRYPTION_KEY_FILE: cannot read the file it names: "),
        "{error}"
    );
}

/// Putting the key itself in the file variable by mistake never logs it: the error leaves the
/// path out.
#[test]
fn a_key_given_as_the_file_path_is_not_logged() {
    let error = refused_key(
        &KeyEnv {
            file: named_file(KEY),
            value: None,
        },
        &[KEY, &KEY[..8]],
    );
    assert!(
        error.starts_with("BRIDGE_ENCRYPTION_KEY_FILE: cannot read the file it names: "),
        "{error}"
    );
}

/// The key file wins even when it is unusable or missing: the Bridge never falls back to the
/// other.
#[test]
fn an_unusable_key_file_is_not_replaced_by_the_value() {
    for (file, problem) in [
        (KeyFile::Holding("not base64!".into()), "is not base64"),
        (KeyFile::Named("no-such-key-file".into()), "cannot read"),
    ] {
        let error = refused_key(
            &KeyEnv {
                file: Some(file),
                value: Some(KEY.into()),
            },
            &[KEY],
        );
        assert!(error.contains(problem), "{error}");
    }
}

/// `--help` lists no value of the key variables.
#[test]
fn help_shows_no_key() {
    let (dir, _config) = config_file("");
    std::fs::write(dir.path().join("key"), KEY).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_bridge"))
        .arg("--help")
        .env("BRIDGE_ENCRYPTION_KEY", KEY)
        .env("BRIDGE_ENCRYPTION_KEY_FILE", dir.path().join("key"))
        .output()
        .expect("run bridge --help");
    assert!(output.status.success(), "{output:?}");
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(printed.contains("--config"), "{printed}");
    assert!(!printed.contains(KEY), "--help printed the key");
    assert!(
        !printed.contains(&KEY[..8]),
        "--help printed part of the key"
    );
}
