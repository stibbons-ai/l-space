//! What the App keeps on the device across launches, behind one seam ([`DeviceStore`]) so that
//! a Keychain implementation can replace the file later.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use api::{CurrentUser, LibraryServer, SessionToken};
use serde::{Deserialize, Deserializer, Serialize};

use crate::i18n::LanguageChoice;

/// Everything the App remembers. The session tokens are the only secrets; their `Debug`
/// output is redacted ([`SessionToken`]).
///
/// Reads what earlier Apps wrote too: their single `session` becomes the only session, selected.
/// A file holding several sessions on one Library Server keeps the last; a `selected` naming
/// none of the sessions selects nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "StoredFile")]
pub struct Stored {
    /// The Bridge's base URL, once the person entered one that answered.
    pub bridge_url: Option<String>,
    /// Every session, at most one per Library Server.
    sessions: Vec<StoredSession>,
    /// The Library Server (its base URL) of the session the App uses; `None` when none is
    /// selected (it ended, or the file named none), until the person switches or logs in.
    selected: Option<String>,
    pub language: LanguageChoice,
}

impl Stored {
    /// The session the App uses, if one is selected.
    pub fn session(&self) -> Option<&StoredSession> {
        Some(&self.sessions[self.selected_position()?])
    }

    /// The sessions other than the selected one, to switch to.
    pub fn other_sessions(&self) -> impl Iterator<Item = &StoredSession> {
        self.sessions
            .iter()
            .filter(|session| Some(session.library_server_url()) != self.selected.as_deref())
    }

    /// Whether any session is stored, selected or not.
    pub fn has_sessions(&self) -> bool {
        !self.sessions.is_empty()
    }

    /// Sets the account of the selected session; whether that changed it.
    pub fn update_user(&mut self, user: CurrentUser) -> bool {
        let Some(index) = self.selected_position() else {
            return false;
        };
        let session = &mut self.sessions[index];
        let changed = session.user != user;
        session.user = user;
        changed
    }

    /// Where the selected session is, if one is selected.
    fn selected_position(&self) -> Option<usize> {
        self.position(self.selected.as_deref()?)
    }

    /// Selects the session on the Library Server with base URL `library_server`; whether there
    /// is one (without, the selection stays).
    pub fn select(&mut self, library_server: &str) -> bool {
        let found = self.position(library_server).is_some();
        if found {
            self.selected = Some(library_server.to_owned());
        }
        found
    }

    /// Adds `session` and selects it. Returns the session it replaces: the one on the same
    /// Library Server, if any.
    pub fn add(&mut self, session: StoredSession) -> Option<StoredSession> {
        let library_server = session.library_server_url().to_owned();
        let replaced = match self.position(&library_server) {
            Some(index) => Some(std::mem::replace(&mut self.sessions[index], session)),
            None => {
                self.sessions.push(session);
                None
            }
        };
        self.selected = Some(library_server);
        replaced
    }

    /// Where the session on the Library Server with base URL `library_server` is, if any.
    fn position(&self, library_server: &str) -> Option<usize> {
        self.sessions
            .iter()
            .position(|session| session.library_server_url() == library_server)
    }

    /// Forgets the selected session; the others stay, none selected.
    pub fn forget_selected(&mut self) {
        if let Some(selected) = self.selected.take() {
            self.sessions
                .retain(|session| session.library_server_url() != selected);
        }
    }

    /// Forgets every session.
    pub fn forget_all(&mut self) {
        self.sessions.clear();
        self.selected = None;
    }
}

/// [`Stored`] as the file holds it, written by this App or an earlier one.
#[derive(Deserialize)]
struct StoredFile {
    bridge_url: Option<String>,
    /// The one session of an App from before several sessions.
    #[serde(default)]
    session: Option<StoredSession>,
    #[serde(default)]
    sessions: Vec<StoredSession>,
    #[serde(default)]
    selected: Option<String>,
    /// A value this build doesn't know (from a newer App) reads as [`LanguageChoice::System`].
    #[serde(default, deserialize_with = "language_or_system")]
    language: LanguageChoice,
}

