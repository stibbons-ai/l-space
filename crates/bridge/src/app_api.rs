//! The App API's view of this Bridge: configured Library Servers, logins and sessions,
//! browsing a session's Library Server as its user, the household's Sonos Groups, and
//! Handoffs.

use std::collections::HashMap;
use std::fmt::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use api::server::{BoxFuture, Bridge};
use api::{
    ApiError, CurrentUser, EpisodeFilter, Group, GroupMember, HOME_ENTRIES, Handoff, Home,
    HomeEntry, Item, ItemId, Library, LibraryEntry, LibraryServer, NowPlaying, Podcast, Session,
    SessionToken, TransportState,
};
use sonos::Sonos;
use store::{ActiveHandoff, HandoffFile, LibraryUser, SessionId, Store};

use crate::browse::{self, NoSingleFile};
use crate::config::{LibraryServerConfig, LibraryServerUrl};
use crate::stream::StreamToken;

/// How many more Items in progress than the user has Progress records the Bridge asks the
/// Library Server for, for Home. It lists one entry per Progress record at most, so the
/// records `/api/me` showed bound the list; the margin covers Progress first recorded (by
/// another of the user's players) between that read and the list's, which would otherwise
/// push the user's oldest entries out of it.
const IN_PROGRESS_MARGIN: usize = 10;

/// Serves the App API from the configured Library Servers, the [`Store`] and [`Sonos`]; and,
/// for the same Handoffs, the media streams (`crate::stream`).
#[derive(Debug)]
pub struct AppApi<S> {
    library_servers: Vec<LibraryServer>,
    pub(crate) abs: abs::Client,
    store: Arc<S>,
    sonos: Arc<dyn Sonos>,
    /// Where the Bridge listens, port 0 resolved: where Groups fetch streams.
    listen: SocketAddr,
    /// One lock per Group a start is under way on, so starts on one Group take turns from
    /// storing the Handoff to the Group playing it (or it ending again). An entry whose lock
    /// no start holds or waits for any more is dropped at the next start.
    starting: Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
}

impl<S: Store> AppApi<S> {
    pub fn new(
        library_servers: Vec<LibraryServerConfig>,
        store: S,
        sonos: Arc<dyn Sonos>,
        listen: SocketAddr,
    ) -> Self {
        let library_servers = library_servers
            .into_iter()
            .map(|server| LibraryServer {
                base_url: server.url.into_string(),
                name: server.name,
            })
            .collect();
        Self {
            library_servers,
            abs: abs::Client::new(),
            store: Arc::new(store),
            sonos,
            listen,
            starting: Mutex::default(),
        }
    }

    /// The base URL of the configured Library Server that `url` (in any spelling) normalizes
    /// to.
    fn configured(&self, url: &str) -> Option<&str> {
        self.configured_base_url(&LibraryServerUrl::parse(url).ok()?.into_string())
    }

    /// `base_url` (already normalized), if it is a configured Library Server's.
    fn configured_base_url(&self, base_url: &str) -> Option<&str> {
        self.library_servers
            .iter()
            .map(|server| server.base_url.as_str())
            .find(|&configured| configured == base_url)
    }

    /// The active Handoff whose stream is `stream`, if `file` is its file's name, with its
    /// configured Library Server; none otherwise, alike for every reason.
    pub(crate) async fn active_handoff(
        &self,
        stream: &StreamToken,
        file: &str,
    ) -> Result<Option<(&str, ActiveHandoff)>, store::Error> {
        let Some(active) = self.store.handoff(&stream.hash()).await? else {
            return Ok(None);
        };
        if file != active.handoff.file.name {
            return Ok(None);
        }
        let started_by = &active.handoff.started_by;
        // The operator may have removed the Library Server since the start.
        let Some(library_server) = self.configured_base_url(&started_by.library_server) else {
            tracing::info!(
                group = active.handoff.group,
                library_server = started_by.library_server,
                "stream for a Library Server that is not configured"
            );
            return Ok(None);
        };
        Ok(Some((library_server, active)))
    }

