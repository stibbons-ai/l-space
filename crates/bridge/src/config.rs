//! The operator's configuration file (TOML).
//!
//! ```toml
//! listen = "0.0.0.0:8080"
//!
//! [[library_servers]]
//! url = "https://abs.example.com"
//! name = "Home"              # optional
//! ```

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::path::Path;

use serde::Deserialize;
use url::Url;

/// The Bridge's validated configuration.
#[derive(Debug)]
pub struct Config {
    /// Where the App API listens. Port 0 picks a free port.
    pub listen: SocketAddr,
    /// In file order; no two share a [`LibraryServerUrl`].
    pub library_servers: Vec<LibraryServerConfig>,
}

/// One configured Library Server.
#[derive(Debug)]
pub struct LibraryServerConfig {
    pub url: LibraryServerUrl,
    pub name: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    listen: SocketAddr,
    #[serde(default)]
    library_servers: Vec<RawLibraryServer>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLibraryServer {
    url: String,
    name: Option<String>,
}

/// Why the configuration was rejected. URL errors name the offending entry, and its value only
/// as far as it is safe to show; no error repeats a secret (a password, a query or fragment, a
/// TOML source line, a value quoted by the TOML parser).
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read the file: {0}")]
    Read(io::Error),
    /// The parser's message, with everything from its first to its last double quote replaced
    /// by `REDACTED`, and its position if it has one; never the source line it usually quotes.
    #[error("not valid configuration TOML{}: {message}", at(.position))]
    Parse {
        message: String,
        /// 1-based line and column.
        position: Option<(usize, usize)>,
    },
    #[error("library_servers[{index}].url{}: {reason}", quoted(.url))]
    InvalidUrl {
        index: usize,
        /// What [`shown`] allows of the value.
        url: Option<String>,
        reason: UrlError,
    },
    #[error(
        "library_servers[{index}].url is the same Library Server as library_servers[{first}]: both normalize to {normalized}"
    )]
    Duplicate {
        index: usize,
        first: usize,
        normalized: LibraryServerUrl,
    },
}

fn at(position: &Option<(usize, usize)>) -> String {
    position.map_or_else(String::new, |(line, column)| {
        format!(" at line {line}, column {column}")
    })
}

fn quoted(url: &Option<String>) -> String {
    url.as_ref()
        .map_or_else(String::new, |url| format!(" {url:?}"))
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(ConfigError::Read)?;
        text.parse()
    }
}

impl std::str::FromStr for Config {
    type Err = ConfigError;

    fn from_str(text: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(text).map_err(|error| parse_error(text, &error))?;
        let mut library_servers: Vec<LibraryServerConfig> =
            Vec::with_capacity(raw.library_servers.len());
        for (index, server) in raw.library_servers.into_iter().enumerate() {
            let url =
                LibraryServerUrl::parse(&server.url).map_err(|reason| ConfigError::InvalidUrl {
                    index,
                    url: shown(&server.url),
                    reason,
                })?;
            if let Some(first) = library_servers.iter().position(|known| known.url == url) {
                return Err(ConfigError::Duplicate {
                    index,
                    first,
                    normalized: url,
                });
            }
            library_servers.push(LibraryServerConfig {
                url,
                name: server.name,
            });
        }
        Ok(Self {
            listen: raw.listen,
            library_servers,
        })
    }
}

/// The TOML error without its source snippet, positioned by 1-based line and column. A missing
/// top-level field has no position (the parser reports it as the empty span at the start of
/// the file), so none is given; a field missing inside a table points at the table's header.
///
/// The message itself can quote a value too (`invalid type: string "s3cret", expected a
/// sequence`), so everything from its first to its last double quote becomes `REDACTED`.
/// Non-string values and key names stay.
fn parse_error(text: &str, error: &toml::de::Error) -> ConfigError {
    let missing_field = error.message().starts_with("missing field");
    let position = error
        .span()
        .filter(|span| !(missing_field && *span == (0..0)))
        .map(|span| {
            let before = text.get(..span.start).unwrap_or(text);
            let line_start = before.rfind('\n').map_or(0, |newline| newline + 1);
            let column = before[line_start..]
                .chars()
                .filter(|&c| c != '\r' && c != '\u{feff}')
                .count()
                + 1;
            (before.matches('\n').count() + 1, column)
        });
    let message = error.message();
    let message = match (message.find('"'), message.rfind('"')) {
        (Some(open), Some(close)) if open < close => {
            format!("{}\"REDACTED\"{}", &message[..open], &message[close + 1..])
        }
        _ => message.to_owned(),
    };
    ConfigError::Parse { message, position }
}