impl From<StoredFile> for Stored {
    fn from(file: StoredFile) -> Self {
        let mut stored = Self {
            bridge_url: file.bridge_url,
            language: file.language,
            ..Self::default()
        };
        // At most one per Library Server, whatever the file holds: the last one wins.
        for session in file.sessions {
            stored.add_loaded(session);
        }
        stored.selected = file.selected.filter(|url| {
            let found = stored.position(url).is_some();
            if !found {
                tracing::warn!(
                    library_server = url.as_str(),
                    "the saved state selects a Library Server without a session; selecting none"
                );
            }
            found
        });
        // An earlier App's one session is the one it used; it wins over `sessions` (which such
        // an App never wrote).
        if let Some(session) = file.session {
            stored.add_loaded(session);
        }
        stored
    }
}

impl Stored {
    /// [`Stored::add`] while loading, saying when the file held another session on the same
    /// Library Server (named by its URL only, never a token).
    fn add_loaded(&mut self, session: StoredSession) {
        if let Some(dropped) = self.add(session) {
            tracing::warn!(
                library_server = dropped.library_server_url(),
                "the saved state holds two sessions on one Library Server; keeping the later one"
            );
        }
    }
}

fn language_or_system<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<LanguageChoice, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(LanguageChoice::deserialize(value).unwrap_or_default())
}

/// A logged-in session on the stored Bridge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredSession {
    pub token: SessionToken,
    /// The Library Server as the Bridge listed it at login, for Settings.
    pub library_server: LibraryServer,
    /// Who the session acts for, as last confirmed by the Bridge.
    pub user: CurrentUser,
}

impl StoredSession {
    /// The base URL of the Library Server the session is bound to, as the Bridge confirmed it
    /// for the account (`user.library_server`).
    pub fn library_server_url(&self) -> &str {
        &self.user.library_server
    }
}

/// Where the App keeps [`Stored`] on the device.
pub trait DeviceStore: fmt::Debug + Send + Sync {
    /// What was saved last, or [`Stored::default`] if nothing was ever saved.
    fn load(&self) -> Result<Stored, LoadError>;

    /// Replaces what is saved with `stored`.
    fn save(&self, stored: &Stored) -> io::Result<()>;
}

/// Why [`DeviceStore::load`] has nothing to give.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// What was saved couldn't be read, and was moved aside: saving again is safe.
    #[error("cannot read the saved state ({error}); kept it as {}", kept_as.display())]
    SetAside { error: io::Error, kept_as: PathBuf },
    /// What was saved could neither be read nor moved aside: saving again would destroy it.
    #[error("cannot read the saved state ({error}), nor set it aside ({set_aside_error})")]
    Stuck {
        error: io::Error,
        set_aside_error: io::Error,
    },
}

/// [`Stored`] as one JSON file, readable by its owner only.
#[derive(Debug, Clone)]
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The file in the platform's application data directory (on iOS, inside the App's
    /// sandbox), or `None` if the platform has none.
    pub fn in_data_dir() -> Option<Self> {
        Some(Self::new(
            dirs::data_dir()?.join("ai.stibbons.ook").join("app.json"),
        ))
    }
}

impl DeviceStore for FileStore {
    /// A file that can't be read, or isn't valid JSON for [`Stored`], is moved aside to the
    /// first free `app.json.bad`, `app.json.bad.2`, … next to it ([`LoadError::SetAside`]); if
    /// it can't be moved, it stays where it is ([`LoadError::Stuck`]).
    fn load(&self) -> Result<Stored, LoadError> {
        let error = match std::fs::read(&self.path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(stored) => return Ok(stored),
                Err(error) => io::Error::new(io::ErrorKind::InvalidData, error),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Stored::default()),
            Err(error) => error,
        };
        match self.set_aside() {
            Ok(kept_as) => Err(LoadError::SetAside { error, kept_as }),
            Err(set_aside_error) => Err(LoadError::Stuck {
                error,
                set_aside_error,
            }),
        }
    }

    fn save(&self, stored: &Stored) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_vec_pretty(stored).map_err(io::Error::other)?;
        // Write a sibling, flush it to disk and rename it over the file, so a crash leaves
        // either the old file or the new one, never half of one.
        let partial = self.path.with_extension("json.partial");
        write_private(&partial, &json)?;
        std::fs::rename(&partial, &self.path)
    }
}