    async fn login(
        &self,
        library_server: String,
        api_key: api::ApiKey,
    ) -> Result<Session, ApiError> {
        let library_server = self
            .configured(&library_server)
            .ok_or(ApiError::LibraryServerNotConfigured)?;
        let api_key = abs::ApiKey::from(api_key);
        let user = self
            .abs
            .me(library_server, &api_key)
            .await
            .map_err(|error| abs_error(library_server, Request::ME, error))?;

        let token = SessionToken::new(random_hex("a session token")?);
        let owner = LibraryUser {
            library_server: library_server.to_owned(),
            user_id: user.id,
        };
        self.store
            .open_session(&SessionId::of_token(token.expose()), &owner, &api_key)
            .await
            .map_err(|error| {
                tracing::error!(
                    operation = "open a session",
                    library_server = owner.library_server,
                    user_id = owner.user_id,
                    error = crate::error_chain(&error),
                    "storage failed"
                );
                ApiError::Internal
            })?;
        Ok(Session {
            token,
            user: CurrentUser {
                library_server: owner.library_server,
                user_id: owner.user_id,
                username: user.username,
            },
        })
    }

    async fn logout(&self, token: SessionToken) -> Result<(), ApiError> {
        let closed = self
            .store
            .close_session(&SessionId::of_token(token.expose()))
            .await
            .map_err(storage_error("close a session"))?;
        if closed {
            Ok(())
        } else {
            Err(ApiError::SessionInvalid)
        }
    }

    /// Asks the session's Library Server who its key belongs to.
    async fn current_user(&self, token: SessionToken) -> Result<CurrentUser, ApiError> {
        let (library_server, session) = self.session(&token).await?;
        let user = self.abs.me(library_server, &session.api_key).await;
        let user = self.verified(library_server, &session, user).await?;
        Ok(CurrentUser {
            library_server: library_server.to_owned(),
            user_id: user.id,
            username: user.username,
        })
    }

    async fn libraries(&self, token: SessionToken) -> Result<Vec<Library>, ApiError> {
        let request = Request {
            operation: "list the Libraries",
            id: None,
        };
        let browsed = self
            .browse(token, request, async |server, key| {
                self.abs.libraries(server, key).await
            })
            .await?;
        let libraries = browsed.fetched;
        warn_dropped(browsed.library_server, None, "Libraries", libraries.dropped);
        Ok(libraries.entries.into_iter().map(browse::library).collect())
    }

    async fn library_entries(
        &self,
        token: SessionToken,
        library: String,
    ) -> Result<Vec<LibraryEntry>, ApiError> {
        let request = Request {
            operation: "list a Library",
            id: Some(&library),
        };
        let items = self
            .browse(token, request, async |server, key| {
                self.abs.library_items(server, key, &library).await
            })
            .await?
            .fetched;
        Ok(items
            .into_iter()
            .filter_map(browse::library_entry)
            .collect())
    }

    async fn podcast(
        &self,
        token: SessionToken,
        podcast: String,
        filter: EpisodeFilter,
    ) -> Result<Podcast, ApiError> {
        let request = Request {
            operation: "fetch a podcast",
            id: Some(&podcast),
        };
        let Browsed {
            library_server,
            user,
            fetched: item,
            ..
        } = self
            .browse(token, request, async |server, key| {
                self.abs.library_item(server, key, &podcast).await
            })
            .await?;
        browse::podcast(&user, item, filter)
            .map_err(|reason| missing(library_server, request, None, reason))
    }

