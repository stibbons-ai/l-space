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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "StoredFile")]
pub struct Stored {
    /// The Bridge's base URL, once the person entered one that answered.
    pub bridge_url: Option<String>,
    /// Every session, at most one per Library Server.
    sessions: Vec<StoredSession>,
    /// The Library Server (its base URL) of the session the App uses; `None` once that one
    /// ended, until the person picks another.
    selected: Option<String>,
    pub language: LanguageChoice,
}

impl Stored {
    /// The session the App uses, if one is selected.
    pub fn session(&self) -> Option<&StoredSession> {
        let selected = self.selected.as_deref()?;
        self.sessions
            .iter()
            .find(|session| session.library_server_url() == selected)
    }

    /// Every session, the selected one among them.
    pub fn sessions(&self) -> &[StoredSession] {
        &self.sessions
    }

    /// Sets the account of the selected session; whether that changed it.
    pub fn update_user(&mut self, user: CurrentUser) -> bool {
        let selected = self.selected.as_deref();
        let session = self
            .sessions
            .iter_mut()
            .find(|session| Some(session.library_server_url()) == selected);
        match session {
            Some(session) if session.user != user => {
                session.user = user;
                true
            }
            _ => false,
        }
    }

    /// Selects the session on the Library Server with base URL `library_server`; returns it,
    /// or `None` (selecting nothing new) if there is none.
    pub fn select(&mut self, library_server: &str) -> Option<&StoredSession> {
        let index = self
            .sessions
            .iter()
            .position(|session| session.library_server_url() == library_server)?;
        self.selected = Some(library_server.to_owned());
        Some(&self.sessions[index])
    }

    /// Adds `session` and selects it. Returns the session it replaces: the one on the same
    /// Library Server, if any.
    pub fn add(&mut self, session: StoredSession) -> Option<StoredSession> {
        let library_server = session.library_server_url().to_owned();
        let replaced = match self
            .sessions
            .iter_mut()
            .find(|stored| stored.library_server_url() == library_server)
        {
            Some(stored) => Some(std::mem::replace(stored, session)),
            None => {
                self.sessions.push(session);
                None
            }
        };
        self.selected = Some(library_server);
        replaced
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
        // At most one per Library Server, whatever the file holds.
        for session in file.sessions {
            stored.add(session);
        }
        stored.selected = file.selected;
        // An earlier App's one session is the one it used.
        if let Some(session) = file.session {
            stored.add(session);
        }
        stored
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
    /// The base URL of the Library Server the session is bound to, as the Bridge names it.
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