/// What of a rejected URL is safe to show in a log: scheme, host, port and path, plus
/// `?REDACTED` / `#REDACTED` where it has a query or fragment. Nothing at all when it contains
/// an `@` (a password may hide anywhere around it, see [`LibraryServerUrl`]), when the url crate
/// cannot parse it, when it is not written as `scheme://` (`ftp:/s3cret` would show a host the
/// operator never wrote), or when it has no host, because then any part of it may be a secret.
fn shown(input: &str) -> Option<String> {
    if input.contains('@') {
        return None;
    }
    let url = Url::parse(input).ok()?;
    if !written_with_scheme(input, &url) {
        return None;
    }
    let host = url.host_str()?;
    let port = url
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    let mut shown = format!("{}://{host}{port}{}", url.scheme(), url.path());
    if url.query().is_some() {
        shown.push_str("?REDACTED");
    }
    if url.fragment().is_some() {
        shown.push_str("#REDACTED");
    }
    Some(shown)
}

/// Whether `input` literally starts with `url`'s scheme and `://`. Otherwise what the url crate
/// took for a scheme, host or path may be something else the operator wrote, so it is not
/// repeated.
fn written_with_scheme(input: &str, url: &Url) -> bool {
    let prefix = format!("{}://", url.scheme());
    input
        .trim_start()
        .get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(&prefix))
}

/// A Library Server's normalized base URL, which identifies it (ADR 0006: Audiobookshelf has no
/// stable server id).
///
/// Normalization, so that two spellings of one server compare equal:
/// - only `http` and `https`;
/// - scheme and host lowercased;
/// - the scheme's default port dropped (`:80` for http, `:443` for https), other ports kept;
/// - the path kept, since Audiobookshelf may live under a subpath, but trailing slashes dropped;
///   `https://abs.example.com/` becomes `https://abs.example.com`;
/// - a query, a fragment, or a user name or password is rejected rather than dropped, because
///   it means the operator pasted something other than a base URL;
/// - any `@` is rejected as a user name or password. A password containing an unencoded `/`,
///   `\`, `?` or `#` would otherwise end the URL's authority early and turn its tail into a
///   host, port, path, query or fragment that looks valid.
///
/// The rest is the url crate's WHATWG parsing: `.` and `..` path segments are resolved,
/// percent-encoding is kept as written, a trailing-dot host (`abs.lan.`) stays distinct,
/// international domain names become punycode, and lenient forms such as `http:///abs` or
/// `http:abs.lan` are accepted as `http://abs`/`http://abs.lan`.
#[derive(Debug, PartialEq, Eq)]
pub struct LibraryServerUrl(String);

/// Why a string is not a Library Server base URL.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum UrlError {
    #[error("not a valid URL: {0}")]
    Malformed(url::ParseError),
    #[error("expected an http:// or https:// URL")]
    MissingScheme,
    #[error("unsupported scheme {0:?}, expected http or https")]
    UnsupportedScheme(String),
    #[error("must not contain `@` (it may hide a user name or password)")]
    Userinfo,
    #[error("must not contain a query")]
    Query,
    #[error("must not contain a fragment")]
    Fragment,
}

impl LibraryServerUrl {
    pub fn parse(input: &str) -> Result<Self, UrlError> {
        if input.contains('@') {
            return Err(UrlError::Userinfo);
        }
        // The url crate already lowercases the scheme and host and forgets a default port.
        let url = Url::parse(input).map_err(|error| match error {
            url::ParseError::RelativeUrlWithoutBase => UrlError::MissingScheme,
            error => UrlError::Malformed(error),
        })?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(if written_with_scheme(input, &url) {
                UrlError::UnsupportedScheme(url.scheme().to_owned())
            } else {
                UrlError::MissingScheme
            });
        }
        if url.query().is_some() {
            return Err(UrlError::Query);
        }
        if url.fragment().is_some() {
            return Err(UrlError::Fragment);
        }
        // What is left serializes as `scheme://host[:port]/path`.
        let mut normalized = String::from(url);
        normalized.truncate(normalized.trim_end_matches('/').len());
        Ok(Self(normalized))
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for LibraryServerUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalized(input: &str) -> String {
        LibraryServerUrl::parse(input)
            .unwrap_or_else(|error| panic!("{input:?} rejected: {error}"))
            .0
    }