    /// Home for the session's user: the Items they are in the middle of, and the newest
    /// episodes they haven't Finished in every podcast Library they can see; in each list, at
    /// most [`HOME_ENTRIES`] the Library Server lets the user open.
    async fn home(&self, token: SessionToken) -> Result<Home, ApiError> {
        let request = Request {
            operation: "list the Libraries",
            id: None,
        };
        let Browsed {
            library_server,
            session,
            user,
            fetched: libraries,
        } = self
            .browse(token, request, async |server, key| {
                self.abs.libraries(server, key).await
            })
            .await?;
        warn_dropped(library_server, None, "Libraries", libraries.dropped);
        let records = user.media_progress.entries.len() + user.media_progress.dropped;
        let (in_progress, episodes) = tokio::join!(
            self.abs.items_in_progress(
                library_server,
                &session.api_key,
                records + IN_PROGRESS_MARGIN
            ),
            self.newest_episodes(library_server, &session, &user.id, &libraries.entries),
        );
        let in_progress = match in_progress {
            Ok(in_progress) => in_progress,
            Err(error) => {
                let request = Request {
                    operation: "list the Items in progress",
                    id: None,
                };
                return Err(self.failure(library_server, &session, request, error).await);
            }
        };
        let episodes = episodes?;
        warn_dropped(
            library_server,
            Some(&user.id),
            "Items in progress",
            in_progress.dropped,
        );
        let (continue_listening, unexpected) =
            browse::continue_listening(&user, &libraries.entries, in_progress.entries);
        warn_dropped(
            library_server,
            Some(&user.id),
            "Items in progress naming no Item",
            unexpected,
        );
        let mut may_open = HashMap::new();
        Ok(Home {
            continue_listening: self
                .openable(library_server, &session, continue_listening, &mut may_open)
                .await?,
            latest_episodes: self
                .openable(
                    library_server,
                    &session,
                    browse::latest_episodes(&user, episodes),
                    &mut may_open,
                )
                .await?,
        })
    }

    /// The first [`HOME_ENTRIES`] of `candidates` whose library item the session's user may
    /// open. The Library Server alone knows that (Libraries, explicit content, tags): each
    /// library item is read with the user's key, once per Home (`may_open` remembers the
    /// answers), the next candidates' at the same time, never more than [`HOME_ENTRIES`] at
    /// once.
    async fn openable(
        &self,
        library_server: &str,
        session: &store::Session,
        candidates: Vec<HomeEntry>,
        may_open: &mut HashMap<String, bool>,
    ) -> Result<Vec<HomeEntry>, ApiError> {
        let mut listed = Vec::new();
        let mut candidates = candidates.into_iter();
        while listed.len() < HOME_ENTRIES {
            let next: Vec<_> = candidates
                .by_ref()
                .take(HOME_ENTRIES - listed.len())
                .collect();
            if next.is_empty() {
                break;
            }
            let mut unknown: Vec<&str> = next
                .iter()
                .map(|entry| entry.id.library_item.as_str())
                .filter(|id| !may_open.contains_key(*id))
                .collect();
            unknown.sort_unstable();
            unknown.dedup();
            let read = futures::future::join_all(unknown.into_iter().map(|id| async move {
                (
                    id,
                    self.abs
                        .library_item(library_server, &session.api_key, id)
                        .await,
                )
            }))
            .await;
            for (id, read) in read {
                let open = match read {
                    Ok(_) => true,
                    Err(abs::Error::NotFound(cause)) => {
                        tracing::info!(
                            library_server,
                            library_item = id,
                            cause = ?cause,
                            "an Item the user may not open was left out of Home"
                        );
                        false
                    }
                    Err(error) => {
                        let request = Request {
                            operation: "check the user may open an Item on Home",
                            id: Some(id),
                        };
                        return Err(self.failure(library_server, session, request, error).await);
                    }
                };
                may_open.insert(id.to_owned(), open);
            }
            listed.extend(
                next.into_iter()
                    .filter(|entry| may_open[&entry.id.library_item]),
            );
        }
        Ok(listed)
    }

    /// The newest episodes the session's user (`user_id`) hasn't Finished in each podcast
    /// Library of `libraries`, at most [`HOME_ENTRIES`] of each by publish date (the Library
    /// Server's order, episodes without one last). A podcast Library gone or hidden from the
    /// user since `libraries` were listed adds none.
    async fn newest_episodes(
        &self,
        library_server: &str,
        session: &store::Session,
        user_id: &str,
        libraries: &[abs::Library],
    ) -> Result<Vec<abs::RecentEpisode>, ApiError> {
        let api_key = &session.api_key;
        let newest = futures::future::join_all(
            libraries
                .iter()
                .filter(|library| library.media_type == abs::MediaType::Podcast)
                .map(|library| async move {
                    let newest = self
                        .abs
                        .recent_episodes(library_server, api_key, &library.id, HOME_ENTRIES)
                        .await;
                    (library, newest)
                }),
        )
        .await;
        let mut episodes = Vec::new();
        for (library, newest) in newest {
            match newest {
                Ok(newest) => {
                    warn_dropped(
                        library_server,
                        Some(user_id),
                        "newest episodes",
                        newest.dropped,
                    );
                    episodes.extend(newest.entries);
                }
                Err(abs::Error::NotFound(cause)) => tracing::info!(
                    library_server,
                    library = library.id,
                    cause = ?cause,
                    "a podcast Library went out of sight while listing Home"
                ),
                Err(error) => {
                    let request = Request {
                        operation: "list a podcast Library's newest episodes",
                        id: Some(&library.id),
                    };
                    return Err(self.failure(library_server, session, request, error).await);
                }
            }
        }
        Ok(episodes)
    }