impl FileStore {
    /// Moves the file to the first `.bad` name not taken, never replacing an earlier one.
    fn set_aside(&self) -> io::Result<PathBuf> {
        for n in 1.. {
            let candidate = match n {
                1 => self.path.with_extension("json.bad"),
                n => self.path.with_extension(format!("json.bad.{n}")),
            };
            // A hard link fails instead of replacing an existing file.
            match std::fs::hard_link(&self.path, &candidate) {
                Ok(()) => {
                    // The copy is safe; a save may replace the original later.
                    if let Err(error) = std::fs::remove_file(&self.path) {
                        tracing::warn!(%error, "kept a copy, but cannot remove the original");
                    }
                    return Ok(candidate);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                // No hard links here: a rename to a name not taken does the same.
                Err(_) if !candidate.exists() => {
                    std::fs::rename(&self.path, &candidate)?;
                    return Ok(candidate);
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("an unbounded range")
    }
}

/// Writes `contents` to a new or truncated file only its owner can read.
fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    // `mode` applies only when the file is created.
    #[cfg(unix)]
    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    file.write_all(contents)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use api::{CurrentUser, LibraryServer, SessionToken};
    use serde_json::json;

    use super::{Stored, StoredSession};

    fn session(library_server: &str, token: &str) -> StoredSession {
        StoredSession {
            token: SessionToken::new(token),
            library_server: LibraryServer {
                base_url: library_server.to_owned(),
                name: None,
            },
            user: CurrentUser {
                library_server: library_server.to_owned(),
                user_id: "u1".to_owned(),
                username: "anna".to_owned(),
            },
        }
    }

    fn load(json: serde_json::Value) -> Stored {
        serde_json::from_value(json).expect("a readable file")
    }

    fn tokens<'a>(sessions: impl Iterator<Item = &'a StoredSession>) -> Vec<&'a str> {
        sessions.map(|session| session.token.expose()).collect()
    }

    #[test]
    fn an_earlier_apps_one_session_becomes_the_selected_one() {
        let stored = load(json!({
            "bridge_url": "http://bridge",
            "session": session("http://home", "t-home"),
            "language": "system",
        }));
        assert_eq!(stored.session(), Some(&session("http://home", "t-home")));
        assert_eq!(tokens(stored.other_sessions()), Vec::<&str>::new());
    }

    #[test]
    fn one_session_per_library_server_the_last_in_the_file_wins() {
        let stored = load(json!({
            "bridge_url": "http://bridge",
            "sessions": [
                session("http://home", "t-old"),
                session("http://second", "t-second"),
                session("http://home", "t-new"),
            ],
            "selected": "http://home",
        }));
        assert_eq!(stored.session().map(|s| s.token.expose()), Some("t-new"));
        assert_eq!(tokens(stored.other_sessions()), ["t-second"]);
    }

    #[test]
    fn a_selected_library_server_without_a_session_selects_nothing() {
        let stored = load(json!({
            "bridge_url": "http://bridge",
            "sessions": [session("http://home", "t-home")],
            "selected": "http://gone",
        }));
        assert_eq!(stored.session(), None);
        assert_eq!(tokens(stored.other_sessions()), ["t-home"]);
        // Saved again, the dangling selection is gone.
        let saved = serde_json::to_value(&stored).unwrap();
        assert_eq!(saved["selected"], serde_json::Value::Null);
    }

    #[test]
    fn an_old_session_beside_sessions_is_kept_and_selected() {
        let stored = load(json!({
            "bridge_url": "http://bridge",
            "session": session("http://home", "t-old-app"),
            "sessions": [
                session("http://home", "t-home"),
                session("http://second", "t-second"),
            ],
            "selected": "http://second",
        }));
        assert_eq!(
            stored.session().map(|s| s.token.expose()),
            Some("t-old-app")
        );
        assert_eq!(tokens(stored.other_sessions()), ["t-second"]);
    }

    #[test]
    fn debug_output_shows_no_token() {
        let stored = load(json!({
            "bridge_url": "http://bridge",
            "sessions": [
                session("http://home", "secret-home-token"),
                session("http://second", "secret-second-token"),
            ],
            "selected": "http://home",
        }));
        let debug = format!("{stored:?}");
        assert!(!debug.contains("secret"), "{debug}");
    }
}