    #[test]
    fn lowercases_scheme_and_host_but_not_path() {
        assert_eq!(
            normalized("HTTPS://ABS.Example.COM/Audio"),
            "https://abs.example.com/Audio"
        );
    }

    #[test]
    fn drops_default_ports_and_keeps_others() {
        assert_eq!(normalized("http://abs.lan:80"), "http://abs.lan");
        assert_eq!(normalized("https://abs.lan:443"), "https://abs.lan");
        assert_eq!(normalized("http://abs.lan:443"), "http://abs.lan:443");
        assert_eq!(normalized("https://abs.lan:80"), "https://abs.lan:80");
        assert_eq!(normalized("http://abs.lan:13378"), "http://abs.lan:13378");
    }

    #[test]
    fn drops_trailing_slashes() {
        assert_eq!(normalized("https://abs.lan/"), "https://abs.lan");
        assert_eq!(normalized("https://abs.lan"), "https://abs.lan");
        assert_eq!(normalized("https://abs.lan/abs/"), "https://abs.lan/abs");
        assert_eq!(normalized("https://abs.lan/abs//"), "https://abs.lan/abs");
    }

    #[test]
    fn keeps_subpaths() {
        assert_eq!(
            normalized("https://home.lan:8443/media/abs"),
            "https://home.lan:8443/media/abs"
        );
    }

    #[test]
    fn accepts_ip_hosts() {
        assert_eq!(
            normalized("http://192.168.1.5:13378/"),
            "http://192.168.1.5:13378"
        );
        assert_eq!(
            normalized("http://[FD00::1]:80/abs"),
            "http://[fd00::1]/abs"
        );
    }

    #[test]
    fn rejects_other_schemes() {
        assert_eq!(
            LibraryServerUrl::parse("ftp://abs.lan"),
            Err(UrlError::UnsupportedScheme("ftp".into()))
        );
        for input in ["", "abs.lan", "abs.lan:13378", "ftp:/abs.lan"] {
            assert_eq!(
                LibraryServerUrl::parse(input),
                Err(UrlError::MissingScheme),
                "{input:?}"
            );
        }
    }

    #[test]
    fn rejects_malformed_urls() {
        for input in ["http://", "https://abs lan", "http://abs.lan:99999"] {
            assert!(
                matches!(LibraryServerUrl::parse(input), Err(UrlError::Malformed(_))),
                "{input:?} accepted or rejected for another reason"
            );
        }
    }

    #[test]
    fn rejects_userinfo() {
        for input in [
            "https://alice@abs.lan",
            "https://alice:secret@abs.lan",
            "https://:secret@abs.lan",
            // Unencoded `/ \ ? #` in a password end the authority early in the url crate.
            "https://alice:hun@ter/2@abs.lan",
            "https://alice:12#ss@abs.lan",
            "https://alice:123/s3cret@abs.lan",
            "https://al/ice:pw@abs.lan?x",
            "https://alice:a\\b@abs.lan",
            "https://abs.lan/a@b",
        ] {
            assert_eq!(
                LibraryServerUrl::parse(input),
                Err(UrlError::Userinfo),
                "{input:?}"
            );
        }
    }

    #[test]
    fn rejects_queries_and_fragments_even_empty_ones() {
        for input in ["https://abs.lan/?token=x", "https://abs.lan?"] {
            assert_eq!(
                LibraryServerUrl::parse(input),
                Err(UrlError::Query),
                "{input:?}"
            );
        }
        for input in ["https://abs.lan/#login", "https://abs.lan#"] {
            assert_eq!(
                LibraryServerUrl::parse(input),
                Err(UrlError::Fragment),
                "{input:?}"
            );
        }
    }

    #[test]
    fn rejects_two_entries_for_one_server() {
        let error = r#"
            listen = "127.0.0.1:0"
            [[library_servers]]
            url = "https://abs.lan/"
            [[library_servers]]
            url = "http://other.lan"
            [[library_servers]]
            url = "HTTPS://abs.lan:443"
        "#
        .parse::<Config>()
        .unwrap_err();
        assert!(
            matches!(
                &error,
                ConfigError::Duplicate { index: 2, first: 0, normalized, .. }
                    if normalized.to_string() == "https://abs.lan"
            ),
            "{error:?}"
        );
    }