    async fn item(&self, token: SessionToken, id: ItemId) -> Result<Item, ApiError> {
        let (_, found) = self.found_item(token, &id, "fetch an Item").await?;
        Ok(found.fetched)
    }

    /// The Item `id` on the session's Library Server, with the session's user and their
    /// Progress in it; not found unless a library item the user can see holds it. Also the
    /// `operation`'s [`Request`] (for the library item) for further log events.
    async fn found_item<'r>(
        &self,
        token: SessionToken,
        id: &'r ItemId,
        operation: &'static str,
    ) -> Result<(Request<'r>, Browsed<'_, Item>), ApiError> {
        let request = Request {
            operation,
            id: Some(&id.library_item),
        };
        let Browsed {
            library_server,
            session,
            user,
            fetched: library_item,
        } = self
            .browse(token, request, async |server, key| {
                self.abs.library_item(server, key, &id.library_item).await
            })
            .await?;
        let item = browse::item(&user, library_item, id.clone())
            .map_err(|reason| missing(library_server, request, id.episode.as_deref(), reason))?;
        let found = Browsed {
            library_server,
            session,
            user,
            fetched: item,
        };
        Ok((request, found))
    }

    /// Marks Item `id` Finished or unfinished for the session's user with their key, once the
    /// library item is found to hold it: Audiobookshelf itself checks neither that the user
    /// may see the library item nor that the episode belongs to it. Answers with the Item and
    /// the user's Progress read back from the Library Server.
    async fn mark_finished(
        &self,
        token: SessionToken,
        id: ItemId,
        finished: bool,
    ) -> Result<Item, ApiError> {
        let (library_item_id, episode) = (&id.library_item, id.episode.as_deref());
        let (request, found) = self
            .found_item(token, &id, "mark an Item Finished or unfinished")
            .await?;
        let Browsed {
            library_server,
            session,
            user,
            fetched: mut item,
        } = found;
        if let Err(error) = self
            .abs
            .set_finished(
                library_server,
                &session.api_key,
                library_item_id,
                episode,
                finished,
            )
            .await
        {
            tracing::info!(
                library_server,
                library_item = library_item_id,
                episode,
                finished,
                "an Item was not marked"
            );
            return Err(self.failure(library_server, &session, request, error).await);
        }
        tracing::info!(
            library_server,
            user_id = user.id,
            library_item = library_item_id,
            episode,
            finished,
            "Item marked"
        );
        let user = self.abs.me(library_server, &session.api_key).await;
        let user = self.verified(library_server, &session, user).await?;
        item.progress = browse::item_progress(&user, &item.id);
        Ok(item)
    }

    /// Every Sonos Group, for any valid session.
    async fn groups(&self, token: SessionToken) -> Result<Vec<Group>, ApiError> {
        self.session(&token).await?;
        match self.sonos.groups().await {
            Ok(groups) => Ok(groups.into_iter().map(group).collect()),
            Err(error) => {
                tracing::warn!(%error, "cannot list the Sonos Groups");
                Err(sonos_error(error))
            }
        }
    }

    /// Starts Item `id` on Group `group` for the session's user: resolves the Item's audio
    /// file with the session's key, stores the Handoff (replacing the Group's), then has the
    /// Group play its stream. If the Group doesn't, the new Handoff ends again, and the one it
    /// replaced stays ended: the Group is left with none. Starts on one Group take turns.
    async fn start_handoff(
        &self,
        token: SessionToken,
        id: ItemId,
        group: String,
    ) -> Result<Handoff, ApiError> {
        let library_item_id = id.library_item.clone();
        let request = Request {
            operation: "start a Handoff",
            id: Some(&library_item_id),
        };
        let Browsed {
            library_server,
            session,
            user,
            fetched: library_item,
        } = self
            .browse(token, request, async |server, key| {
                self.abs.library_item(server, key, &library_item_id).await
            })
            .await?;
        let file = match browse::single_audio_file(library_item, &id) {
            Ok(file) => file,
            Err(NoSingleFile::Missing(reason)) => {
                return Err(missing(
                    library_server,
                    request,
                    id.episode.as_deref(),
                    reason,
                ));
            }
            Err(NoSingleFile::Files(files)) => {
                tracing::info!(
                    library_server,
                    library_item = id.library_item,
                    episode = id.episode,
                    files,
                    "an Item without exactly one audio file can't be played on a Group yet"
                );
                return Err(ApiError::ItemNotPlayable);
            }
        };
        let name = file
            .metadata
            .map(|metadata| metadata.filename)
            .filter(|name| sonos::media_type(name).is_some())
            .ok_or_else(|| {
                tracing::info!(
                    library_server,
                    library_item = id.library_item,
                    episode = id.episode,
                    "an Item's audio file is in a format Sonos doesn't play, or unnamed"
                );
                ApiError::ItemNotPlayable
            })?;
        if !is_group_id(&group) {
            return Err(ApiError::GroupNotFound);
        }

        let stream_token = StreamToken::new(random_hex("a stream id")?);
        let stream = stream_token.hash();
        let target = sonos::Stream {
            listen: self.listen,
            path: stream_token.path(&name),
        };
        let (turn, store, sonos) = (
            self.group_lock(&group),
            Arc::clone(&self.store),
            Arc::clone(&self.sonos),
        );
        let library_server = library_server.to_owned();
        // On its own task, so no client hanging up can cut it off between storing the Handoff
        // and the Group playing it (or it ending again); a panic in it is handled below.
        // Dioxus runs server functions detached today as well, so a client hanging up doesn't
        // cancel them; this keeps the start whole should that change.
        let task_group = group.clone();
        let (logged_item, logged_episode) = (library_item_id.clone(), id.episode.clone());
        let started = tokio::spawn(async move {
            let _turn = turn.lock().await;
            let handoff = store::Handoff {
                group: task_group,
                library_item: library_item_id,
                episode: id.episode,
                file: HandoffFile {
                    ino: file.ino,
                    name,
                },
                started_by: session.user,
                username: user.username,
                started_at: now_millis(),
            };
            store
                .start_handoff(&stream, &handoff, &session.api_key)
                .await
                .map_err(storage_error("start a Handoff"))?;
            // The stream answers from here on: the Group may fetch it as soon as it is told.
            if let Err(error) = sonos.play(&handoff.group, &target).await {
                tracing::warn!(group = handoff.group, %error, "a Group did not start a Handoff");
                end_unstarted(store.as_ref(), &stream, &handoff.group).await;
                return Err(sonos_error(error));
            }
            tracing::info!(
                group = handoff.group,
                library_server,
                user_id = handoff.started_by.user_id,
                library_item = handoff.library_item,
                episode = handoff.episode,
                "Handoff started"
            );
            Ok(handoff)
        })
        .await;
        let handoff = match started {
            Ok(started) => started?,
            Err(error) => {
                // It may have stored the Handoff before it failed.
                let ended = end_unstarted(self.store.as_ref(), &stream, &group).await;
                tracing::error!(
                    group,
                    library_item = logged_item,
                    episode = logged_episode,
                    ended,
                    %error,
                    "starting a Handoff failed"
                );
                return Err(ApiError::Internal);
            }
        };
        Ok(api_handoff(handoff))
    }

    /// The lock starts on Group `group` take turns with; forgets the locks no start holds.
    fn group_lock(&self, group: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut starting = self.starting.lock().unwrap_or_else(PoisonError::into_inner);
        starting.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = starting.get(group).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        starting.insert(group.to_owned(), Arc::downgrade(&lock));
        lock
    }

    /// Every active Handoff, for any valid session.
    async fn handoffs(&self, token: SessionToken) -> Result<Vec<Handoff>, ApiError> {
        self.session(&token).await?;
        let handoffs = self
            .store
            .handoffs()
            .await
            .map_err(storage_error("list the Handoffs"))?;
        Ok(handoffs.into_iter().map(api_handoff).collect())
    }

    /// The session `token` names, and the configured Library Server it belongs to.
    async fn session(&self, token: &SessionToken) -> Result<(&str, store::Session), ApiError> {
        let session = self
            .store
            .session(&SessionId::of_token(token.expose()))
            .await
            .map_err(storage_error("look up a session"))?
            .ok_or(ApiError::SessionInvalid)?;
        // The operator may have removed the Library Server since the login.
        let library_server = self
            .configured_base_url(&session.user.library_server)
            .ok_or_else(|| {
                tracing::info!(
                    library_server = session.user.library_server,
                    "session call for a Library Server that is not configured"
                );
                ApiError::SessionInvalid
            })?;
        Ok((library_server, session))
    }

    /// Fetches something with `fetch` (the `request`) from the session's Library Server as its
    /// user, and checks at the same time that the session's key still acts for that user.
    async fn browse<T>(
        &self,
        token: SessionToken,
        request: Request<'_>,
        fetch: impl AsyncFnOnce(&str, &abs::ApiKey) -> Result<T, abs::Error>,
    ) -> Result<Browsed<'_, T>, ApiError> {
        let (library_server, session) = self.session(&token).await?;
        let (user, fetched) = tokio::join!(
            self.abs.me(library_server, &session.api_key),
            fetch(library_server, &session.api_key),
        );
        let user = self.verified(library_server, &session, user).await?;
        match fetched {
            Ok(fetched) => Ok(Browsed {
                library_server,
                session,
                user,
                fetched,
            }),
            Err(error) => Err(self.failure(library_server, &session, request, error).await),
        }
    }

    /// The session's user as the Library Server described them (`me`), if its key still acts
    /// for them. A key the Library Server rejects, or that now belongs to another user, is
    /// revoked, ending this user's sessions opened with it.
    async fn verified(
        &self,
        library_server: &str,
        session: &store::Session,
        me: Result<abs::User, abs::Error>,
    ) -> Result<abs::User, ApiError> {
        match me {
            Ok(user) if user.id == session.user.user_id => {
                warn_dropped(
                    library_server,
                    Some(&user.id),
                    "Progress entries",
                    user.media_progress.dropped,
                );
                Ok(user)
            }
            // An admin can move a key to another user; the session must not follow it.
            Ok(_) => Err(self
                .revoke(session, "the API key now acts for another user")
                .await),
            Err(error) => Err(self
                .failure(library_server, session, Request::ME, error)
                .await),
        }
    }

    /// The error a session call answers with when its Library Server failed `request` with
    /// `error`. A rejected key is revoked.
    async fn failure(
        &self,
        library_server: &str,
        session: &store::Session,
        request: Request<'_>,
        error: abs::Error,
    ) -> ApiError {
        match error {
            abs::Error::Rejected => {
                self.revoke(session, "the Library Server rejected the API key")
                    .await
            }
            error => abs_error(library_server, request, error),
        }
    }

    /// Revokes the session's key for its user because of `reason`, ending this user's sessions
    /// opened with it. Returns the error the call answers with.
    async fn revoke(&self, session: &store::Session, reason: &str) -> ApiError {
        let (library_server, user_id) = (&session.user.library_server, &session.user.user_id);
        match self
            .store
            .revoke_api_key(&session.user, &session.api_key)
            .await
        {
            Ok(revoked) => {
                // A concurrent call may have revoked it first; that one logged it.
                if revoked {
                    tracing::info!(
                        library_server,
                        user_id,
                        reason,
                        "API key revoked, this user's sessions opened with it ended"
                    );
                }
                ApiError::ApiKeyRejected
            }
            Err(error) => {
                tracing::error!(
                    operation = "revoke an API key",
                    library_server,
                    user_id,
                    reason,
                    error = crate::error_chain(&error),
                    "storage failed"
                );
                ApiError::Internal
            }
        }
    }
}

