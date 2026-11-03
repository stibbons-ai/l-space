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

/// Why the configuration was rejected. URL errors name the offending entry and its value; no
/// error repeats a secret (a password, a query or fragment value, a TOML source line).
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read the file: {0}")]
    Read(io::Error),
    /// Only the parser's message and position: its usual rendering quotes the source line,
    /// which may hold a secret.
    #[error("not valid configuration TOML at line {line}, column {column}: {message}")]
    Parse {
        message: String,
        line: usize,
        column: usize,
    },
    #[error("library_servers[{index}].url {url:?}: {reason}")]
    InvalidUrl {
        index: usize,
        url: String,
        reason: UrlError,
    },
    #[error(
        "library_servers[{index}].url {url:?} is the same Library Server as library_servers[{first}]: both normalize to {normalized}"
    )]
    Duplicate {
        index: usize,
        url: String,
        first: usize,
        normalized: LibraryServerUrl,
    },
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
                    url: redact(&server.url),
                    reason,
                })?;
            if let Some(first) = library_servers.iter().position(|known| known.url == url) {
                return Err(ConfigError::Duplicate {
                    index,
                    url: redact(&server.url),
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

/// The TOML error without its source snippet, positioned by 1-based line and column.
///
/// The message itself can quote a value too (`invalid type: string "s3cret", expected a
/// sequence`), so anything between its first and last double quote becomes `REDACTED`. Field
/// names are quoted with backticks and stay.
fn parse_error(text: &str, error: &toml::de::Error) -> ConfigError {
    let offset = error.span().map_or(0, |span| span.start).min(text.len());
    let before = text.get(..offset).unwrap_or(text);
    let line_start = before.rfind('\n').map_or(0, |newline| newline + 1);
    let message = error.message();
    let message = match (message.find('"'), message.rfind('"')) {
        (Some(open), Some(close)) if open < close => {
            format!("{}\"REDACTED\"{}", &message[..open], &message[close + 1..])
        }
        _ => message.to_owned(),
    };
    ConfigError::Parse {
        message,
        line: before.matches('\n').count() + 1,
        column: before[line_start..].chars().count() + 1,
    }
}

/// A URL as the operator wrote it, minus anything that may be a secret: the user name and
/// password, query values and the fragment become `REDACTED`.
///
/// Plain string handling, so it also works on input the url crate cannot parse. Everything
/// up to the last `@` counts as userinfo, so a password containing `/`, `?` or `#` is still
/// hidden.
fn redact(input: &str) -> String {
    let (scheme, rest) = input
        .find("://")
        .map_or(("", input), |i| input.split_at(i + 3));
    let (userinfo, rest) = match rest.rfind('@') {
        Some(at) => ("REDACTED@", &rest[at + 1..]),
        None => ("", rest),
    };
    let (rest, fragment) = match rest.split_once('#') {
        Some((rest, "")) => (rest, "#"),
        Some((rest, _)) => (rest, "#REDACTED"),
        None => (rest, ""),
    };
    let (base, query) = rest
        .split_once('?')
        .map_or((rest, None), |(b, q)| (b, Some(q)));
    let mut redacted = format!("{scheme}{userinfo}{base}");
    if let Some(query) = query {
        redacted.push('?');
        for (i, pair) in query.split('&').enumerate() {
            if i > 0 {
                redacted.push('&');
            }
            match pair.split_once('=') {
                Some((key, _)) => redacted.push_str(&format!("{key}=REDACTED")),
                None if pair.is_empty() => {}
                None => redacted.push_str("REDACTED"),
            }
        }
    }
    redacted.push_str(fragment);
    redacted
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
///   it means the operator pasted something other than a base URL.
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
    Malformed(#[from] url::ParseError),
    #[error("unsupported scheme {0:?}, expected http or https")]
    UnsupportedScheme(String),
    #[error("must not contain a user name or password")]
    Userinfo,
    #[error("must not contain a query")]
    Query,
    #[error("must not contain a fragment")]
    Fragment,
}

impl LibraryServerUrl {
    pub fn parse(input: &str) -> Result<Self, UrlError> {
        // The url crate already lowercases the scheme and host and forgets a default port.
        let url = Url::parse(input)?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(UrlError::UnsupportedScheme(url.scheme().to_owned()));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(UrlError::Userinfo);
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
        assert_eq!(
            LibraryServerUrl::parse("abs.lan:13378"),
            Err(UrlError::UnsupportedScheme("abs.lan".into()))
        );
    }

    #[test]
    fn rejects_malformed_urls() {
        for input in [
            "",
            "abs.lan",
            "http://",
            "https://abs lan",
            "http://abs.lan:99999",
        ] {
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
    fn redacts_userinfo_query_values_and_fragments_even_when_unparseable() {
        for (input, expected) in [
            ("https://alice:hunter2@abs lan", "https://REDACTED@abs lan"),
            (
                "https://alice:hun@ter/2@abs.lan:99999",
                "https://REDACTED@abs.lan:99999",
            ),
            ("alice:hunter2@abs.lan", "REDACTED@abs.lan"),
            (
                "https://abs.lan/?token=s3cret&x",
                "https://abs.lan/?token=REDACTED&REDACTED",
            ),
            ("https://abs.lan/a#s3cret", "https://abs.lan/a#REDACTED"),
            ("https://abs.lan?#", "https://abs.lan?#"),
            ("ftp://abs.lan/books", "ftp://abs.lan/books"),
        ] {
            assert_eq!(redact(input), expected, "{input:?}");
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
}