    #[test]
    fn keeps_servers_that_differ_only_in_path() {
        let config = r#"
            listen = "127.0.0.1:0"
            [[library_servers]]
            url = "https://home.lan/abs"
            [[library_servers]]
            url = "https://home.lan/abs2"
        "#
        .parse::<Config>()
        .unwrap();
        assert_eq!(config.library_servers.len(), 2);
    }

    #[test]
    fn keeps_a_password_out_of_the_error() {
        let error = r#"
            listen = "127.0.0.1:0"
            [[library_servers]]
            url = "https://alice:hunter2@abs.lan"
        "#
        .parse::<Config>()
        .unwrap_err();
        let message = error.to_string();
        assert!(!message.contains("hunter2"), "{message}");
        assert!(message.contains("library_servers[0]"), "{message}");
    }

    #[test]
    fn shows_only_scheme_host_port_and_path_of_a_rejected_url() {
        for (input, expected) in [
            (
                "https://abs.lan/?token=s3cret",
                Some("https://abs.lan/?REDACTED"),
            ),
            (
                "https://abs.lan/a#s3cret",
                Some("https://abs.lan/a#REDACTED"),
            ),
            ("ftp://abs.lan/books", Some("ftp://abs.lan/books")),
            ("https://abs lan", None),
            ("abs.lan/s3cret", None),
            ("ftp:/s3cret", None),
            ("http:abs.lan/?x", None),
            ("https://abs.lan/?user=a@b.c&token=s3cret", None),
            ("https://abs.lan/#tok@s3cret", None),
            ("http:alice:s3cret@abs.lan/?r=https://x", None),
            ("https://alice:hun@ter/2@abs.lan", None),
            ("https://alice:12#ss@abs.lan", None),
            ("https://alice:123/s3cret@abs.lan", None),
            ("https://al/ice:pw@abs.lan?x", None),
            ("alice:s3cret@abs.lan", None),
        ] {
            assert_eq!(shown(input).as_deref(), expected, "{input:?}");
        }
    }

    #[test]
    fn positions_toml_errors_without_quoting_the_source() {
        let error = "listen = \"127.0.0.1:0\"\n  token = \"s3cret\"\n"
            .parse::<Config>()
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("line 2, column 3"), "{message}");
        assert!(!message.contains("s3cret"), "{message}");
    }

    #[test]
    fn counts_toml_columns_in_characters_without_carriage_returns() {
        for (text, position) in [
            (
                "listen = \"127.0.0.1:0\"\r\ntoken = 1\r\n",
                "line 2, column 1",
            ),
            // An unterminated string is reported at the `\r` that ends its line.
            ("listen = \"127.0.0.1:0\r\n", "line 1, column 22"),
            (
                "listen = \"127.0.0.1:0\"\nname = \"äöü\" x\n",
                "line 2, column 14",
            ),
        ] {
            let message = text.parse::<Config>().unwrap_err().to_string();
            assert!(message.contains(position), "{text:?}: {message}");
        }
    }

    #[test]
    fn gives_no_position_when_the_parser_has_none() {
        let message = "".parse::<Config>().unwrap_err().to_string();
        assert!(message.contains("listen"), "{message}");
        assert!(!message.contains("line"), "{message}");
    }

    #[test]
    fn positions_a_field_missing_inside_a_table_at_its_header() {
        let text = "listen = \"127.0.0.1:0\"\n\
                    [[library_servers]]\nurl = \"https://a.lan\"\n\
                    [[library_servers]]\nname = \"Home\"\n";
        let message = text.parse::<Config>().unwrap_err().to_string();
        assert!(message.contains("missing field `url`"), "{message}");
        assert!(message.contains("line 4, column 1"), "{message}");
    }

    #[test]
    fn positions_an_error_at_the_very_start() {
        let message = "= 1\n".parse::<Config>().unwrap_err().to_string();
        assert!(message.contains("line 1, column 1"), "{message}");
    }

    #[test]
    fn does_not_count_a_byte_order_mark_as_a_column() {
        let message = "\u{feff}token = 1\n"
            .parse::<Config>()
            .unwrap_err()
            .to_string();
        assert!(message.contains("line 1, column 1"), "{message}");
    }
}