impl<S: Store> Bridge for AppApi<S> {
    fn library_servers(&self) -> Vec<LibraryServer> {
        self.library_servers.clone()
    }

    fn login(
        &self,
        library_server: String,
        api_key: api::ApiKey,
    ) -> BoxFuture<'_, Result<Session, ApiError>> {
        Box::pin(self.login(library_server, api_key))
    }

    fn logout(&self, token: SessionToken) -> BoxFuture<'_, Result<(), ApiError>> {
        Box::pin(self.logout(token))
    }

    fn current_user(&self, token: SessionToken) -> BoxFuture<'_, Result<CurrentUser, ApiError>> {
        Box::pin(self.current_user(token))
    }

    fn libraries(&self, token: SessionToken) -> BoxFuture<'_, Result<Vec<Library>, ApiError>> {
        Box::pin(self.libraries(token))
    }

    fn library_entries(
        &self,
        token: SessionToken,
        library: String,
    ) -> BoxFuture<'_, Result<Vec<LibraryEntry>, ApiError>> {
        Box::pin(self.library_entries(token, library))
    }

    fn item(&self, token: SessionToken, id: ItemId) -> BoxFuture<'_, Result<Item, ApiError>> {
        Box::pin(self.item(token, id))
    }

    fn mark_finished(
        &self,
        token: SessionToken,
        item: ItemId,
        finished: bool,
    ) -> BoxFuture<'_, Result<Item, ApiError>> {
        Box::pin(self.mark_finished(token, item, finished))
    }

    fn podcast(
        &self,
        token: SessionToken,
        podcast: String,
        filter: EpisodeFilter,
    ) -> BoxFuture<'_, Result<Podcast, ApiError>> {
        Box::pin(self.podcast(token, podcast, filter))
    }

    fn home(&self, token: SessionToken) -> BoxFuture<'_, Result<Home, ApiError>> {
        Box::pin(self.home(token))
    }

    fn groups(&self, token: SessionToken) -> BoxFuture<'_, Result<Vec<Group>, ApiError>> {
        Box::pin(self.groups(token))
    }

    fn start_handoff(
        &self,
        token: SessionToken,
        item: ItemId,
        group: String,
    ) -> BoxFuture<'_, Result<Handoff, ApiError>> {
        Box::pin(self.start_handoff(token, item, group))
    }

    fn handoffs(&self, token: SessionToken) -> BoxFuture<'_, Result<Vec<Handoff>, ApiError>> {
        Box::pin(self.handoffs(token))
    }
}

/// The App API's answer to Sonos failing.
fn sonos_error(error: sonos::Error) -> ApiError {
    match error {
        sonos::Error::Unreachable => ApiError::SonosUnreachable,
        sonos::Error::SearchFailed => ApiError::SonosSearchFailed,
        sonos::Error::UnknownGroup => ApiError::GroupNotFound,
        sonos::Error::GroupFailed => ApiError::GroupFailed,
    }
}

/// A Handoff as the App API describes it.
fn api_handoff(handoff: store::Handoff) -> Handoff {
    Handoff {
        group: handoff.group,
        item: ItemId {
            library_item: handoff.library_item,
            episode: handoff.episode,
        },
        started_by: CurrentUser {
            library_server: handoff.started_by.library_server,
            user_id: handoff.started_by.user_id,
            username: handoff.username,
        },
        started_at: handoff.started_at,
    }
}

/// Whether `group` can be a Group's id: the characters of a Sonos player UUID (ASCII letters,
/// digits and `_`), at most 64. Anything else names no Group and is never stored.
fn is_group_id(group: &str) -> bool {
    !group.is_empty()
        && group.len() <= 64
        && group
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Now, in milliseconds since the Unix epoch.
fn now_millis() -> i64 {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX)
}

/// A Group as the App API describes it.
fn group(group: sonos::Group) -> Group {
    Group {
        id: group.id,
        name: group.name,
        members: group
            .members
            .into_iter()
            .map(|member| GroupMember {
                id: member.id,
                name: member.name,
            })
            .collect(),
        state: group.state.map(|state| match state {
            sonos::TransportState::Playing => TransportState::Playing,
            sonos::TransportState::Paused => TransportState::Paused,
            sonos::TransportState::Stopped => TransportState::Stopped,
            sonos::TransportState::Transitioning => TransportState::Transitioning,
            sonos::TransportState::Unknown => TransportState::Unknown,
        }),
        now_playing: group.track.map(|track| NowPlaying {
            title: track.title,
            creator: track.creator,
            position: track.position.map(|position| position.as_secs_f64()),
            duration: track.duration.map(|duration| duration.as_secs_f64()),
        }),
    }
}

/// 256 random bits from the operating system, hex-encoded, for `what` (a session token, a
/// stream id).
fn random_hex(what: &str) -> Result<String, ApiError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| {
        tracing::error!(%error, what, "no randomness");
        ApiError::Internal
    })?;
    let mut hex = String::with_capacity(2 * bytes.len());
    for byte in bytes {
        write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(hex)
}

/// Something fetched from the session's Library Server as its user ([`AppApi::browse`]), or
/// shaped from that ([`AppApi::found_item`]), and for whom.
struct Browsed<'a, T> {
    /// The session's Library Server.
    library_server: &'a str,
    session: store::Session,
    /// The session's user, with their Progress.
    user: abs::User,
    fetched: T,
}

/// What the Bridge asked a Library Server for, for its log events.
#[derive(Debug, Clone, Copy)]
struct Request<'a> {
    operation: &'static str,
    /// The Library or library item asked for, if any.
    id: Option<&'a str>,
}

impl Request<'_> {
    /// The "who am I" check that proves an API key.
    const ME: Request<'static> = Request {
        operation: "check who the API key acts for",
        id: None,
    };
}

/// Logs that `dropped` of the `what` a Library Server sent (for user `user_id`, if any) were
/// left out because the Bridge doesn't know their shape.
fn warn_dropped(library_server: &str, user_id: Option<&str>, what: &'static str, dropped: usize) {
    if dropped > 0 {
        tracing::warn!(
            library_server,
            user_id,
            what,
            dropped,
            "entries in a shape the Bridge doesn't know were left out"
        );
    }
}

/// The `not_found` a call answers with when the library item it fetched (`request`) holds
/// nothing it asked for, logged with the `reason` (and the `episode` asked for, if any).
fn missing(
    library_server: &str,
    request: Request,
    episode: Option<&str>,
    reason: browse::Missing,
) -> ApiError {
    tracing::info!(
        library_server,
        operation = request.operation,
        library_item = request.id,
        episode,
        reason = ?reason,
        "the library item holds nothing asked for"
    );
    ApiError::NotFound
}

fn abs_error(library_server: &str, request: Request, error: abs::Error) -> ApiError {
    match error {
        abs::Error::Rejected => ApiError::ApiKeyRejected,
        abs::Error::NotFound(cause) => {
            tracing::info!(
                library_server,
                operation = request.operation,
                id = request.id,
                cause = ?cause,
                "not found on the Library Server"
            );
            ApiError::NotFound
        }
        abs::Error::Unavailable(_) => {
            tracing::warn!(
                library_server,
                operation = request.operation,
                id = request.id,
                %error,
                "Library Server unavailable"
            );
            ApiError::LibraryServerUnreachable
        }
    }
}

/// Logs a storage failure during `operation` and hides it from the App.
fn storage_error(operation: &'static str) -> impl FnOnce(store::Error) -> ApiError {
    move |error| {
        log_storage_error(operation, &error);
        ApiError::Internal
    }
}

/// Logs a storage failure during `operation`.
pub(crate) fn log_storage_error(operation: &'static str, error: &store::Error) {
    tracing::error!(
        operation,
        error = crate::error_chain(error),
        "storage failed"
    );
}

/// Ends the Handoff whose stream is `stream`, which did not start, if it was stored. Returns
/// whether it was; none if storage failed, which is logged with `group` (the Group it was
/// for, only for the log).
async fn end_unstarted<S: Store>(store: &S, stream: &store::StreamId, group: &str) -> Option<bool> {
    store
        .end_handoff(stream)
        .await
        .inspect_err(|error| {
            tracing::error!(
                operation = "end a Handoff that did not start",
                group,
                error = crate::error_chain(error),
                "storage failed"
            );
        })
        .ok()
}
