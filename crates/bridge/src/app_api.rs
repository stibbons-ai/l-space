//! The App API's view of this Bridge: configured Library Servers, logins and sessions,
//! browsing a session's Library Server as its user, the household's Sonos Groups, Handoffs
//! (supervised while active: `supervise`), and controlling a Group (its Handoff and its
//! volume).

mod supervise;

use std::collections::HashMap;
use std::fmt::Write;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use api::server::{BoxFuture, Bridge};
use api::{
    ApiError, CurrentUser, EpisodeFilter, Group, GroupMember, HOME_ENTRIES, Handoff, Home,
    HomeEntry, Item, ItemId, Library, LibraryEntry, LibraryServer, NowPlaying, Playback, Podcast,
    Progress, Session, SessionToken, Skip, TransportState,
};
use clock::Clock;
use sonos::Sonos;
use store::{ActiveHandoff, GroupHandoff, HandoffFile, LibraryUser, SessionId, Store};

use crate::browse::{self, NoSingleFile};
use crate::config::{LibraryServerConfig, LibraryServerUrl};
use crate::stream::{self, StreamToken};
use supervise::Supervisors;

/// How many more Items in progress than the user has Progress records the Bridge asks the
/// Library Server for, for Home. Audiobookshelf 2.37.2 lists one entry per Progress record
/// at most, so the records `/api/me` showed bound the list; the margin covers Progress first
/// recorded (by another of the user's players) between that read and the list's, which
/// would otherwise push the user's oldest entries out of it.
const IN_PROGRESS_MARGIN: usize = 10;

/// Serves the App API from the configured Library Servers, the [`Store`] and [`Sonos`]; and,
/// for the same Handoffs, the media streams (`crate::stream`).
#[derive(Debug)]
pub struct AppApi<S> {
    library_servers: Arc<[LibraryServer]>,
    pub(crate) abs: abs::Client,
    store: Arc<S>,
    sonos: Arc<dyn Sonos>,
    /// Where the Bridge listens, port 0 resolved: where Groups fetch streams.
    listen: SocketAddr,
    /// One turn per Group a start, a control or a supervisor is under way on, so they take
    /// turns on one Group: a start from after resolving its Item to the Group playing it (or
    /// it ending again), a control from reading what the Group plays to writing Progress, a
    /// supervisor from reading what the Group plays to writing Progress.
    turns: Arc<Turns>,
    /// Each active Handoff's supervisor, timed on the system clock or a test's
    /// (`crate::test_clock`).
    supervisors: Arc<Supervisors<S>>,
}

impl<S: Store> AppApi<S> {
    pub fn new(
        library_servers: Vec<LibraryServerConfig>,
        store: S,
        sonos: Arc<dyn Sonos>,
        clock: Arc<dyn Clock>,
        listen: SocketAddr,
    ) -> Self {
        let library_servers = library_servers
            .into_iter()
            .map(|server| LibraryServer {
                base_url: server.url.into_string(),
                name: server.name,
            })
            .collect();
        let (store, turns) = (Arc::new(store), Arc::default());
        let controls = Controls {
            abs: abs::Client::new(),
            store: Arc::clone(&store),
            sonos: Arc::clone(&sonos),
            library_servers: Arc::clone(&library_servers),
        };
        Self {
            library_servers,
            abs: controls.abs.clone(),
            store,
            sonos,
            listen,
            turns: Arc::clone(&turns),
            supervisors: Arc::new(Supervisors::new(controls, turns, clock)),
        }
    }

    /// What a control, a start's replacement write or a supervisor acts with.
    fn controls(&self) -> Controls<S> {
        self.supervisors.controls().clone()
    }

    /// The base URL of the configured Library Server that `url` (in any spelling) normalizes
    /// to.
    fn configured(&self, url: &str) -> Option<&str> {
        self.configured_base_url(&LibraryServerUrl::parse(url).ok()?.into_string())
    }

    /// `base_url` (already normalized), if it is a configured Library Server's.
    fn configured_base_url(&self, base_url: &str) -> Option<&str> {
        configured_in(&self.library_servers, base_url)
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
    /// open. Only the Library Server knows that (Libraries, explicit content, tags), so each
    /// candidate's library item is read with the user's key, in order, up to [`HOME_ENTRIES`]
    /// reads at a time, until enough are found or none are left. `may_open` remembers each
    /// answer, so a library item is read at most once per Home. A refusal (not found or
    /// forbidden) leaves the entry out; any other failure fails Home.
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

    /// Every Sonos Group, for any valid session, each with its active Handoff, if any. If the
    /// Handoffs can't be read, the Groups are listed without them (the failure is logged).
    async fn groups(&self, token: SessionToken) -> Result<Vec<Group>, ApiError> {
        self.session(&token).await?;
        let (groups, handoffs) = tokio::join!(self.sonos.groups(), self.store.handoffs());
        // Logged first, so a store failure is logged when Sonos failed too.
        let handoffs = handoffs.unwrap_or_else(|error| {
            log_storage_error("list the Handoffs", &error);
            Vec::new()
        });
        let groups = groups.map_err(|error| {
            tracing::warn!(%error, "cannot list the Sonos Groups");
            sonos_error(error)
        })?;
        let mut handoffs: HashMap<String, store::Handoff> = handoffs
            .into_iter()
            .map(|handoff| (handoff.group.clone(), handoff))
            .collect();
        Ok(groups
            .into_iter()
            .map(|listed| {
                let handoff = handoffs.remove(&listed.id).map(api_handoff);
                group(listed, handoff)
            })
            .collect())
    }

    /// Starts Item `id` on Group `group` for the session's user: resolves the Item's audio
    /// file with the session's key and the start position ([`start_position`]: the user's
    /// Progress, or 0 when `from_start`, Finished, without Progress, or when its whole seconds
    /// lie at or past the Item's end), then writes the Progress of the Handoff it replaces, if
    /// any ([`Controls::write_replaced`]), stores the Handoff (replacing the Group's), has the
    /// Group play its stream and go to the start position, and supervises it (`supervise`)
    /// until it ends. The start position is read before the replaced Handoff's write: a user
    /// who starts the Item their own Handoff on the Group plays starts it from their Progress
    /// as it was before that write. If the Group doesn't play or seek, the new Handoff ends
    /// again, and the one it replaced stays ended: the Group is left with none. After a failed
    /// seek the Group, already playing, is also paused, on a task of its own that keeps the
    /// Group's turn until the pause is done (at most [`sonos::CONTROL_TIMEOUT`]) without
    /// holding up the answer.
    ///
    /// Starts take turns with starts, controls and supervisors on the Group. A start that had
    /// to wait for its turn, or whose Group had a turn end while it resolved (a start, control
    /// or supervisor visit that was under way, or came and went), reads the user's Progress
    /// again once it has its turn (unless `from_start`), as those may have changed it (a pause
    /// or the cadence writes it), within [`START_PROGRESS_TIME`], checking again that the
    /// session's key acts for its user. It waits for its turn only as long as [`START_TIME`]
    /// leaves after resolving, less [`START_PROGRESS_TIME`] for that read,
    /// [`sonos::PLAY_TIMEOUT`] and [`sonos::CONTROL_TIMEOUT`] for a seek (just
    /// [`sonos::PLAY_TIMEOUT`] when `from_start`); past that, or when resolving left less than
    /// those, or the read fails, nothing is stored or played ([`ApiError::GroupFailed`], or the
    /// read's error).
    ///
    /// A start that found its Group's turn free, with none ended since before resolving, uses
    /// the Progress read while resolving, and needs only [`sonos::PLAY_TIMEOUT`] (and
    /// [`sonos::CONTROL_TIMEOUT`] when it seeks) left after it. The replaced Handoff's
    /// Progress write gets what [`START_TIME`] leaves beyond those, at most
    /// [`REPLACED_PROGRESS_TIME`]; one that fails or runs out of time is logged, and the start
    /// goes on.
    async fn start_handoff(
        &self,
        token: SessionToken,
        id: ItemId,
        group: String,
        from_start: bool,
    ) -> Result<Handoff, ApiError> {
        let called = tokio::time::Instant::now();
        // Before resolving, so that a turn on the Group ending from here on, whose start or
        // control may change Progress, is seen.
        let group_turn = self.turns.of(&group);
        let ended = group_turn.ended();
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
        let progress = browse::item_progress(&user, &id);
        let length = browse::item_duration(&library_item, &id);
        let position = start_position(progress.as_ref(), from_start, length);
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
        let after_turn = if from_start {
            sonos::PLAY_TIMEOUT
        } else {
            START_PROGRESS_TIME + sonos::PLAY_TIMEOUT + sonos::CONTROL_TIMEOUT
        };
        let (turn, position) = match group_turn.try_take() {
            // Free, and no turn on the Group ended since before resolving, so the Progress read
            // then is current (a start `from_start` uses none).
            Some(turn) if from_start || group_turn.ended() == ended => (turn, position),
            free => {
                // Whatever held the turn may have changed Progress (a pause writes it), so a
                // start that may seek reads it again once it has its turn, and keeps time for
                // that read and a seek even when Progress was none.
                let turn = match free {
                    Some(turn) => turn,
                    None => {
                        let turn_time = START_TIME
                            .saturating_sub(called.elapsed())
                            .saturating_sub(after_turn);
                        group_turn.take(&group, turn_time, START_NOT_DONE).await?
                    }
                };
                if from_start {
                    (turn, position)
                } else {
                    // A turn taken with (next to) no wait left, after a slow resolve, leaves
                    // too little for the read and the Group's calls within the App's limit.
                    if START_TIME.saturating_sub(called.elapsed()) < after_turn {
                        return Err(too_little_time(
                            called,
                            &group,
                            library_server,
                            &id,
                            position,
                        ));
                    }
                    let reread = Request {
                        operation: "read Progress again for a start",
                        id: Some(&library_item_id),
                    };
                    let user = self
                        .user_now(library_server, &session, reread, START_PROGRESS_TIME)
                        .await?;
                    let progress = browse::item_progress(&user, &id);
                    (turn, start_position(progress.as_ref(), false, length))
                }
            }
        };
        let seek_time = if position.is_zero() {
            Duration::ZERO
        } else {
            sonos::CONTROL_TIMEOUT
        };
        // A turn free at once after a slow resolve may leave too little for the Group's calls.
        if START_TIME.saturating_sub(called.elapsed()) < sonos::PLAY_TIMEOUT + seek_time {
            return Err(too_little_time(
                called,
                &group,
                library_server,
                &id,
                position,
            ));
        }
        let (store, sonos) = (Arc::clone(&self.store), Arc::clone(&self.sonos));
        let library_server = library_server.to_owned();
        let (controls, supervisors) = (self.controls(), Arc::clone(&self.supervisors));
        // On its own task, so no client hanging up can cut it off between storing the Handoff
        // and the Group playing it (or it ending again); a panic in it is handled below.
        // Dioxus runs server functions detached today as well, so a client hanging up doesn't
        // cancel them; this keeps the start whole should that change. Up to here nothing was
        // stored or sent, so a cancelled call left nothing behind.
        let task_group = group.clone();
        let (logged_item, logged_episode) = (library_item_id.clone(), id.episode.clone());
        let started = tokio::spawn(async move {
            // The Handoff this start replaces gets its Progress first, with what is left after
            // keeping the time to play and seek.
            match store.group_handoff(&task_group).await {
                Ok(Some(replaced)) => {
                    let time = START_TIME
                        .saturating_sub(called.elapsed())
                        .saturating_sub(sonos::PLAY_TIMEOUT + seek_time)
                        .min(REPLACED_PROGRESS_TIME);
                    let known = supervisors.replaced(&task_group, &replaced.stream);
                    controls
                        .write_replaced(&task_group, &replaced, known, time)
                        .await;
                }
                Ok(None) => {}
                Err(error) => log_storage_error("look up the Handoff a start replaces", &error),
            }
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
            // The replaced Handoff, if any, is gone: so is its supervision.
            supervisors.stop(&handoff.group);
            // The stream answers from here on: the Group may fetch it as soon as it is told.
            if let Err(error) = sonos.play(&handoff.group, &target).await {
                tracing::warn!(group = handoff.group, %error, "a Group did not start a Handoff");
                end_unstarted(store.as_ref(), &stream, &handoff.group).await;
                return Err(sonos_error(error));
            }
            // Seek after Play: the smoke test (docs/research/sonos-smoke-2026-11-04.md) sought
            // Groups already playing; a Seek right after Play is unverified on hardware.
            if !position.is_zero()
                && let Err(error) = sonos.seek(&handoff.group, position).await
            {
                tracing::warn!(
                    group = handoff.group,
                    position = position.as_secs(),
                    library_item = handoff.library_item,
                    episode = handoff.episode,
                    %error,
                    "a Group did not go to a Handoff's start position"
                );
                if end_unstarted(store.as_ref(), &stream, &handoff.group)
                    .await
                    .is_none()
                    && end_unstarted(store.as_ref(), &stream, &handoff.group)
                        .await
                        .is_none()
                {
                    tracing::error!(
                        group = handoff.group,
                        library_item = handoff.library_item,
                        episode = handoff.episode,
                        "a Handoff left active at about 0 after a failed seek; a pause or skip \
                         would overwrite Progress"
                    );
                }
                // The Group already plays, from 0: paused, so the listener doesn't hear the
                // Item from its beginning without a Handoff. Its turn is kept until then, but
                // the answer doesn't wait for it.
                let group = handoff.group;
                tokio::spawn(async move {
                    if let Err(error) = sonos.pause(&group).await {
                        tracing::warn!(
                            group,
                            %error,
                            "a Group that did not go to a Handoff's start position was not paused"
                        );
                    }
                    drop(turn);
                });
                return Err(sonos_error(error));
            }
            supervisors.start(&handoff, stream, known_length(length), position);
            tracing::info!(
                group = handoff.group,
                library_server,
                user_id = handoff.started_by.user_id,
                library_item = handoff.library_item,
                episode = handoff.episode,
                position = position.as_secs(),
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

    /// Has Group `group` do `control` on its Handoff, for any valid session, whoever started
    /// the Handoff ([`Controls::control`]), and tells the Handoff's supervisor where the Group
    /// now is. Takes turns with starts, other controls and supervisors on the Group, waiting
    /// at most [`CONTROL_TURN_TIME`] for its turn.
    async fn control(
        &self,
        token: SessionToken,
        group: String,
        control: Control,
    ) -> Result<Playback, ApiError> {
        self.session(&token).await?;
        if !is_group_id(&group) {
            return Err(ApiError::GroupNotFound);
        }
        let turn = self.turns.of(&group);
        let controls = self.controls();
        let supervisors = Arc::clone(&self.supervisors);
        let task_group = group.clone();
        // On its own task, as a start is, so no client hanging up can cut it off between the
        // Group's action and the Progress write; a panic in it is handled below.
        let controlled = tokio::spawn(async move {
            let not_done = "a control not sent";
            let _turn = turn.take(&task_group, CONTROL_TURN_TIME, not_done).await?;
            let controlled = controls.control(&task_group, control).await;
            // Still holding the turn, so the supervisor told is this Handoff's.
            if let Ok(playback) = &controlled {
                supervisors.seen(&task_group, playback);
            }
            controlled
        })
        .await;
        controlled.unwrap_or_else(|error| {
            tracing::error!(group, ?control, %error, "controlling a Group failed");
            Err(ApiError::Internal)
        })
    }

    /// Sets Group `group`'s volume to `volume` (above [`api::MAX_VOLUME`]: that), for any
    /// valid session, with or without a Handoff; takes turns with starts and controls on the
    /// Group, waiting at most [`CONTROL_TURN_TIME`] for its turn. Returns the volume set.
    async fn set_group_volume(
        &self,
        token: SessionToken,
        group: String,
        volume: u8,
    ) -> Result<u8, ApiError> {
        self.session(&token).await?;
        if !is_group_id(&group) {
            return Err(ApiError::GroupNotFound);
        }
        let volume = volume.min(api::MAX_VOLUME);
        let turn = self.turns.of(&group);
        let not_done = "a volume change not sent";
        let _turn = turn.take(&group, CONTROL_TURN_TIME, not_done).await?;
        self.sonos
            .set_volume(&group, volume)
            .await
            .map_err(|error| {
                tracing::warn!(group, volume, %error, "a Group's volume was not set");
                sonos_error(error)
            })?;
        tracing::info!(group, volume, "Group volume set");
        Ok(volume)
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

    /// The session's user as the Library Server describes them now, read within `within`
    /// (no answer by then is [`ApiError::LibraryServerUnreachable`]), checked as [`verified`]
    /// checks it; a failed read is logged as `request`.
    ///
    /// [`verified`]: Self::verified
    async fn user_now(
        &self,
        library_server: &str,
        session: &store::Session,
        request: Request<'_>,
        within: Duration,
    ) -> Result<abs::User, ApiError> {
        let me = tokio::time::timeout(within, self.abs.me(library_server, &session.api_key))
            .await
            .unwrap_or_else(|_| Err(abs::Error::Unavailable("no answer in time".to_owned())));
        match me {
            Err(error) => Err(self.failure(library_server, session, request, error).await),
            me => self.verified(library_server, session, me).await,
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
        let ended = "this user's sessions opened with it ended";
        let user = &session.user;
        if revoke_key(self.store.as_ref(), user, &session.api_key, reason, ended).await {
            ApiError::ApiKeyRejected
        } else {
            ApiError::Internal
        }
    }
}

/// How long a control (or a volume change) waits at most for its turn on the Group, behind a
/// start, another control or a supervisor's visit on it; past that it answers
/// [`ApiError::GroupFailed`] without acting. A supervisor waits as long for its own turn.
const CONTROL_TURN_TIME: Duration = Duration::from_secs(3);

/// How long a control's requests to the starter's Library Server may take in all: checking
/// whom the Handoff's key acts for, looking up the Item's length, writing Progress. With
/// [`CONTROL_TURN_TIME`] for its turn and the Group's [`sonos::CONTROL_TIMEOUT`] for the
/// control's calls to the Group, a control answers in about 14 seconds at most (plus local
/// storage), within the App's 15.
const CONTROL_LIBRARY_SERVER_TIME: Duration = Duration::from_secs(7);

/// How long a start may take in all, about (plus local storage): resolving the Item, waiting
/// for its turn on the Group, then, when it may seek and had to wait or saw a turn end while
/// resolving, [`START_PROGRESS_TIME`] to read the user's Progress again, then, when it
/// replaces a Handoff, that Handoff's Progress write, with only what is left beyond the
/// Group's calls (at most [`REPLACED_PROGRESS_TIME`]), then [`sonos::PLAY_TIMEOUT`] for the
/// Group to play and, starting past 0, [`sonos::CONTROL_TIMEOUT`] for it to seek. Its turn
/// waits only for what the others leave: at most 14 − 4 = 10 seconds less resolving for a
/// start `from_start`, 14 − 2 − 4 − 4 = 4 for any other (which keeps the seek's time even
/// without Progress, as the read may find some); a start left after its turn with less than
/// it needs for that read (when it reads again) and the Group's calls sends nothing. One
/// second under the App's 15.
const START_TIME: Duration = Duration::from_secs(14);

/// The most a start gives the Progress write of the Handoff it replaces: reading where the
/// Group is, checking whom the replaced Handoff's key acts for (and the Item's length, if the
/// Group doesn't tell it), writing. It gets less when [`START_TIME`] leaves less beyond the
/// Group's calls; with nothing left, the write isn't tried (logged).
const REPLACED_PROGRESS_TIME: Duration = Duration::from_secs(4);

/// How long a start may take to read the user's Progress again after its turn: one `/api/me`
/// the Library Server already answered while resolving, so 2 seconds are ample, and still
/// leave such a start at most 4 seconds of [`START_TIME`], less resolving, to wait for its
/// turn.
const START_PROGRESS_TIME: Duration = Duration::from_secs(2);

/// What a start that fails before the Group is told anything leaves undone, as logged.
const START_NOT_DONE: &str = "a Handoff not started";

/// Logs that a start `called` then, of Item `id` from `library_server` on Group `group` at
/// `position`, has too little of [`START_TIME`] left for what comes next, so sends nothing;
/// the error it answers with.
fn too_little_time(
    called: tokio::time::Instant,
    group: &str,
    library_server: &str,
    id: &ItemId,
    position: Duration,
) -> ApiError {
    tracing::warn!(
        group,
        elapsed_ms = called.elapsed().as_millis(),
        library_server,
        library_item = id.library_item,
        episode = id.episode,
        position = position.as_secs(),
        not_done = START_NOT_DONE,
        "a start has too little time left for the Group"
    );
    ApiError::GroupFailed
}

/// Where a start has the Group begin, in whole seconds: 0 when asked to play `from_start`, or
/// when `progress` (the starting user's own) is absent, Finished, less than a second in (or
/// not a time at all), or when its whole seconds lie at or past the Item's end (`duration`,
/// in seconds, when known); else its `current_time`, without the fraction.
fn start_position(
    progress: Option<&Progress>,
    from_start: bool,
    duration: Option<f64>,
) -> Duration {
    match progress {
        Some(progress)
            if !from_start && !progress.finished && progress.current_time.is_finite() =>
        {
            // Saturating: a negative time starts at 0.
            let position = Duration::from_secs(progress.current_time as u64);
            match duration {
                // A Group may refuse a seek past its track (sonos-fake does; unverified on
                // hardware).
                Some(duration) if position.as_secs_f64() >= duration => Duration::ZERO,
                _ => position,
            }
        }
        _ => Duration::ZERO,
    }
}

/// The turns on each Group a start, a control or a supervisor visit is under way on (or waits
/// for). An entry nothing holds or waits for any more is dropped at the next lookup.
#[derive(Debug, Default)]
struct Turns(Mutex<HashMap<String, Weak<GroupTurn>>>);

impl Turns {
    /// Group `group`'s turn; forgets the turns nothing holds.
    fn of(&self, group: &str) -> Arc<GroupTurn> {
        let mut turns = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        turns.retain(|_, turn| turn.strong_count() > 0);
        if let Some(turn) = turns.get(group).and_then(Weak::upgrade) {
            return turn;
        }
        let turn = Arc::new(GroupTurn {
            lock: Arc::new(tokio::sync::Mutex::new(())),
            ended: AtomicU64::new(0),
        });
        turns.insert(group.to_owned(), Arc::downgrade(&turn));
        turn
    }
}

/// A Group's turn, which starts, controls and supervisor visits on it take one at a time,
/// and how many turns on it ended so far: a start can tell whether one of them held the turn
/// while it resolved its Item.
struct GroupTurn {
    lock: Arc<tokio::sync::Mutex<()>>,
    ended: AtomicU64,
}

impl GroupTurn {
    /// How many turns on the Group ended so far.
    fn ended(&self) -> u64 {
        self.ended.load(Ordering::SeqCst)
    }

    /// The turn, if free now.
    fn try_take(self: &Arc<Self>) -> Option<Turn> {
        let held = Arc::clone(&self.lock).try_lock_owned().ok()?;
        Some(Turn {
            of: Arc::clone(self),
            _held: held,
        })
    }

    /// Waits for the turn, Group `group`'s, at most `wait`; past it, logs that `not_done`
    /// (what the caller then doesn't do) and answers [`ApiError::GroupFailed`].
    async fn take(
        self: &Arc<Self>,
        group: &str,
        wait: Duration,
        not_done: &'static str,
    ) -> Result<Turn, ApiError> {
        let held = tokio::time::timeout(wait, Arc::clone(&self.lock).lock_owned())
            .await
            .map_err(|_| {
                tracing::warn!(
                    group,
                    not_done,
                    wait_ms = wait.as_millis(),
                    "the Group is busy with another start or control"
                );
                ApiError::GroupFailed
            })?;
        Ok(Turn {
            of: Arc::clone(self),
            _held: held,
        })
    }
}

/// A turn on a Group, held until dropped, owned, so a start can hand it to the task that
/// pauses the Group after a failed seek. Keeps the Group's [`GroupTurn`], and so its count of
/// ended turns, alive while held.
struct Turn {
    of: Arc<GroupTurn>,
    _held: tokio::sync::OwnedMutexGuard<()>,
}

impl Drop for Turn {
    /// Counts the turn ended while still holding it, so whoever takes it next sees the count.
    fn drop(&mut self) {
        self.of.ended.fetch_add(1, Ordering::SeqCst);
    }
}

/// What a control on a Group's Handoff acts with, owned, so the control runs on its own task.
struct Controls<S> {
    abs: abs::Client,
    store: Arc<S>,
    sonos: Arc<dyn Sonos>,
    library_servers: Arc<[LibraryServer]>,
}

impl<S> Clone for Controls<S> {
    fn clone(&self) -> Self {
        Self {
            abs: self.abs.clone(),
            store: Arc::clone(&self.store),
            sonos: Arc::clone(&self.sonos),
            library_servers: Arc::clone(&self.library_servers),
        }
    }
}

impl<S: Store> Controls<S> {
    /// Has Group `group` do `control` on its Handoff; only while the Group plays the
    /// Handoff's stream, else no control is sent to it ([`ApiError::NoHandoff`]). A Group
    /// already paused (or playing) is sent nothing for a pause (or resume). After a pause or
    /// a skip, the Group's position is written as Progress for the starter, with the
    /// Handoff's key, once the Library Server confirmed that key still acts for the starter:
    /// for a pause, where the Group tells it paused (read right after the pause; if that read
    /// fails, is late, or tells no position of the Handoff's stream, where it was just before
    /// the pause), for a skip, the seek's target. The control's calls to the Group take [`sonos::CONTROL_TIMEOUT`] in all, its
    /// requests to the Library Server [`CONTROL_LIBRARY_SERVER_TIME`]; a write that fails or
    /// runs out of time is logged and otherwise left alone, as the Group already did what was
    /// asked, except that a key the Library Server rejects is revoked, which ends the Handoff.
    async fn control(&self, group: &str, control: Control) -> Result<Playback, ApiError> {
        let mut group_time = Budget(sonos::CONTROL_TIMEOUT);
        let mut library_time = Budget(CONTROL_LIBRARY_SERVER_TIME);
        let playback = self.playback(group, &mut group_time).await?;
        let found = self.playing_handoff(group, &playback).await?;
        let duration = told_length(&playback);
        let position = playback.position.map(|position| position.as_secs_f64());
        let asked = match control {
            Control::Pause => Some(sonos::TransportState::Paused),
            Control::Resume => Some(sonos::TransportState::Playing),
            Control::Skip(_) => None,
        };
        if asked == Some(playback.state) {
            tracing::info!(group, ?control, "a Group already as asked was sent nothing");
            return Ok(Playback {
                state: api_state(playback.state),
                position,
                duration,
            });
        }
        match control {
            Control::Resume => {
                self.act(group, control, &mut group_time, self.sonos.resume(group))
                    .await?;
                Ok(Playback {
                    state: TransportState::Playing,
                    position,
                    duration,
                })
            }
            Control::Pause => {
                self.act(group, control, &mut group_time, self.sonos.pause(group))
                    .await?;
                let position = self
                    .paused_at(group, &found.stream, &mut group_time)
                    .await
                    .or(position);
                let Some(position) = position else {
                    tracing::warn!(
                        group,
                        reason = "the Group did not tell where it paused",
                        "Progress not written"
                    );
                    return Ok(Playback {
                        state: TransportState::Paused,
                        position,
                        duration,
                    });
                };
                let duration = self
                    .check_and_write(
                        &found.active,
                        position,
                        duration,
                        "pause",
                        &mut library_time,
                    )
                    .await;
                Ok(Playback {
                    state: TransportState::Paused,
                    position: Some(position),
                    duration,
                })
            }
            Control::Skip(skip) => {
                self.skip(group, skip, &found, playback, group_time, library_time)
                    .await
            }
        }
    }

    /// Has Group `group` skip as `skip` says, kept within the Item; the Group plays `found`'s
    /// stream, and `playback` is what it told just before. See [`Controls::control`]. If the
    /// Group tells no length, a forward skip first looks the Item's length up on the Library
    /// Server. When the Library Server doesn't answer with it (unreachable, too slow, or the
    /// Item is gone there), the skip is refused with [`ApiError::LibraryServerUnreachable`].
    /// When it answers but knows no length either, the skip is refused with
    /// [`ApiError::LengthUnknown`]. Either way nothing is sent to the Group and nothing written.
    async fn skip(
        &self,
        group: &str,
        skip: Skip,
        found: &GroupHandoff,
        mut playback: sonos::Playback,
        mut group_time: Budget,
        mut library_time: Budget,
    ) -> Result<Playback, ApiError> {
        let active = &found.active;
        let no_position = || {
            tracing::warn!(group, "a Group did not tell where it is, so it can't skip");
            ApiError::GroupFailed
        };
        if playback.position.is_none() {
            return Err(no_position());
        }
        let mut duration = told_length(&playback);
        let mut acting = None;
        if duration.is_none() && skip.seconds() > 0 {
            let checked = self.check(active, Wanted::Needed, &mut library_time).await;
            match checked.acting {
                Acting::Revoked => return Err(ApiError::NoHandoff),
                Acting::RevokeFailed => return Err(ApiError::Internal),
                Acting::Starter(_) | Acting::Unknown => {}
            }
            duration = match checked.length {
                Length::Told(told) => {
                    let Some(told) = known_length(told) else {
                        tracing::info!(
                            group,
                            "a forward skip refused: neither the Group nor the Library Server \
                             knows the Item's length"
                        );
                        return Err(ApiError::LengthUnknown);
                    };
                    Some(told)
                }
                Length::Unasked | Length::Failed => {
                    tracing::info!(
                        group,
                        "a forward skip refused: the Item's length is not known to the Group, \
                         and the Library Server didn't tell it"
                    );
                    return Err(ApiError::LibraryServerUnreachable);
                }
            };
            acting = Some(checked.acting);
            // The Group moved on while the Library Server answered: skip from where it is now.
            playback = self.playback(group, &mut group_time).await?;
            ensure_plays(group, &playback, &found.stream)?;
        }
        let position = playback.position.ok_or_else(no_position)?;
        let target = skipped(position, skip, duration);
        let control = Control::Skip(skip);
        self.act(
            group,
            control,
            &mut group_time,
            self.sonos.seek(group, target),
        )
        .await?;
        let acting = match acting {
            Some(acting) => acting,
            None => {
                let checked = self
                    .check(active, Wanted::optional(duration), &mut library_time)
                    .await;
                duration = duration.or(checked.length.told());
                checked.acting
            }
        };
        let target = target.as_secs_f64();
        let update = abs::ProgressUpdate::at(target, duration);
        self.write_progress(active, acting, update, "skip", &mut library_time)
            .await;
        Ok(Playback {
            state: api_state(playback.state),
            position: Some(target),
            duration,
        })
    }

    /// What Group `group` plays, as its coordinator tells it within `time`.
    async fn playback(&self, group: &str, time: &mut Budget) -> Result<sonos::Playback, ApiError> {
        let Some(read) = time.spend(self.sonos.playback(group)).await else {
            tracing::warn!(group, "a Group did not tell its playback in time");
            return Err(ApiError::GroupFailed);
        };
        read.map_err(|error| {
            tracing::warn!(group, %error, "cannot read what a Group plays");
            sonos_error(error)
        })
    }

    /// Where Group `group` tells it is, read right after a pause within `time`, if it still
    /// plays `stream` and tells a position; none otherwise.
    async fn paused_at(
        &self,
        group: &str,
        stream: &store::StreamId,
        time: &mut Budget,
    ) -> Option<f64> {
        let read = match time.spend(self.sonos.playback(group)).await {
            Some(Ok(read)) => read,
            Some(Err(error)) => {
                tracing::info!(group, %error, "cannot read where a Group paused; using where it was just before");
                return None;
            }
            None => {
                tracing::info!(
                    group,
                    "a Group did not tell where it paused in time; using where it was just before"
                );
                return None;
            }
        };
        let plays_handoff = plays_stream(&read, stream);
        let position = read.position.filter(|_| plays_handoff);
        if position.is_none() {
            if plays_handoff {
                tracing::info!(
                    group,
                    "a Group told no position after pausing; using where it was just before"
                );
            } else {
                tracing::info!(
                    group,
                    "a Group plays something other than its Handoff after pausing; using where it was just before"
                );
            }
        }
        position.map(|position| position.as_secs_f64())
    }

    /// Has Group `group` do `control` by `sent`, within `time`.
    async fn act(
        &self,
        group: &str,
        control: Control,
        time: &mut Budget,
        sent: impl Future<Output = Result<(), sonos::Error>>,
    ) -> Result<(), ApiError> {
        let Some(sent) = time.spend(sent).await else {
            tracing::warn!(
                group,
                ?control,
                "a Group did not answer a control within the control's time; it may still have \
                 acted"
            );
            return Err(ApiError::GroupFailed);
        };
        sent.map_err(|error| {
            tracing::warn!(group, ?control, %error, "a Group did not take a control");
            sonos_error(error)
        })?;
        tracing::info!(group, ?control, "Group controlled");
        Ok(())
    }

    /// The active Handoff on Group `group`, if the Group plays its stream (`playback`, as read
    /// just before); [`ApiError::NoHandoff`] otherwise.
    async fn playing_handoff(
        &self,
        group: &str,
        playback: &sonos::Playback,
    ) -> Result<GroupHandoff, ApiError> {
        let Some(found) = self
            .store
            .group_handoff(group)
            .await
            .map_err(storage_error("look up a Group's Handoff"))?
        else {
            tracing::info!(group, "a control for a Group without a Handoff");
            return Err(ApiError::NoHandoff);
        };
        ensure_plays(group, playback, &found.stream)?;
        Ok(found)
    }

    /// Writes where Group `group` is on `replaced`'s stream as Progress for the replaced
    /// Handoff's starter, with its key ([`Controls::write_progress`]), within `time` in all:
    /// reading the Group, checking the key (and the Item's length, if neither the Group nor
    /// its supervisor (`known`) tells it), writing. Only a Group playing or paused on that
    /// stream, telling a position, gets it written; a stopped one may tell 0 there. A Group
    /// still stopped on that stream after playing to the end of the Item, whose Finished write
    /// the supervisor hadn't made yet, gets that one. With no `time` at all, nothing is tried.
    /// Anything else, a failure or running out of time is logged and otherwise left alone. A
    /// pending Finished write (`known.finished`) left unmade is logged as an error when there
    /// is no `time`, the Group doesn't tell what it plays, the write doesn't land, or the Group
    /// tells no position playing or paused on that stream; a Group playing or paused on that
    /// stream, telling a position, gets that position written instead, with no such error.
    async fn write_replaced(
        &self,
        group: &str,
        replaced: &GroupHandoff,
        known: supervise::Replaced,
        time: Duration,
    ) {
        let not_written = "the replaced Handoff's Progress not written";
        let handoff = &replaced.active.handoff;
        let lost = |reason: &str| {
            if let Some(update) = known.finished {
                tracing::error!(
                    group,
                    user_id = handoff.started_by.user_id,
                    library_item = handoff.library_item,
                    episode = handoff.episode,
                    position = update.current_time(),
                    finished = update.is_finished(),
                    reason,
                    "Finished at the end of a replaced Handoff's Item was never written: lost"
                );
            }
        };
        if time.is_zero() {
            tracing::info!(group, reason = "no time left", "{not_written}");
            lost("no time left");
            return;
        }
        let mut time = Budget(time);
        let Ok(playback) = self.playback(group, &mut time).await else {
            tracing::info!(group, "{not_written}");
            lost("the Group did not tell what it plays");
            return;
        };
        let on_stream = plays_stream(&playback, &replaced.stream);
        if let Some(update) = known.finished
            && on_stream
            && playback.state == sonos::TransportState::Stopped
        {
            let checked = self.check(&replaced.active, Wanted::No, &mut time).await;
            let cause = "replaced, end of Item";
            let wrote = self
                .write_progress(&replaced.active, checked.acting, update, cause, &mut time)
                .await;
            if !matches!(wrote, Wrote::Written) {
                lost("the write did not land");
            }
            return;
        }
        let position = playback.position.filter(|_| {
            on_stream
                && matches!(
                    playback.state,
                    sonos::TransportState::Playing | sonos::TransportState::Paused
                )
        });
        let Some(position) = position else {
            tracing::info!(
                group,
                state = ?playback.state,
                "a Group tells no position on the Handoff a start replaces; {not_written}"
            );
            lost("the Group is no longer stopped on the Handoff");
            return;
        };
        let duration = told_length(&playback).or(known.length);
        let position = position.as_secs_f64();
        self.check_and_write(&replaced.active, position, duration, "replaced", &mut time)
            .await;
    }

    /// Writes `position` (seconds) as unfinished Progress for `active`'s starter, with its key,
    /// within `time`, once the starter's Library Server confirmed the key still acts for them
    /// ([`Controls::check`], also telling the Item's length when `duration` is none), as
    /// `cause` ([`Controls::write_progress`]). Returns the length written with, if any.
    async fn check_and_write(
        &self,
        active: &ActiveHandoff,
        position: f64,
        duration: Option<f64>,
        cause: &'static str,
        time: &mut Budget,
    ) -> Option<f64> {
        let checked = self.check(active, Wanted::optional(duration), time).await;
        let duration = duration.or(checked.length.told());
        let update = abs::ProgressUpdate::at(position, duration);
        self.write_progress(active, checked.acting, update, cause, time)
            .await;
        duration
    }

    /// What the starter's Library Server tells about `active` with the Handoff's key, within
    /// `time`: whom the key acts for, and the Item's length as `wanted`. An optional length
    /// gets at most half the time left, so that a slow one neither loses a timely answer on
    /// the key nor leaves the Progress write less than half the time; a needed one gets all of
    /// it. Each answer is bounded on its own, so a timely one, a rejection above all, is kept
    /// when the other is late. A slow answer on the key itself can still leave the write too
    /// little time ([`MIN_WRITE_TIME`]). A key the Library Server rejects, or that now acts for
    /// another user, is revoked, which ends the Handoff.
    /// Failures are logged.
    async fn check(
        &self,
        active: &ActiveHandoff,
        wanted: Wanted,
        time: &mut Budget,
    ) -> Checked<'_> {
        let handoff = &active.handoff;
        let (group, starter, key) = (&handoff.group, &handoff.started_by, &active.api_key);
        let not_told = match wanted {
            Wanted::No => Length::Unasked,
            Wanted::Optional | Wanted::Needed => Length::Failed,
        };
        let unknown = Checked {
            acting: Acting::Unknown,
            length: not_told,
        };
        // The operator may have removed the Library Server since the start.
        let Some(library_server) = configured_in(&self.library_servers, &starter.library_server)
        else {
            tracing::warn!(
                group,
                library_server = starter.library_server,
                "the Library Server of a Handoff is not configured"
            );
            return unknown;
        };
        let late = |what: &'static str| {
            tracing::warn!(
                group,
                library_server,
                user_id = starter.user_id,
                what,
                "the Library Server of a Handoff did not answer a control in time"
            );
        };
        let id = ItemId {
            library_item: handoff.library_item.clone(),
            episode: handoff.episode.clone(),
        };
        let length_time = match wanted {
            Wanted::No => Duration::ZERO,
            Wanted::Optional => time.0 / 2,
            Wanted::Needed => time.0,
        };
        // None if not asked; Some(None) if not told in time.
        let length = async {
            if let Wanted::No = wanted {
                return None;
            }
            let lookup = self.abs.library_item(library_server, key, &id.library_item);
            Some(tokio::time::timeout(length_time, lookup).await.ok())
        };
        // Each bounded on its own, so a definitive answer survives its sibling being late.
        let (me, item) = time
            .spend_each(self.abs.me(library_server, key), length)
            .await;
        // A length lookup late on the budget rather than its own timeout is late all the same.
        let item = item.unwrap_or(Some(None));
        let rejected = matches!(me, Some(Err(abs::Error::Rejected)))
            || matches!(item, Some(Some(Err(abs::Error::Rejected))));
        let acting = match me {
            _ if rejected => {
                let reason = "the Library Server rejected the API key";
                self.revoke(starter, key, reason).await
            }
            None => {
                late("whom the Handoff's API key acts for");
                return unknown;
            }
            Some(Ok(user)) if user.id == starter.user_id => Acting::Starter(library_server),
            // An admin can move a key to another user; the starter's Progress must not follow.
            Some(Ok(_)) => {
                let reason = "the API key now acts for another user";
                self.revoke(starter, key, reason).await
            }
            Some(Err(error)) => {
                tracing::warn!(
                    group,
                    library_server,
                    user_id = starter.user_id,
                    %error,
                    "cannot check whom a Handoff's API key acts for"
                );
                Acting::Unknown
            }
        };
        let length = match item {
            None => Length::Unasked,
            Some(Some(Ok(item))) => Length::Told(browse::item_duration(&item, &id)),
            Some(Some(Err(error))) => {
                tracing::warn!(
                    group,
                    library_server,
                    library_item = id.library_item,
                    %error,
                    "cannot read the length of a Handoff's Item"
                );
                Length::Failed
            }
            Some(None) => {
                late("the length of the Handoff's Item");
                Length::Failed
            }
        };
        Checked { acting, length }
    }

    /// Revokes `key`, `starter`'s, because of `reason`, ending their Handoffs using it; whether
    /// storage did.
    async fn revoke(&self, starter: &LibraryUser, key: &abs::ApiKey, reason: &str) -> Acting<'_> {
        let ended = "this user's sessions and Handoffs using it ended";
        if revoke_key(self.store.as_ref(), starter, key, reason, ended).await {
            Acting::Revoked
        } else {
            Acting::RevokeFailed
        }
    }

    /// Writes `update` as Progress in `active`'s Item for the user who started it, with the
    /// Handoff's key, never with a caller's, within `time`; only if the key still acts for the
    /// starter (`acting`). Whether the starter may still see the Item is not checked again
    /// (Audiobookshelf doesn't check on a Progress write): the Handoff relies on the check made
    /// at its start. A failure is logged and otherwise left alone; a key the Library Server
    /// rejects is revoked, which ends the Handoff. `cause` (what wrote it: a pause, the
    /// cadence, ...) is logged with the write, and with a write not made. Returns whether it
    /// was written.
    async fn write_progress(
        &self,
        active: &ActiveHandoff,
        acting: Acting<'_>,
        update: abs::ProgressUpdate,
        cause: &'static str,
        time: &mut Budget,
    ) -> Wrote {
        let handoff = &active.handoff;
        let (group, starter) = (&handoff.group, &handoff.started_by);
        let (position, finished) = (update.current_time(), update.is_finished());
        let not_written = |reason: &str| {
            tracing::warn!(
                group,
                user_id = starter.user_id,
                library_item = handoff.library_item,
                episode = handoff.episode,
                position,
                finished,
                cause,
                reason,
                "Progress not written"
            );
        };
        let library_server = match acting {
            Acting::Starter(library_server) => library_server,
            Acting::Revoked => {
                not_written("the Handoff's API key was revoked");
                return Wrote::Revoked;
            }
            Acting::RevokeFailed => {
                not_written("the Handoff's API key is to be revoked, but storage failed");
                return Wrote::Not;
            }
            Acting::Unknown => {
                not_written("whom the Handoff's API key acts for is not known");
                return Wrote::Not;
            }
        };
        if time.0 < MIN_WRITE_TIME {
            not_written("no time left");
            return Wrote::Not;
        }
        let written = time
            .spend(self.abs.set_progress(
                library_server,
                &active.api_key,
                &handoff.library_item,
                handoff.episode.as_deref(),
                update,
            ))
            .await;
        match written {
            Some(Ok(())) => {
                tracing::info!(
                    group,
                    library_server,
                    user_id = starter.user_id,
                    library_item = handoff.library_item,
                    episode = handoff.episode,
                    position,
                    finished,
                    cause,
                    "Progress written"
                );
                Wrote::Written
            }
            Some(Err(abs::Error::Rejected)) => {
                not_written("the Library Server rejected the Handoff's API key");
                let reason = "the Library Server rejected the API key";
                match self.revoke(starter, &active.api_key, reason).await {
                    Acting::Revoked => Wrote::Revoked,
                    _ => Wrote::Not,
                }
            }
            Some(Err(error)) => {
                not_written(&error.to_string());
                Wrote::Not
            }
            // The request may have landed all the same.
            None => {
                tracing::warn!(
                    group,
                    library_server,
                    user_id = starter.user_id,
                    library_item = handoff.library_item,
                    episode = handoff.episode,
                    position,
                    finished,
                    cause,
                    reason = "the Library Server did not answer in time",
                    "Progress write not confirmed"
                );
                Wrote::Not
            }
        }
    }
}

/// What a control learned from the starter's Library Server ([`Controls::check`]).
struct Checked<'a> {
    acting: Acting<'a>,
    length: Length,
}

/// Whom a Handoff's key acts for, as the starter's Library Server told a control.
#[derive(Debug, Clone, Copy)]
enum Acting<'a> {
    /// The starter, on this configured Library Server: Progress may be written with it.
    Starter(&'a str),
    /// Not the starter any more, or no one: the key was revoked, ending the Handoff.
    Revoked,
    /// Not the starter any more, or no one, but revoking the key failed: the Handoff and its
    /// key are still stored.
    RevokeFailed,
    /// Not known: the Library Server failed, or didn't answer in time.
    Unknown,
}

/// Whether a Progress write ([`Controls::write_progress`]) landed.
#[derive(Debug, Clone, Copy)]
enum Wrote {
    Written,
    /// Not written, as the Handoff's key was revoked, ending the Handoff.
    Revoked,
    /// Not written, or not confirmed: failed, refused for lack of time, or the key's user not
    /// known.
    Not,
}

/// An Item's length in seconds, as the starter's Library Server told a control.
#[derive(Debug, Clone, Copy)]
enum Length {
    /// Not asked for.
    Unasked,
    /// Told: the length, none if the Item has no known length.
    Told(Option<f64>),
    /// Asked for, but not told.
    Failed,
}

impl Length {
    fn told(self) -> Option<f64> {
        match self {
            Self::Told(length) => known_length(length),
            Self::Unasked | Self::Failed => None,
        }
    }
}

/// Whether a control asks the starter's Library Server for the Item's length.
#[derive(Debug, Clone, Copy)]
enum Wanted {
    No,
    /// For the Progress write: it is written without one if not told quickly.
    Optional,
    /// To act at all.
    Needed,
}

impl Wanted {
    /// Optional if the Group told no length (`told`), else not asked.
    fn optional(told: Option<f64>) -> Self {
        if told.is_some() {
            Self::No
        } else {
            Self::Optional
        }
    }
}

/// Time that one party's answers to a call may take in all, spent only while it is waited on.
#[derive(Debug)]
struct Budget(Duration);

impl Budget {
    /// What `call` gives, if it does in the time left; none (and `call` dropped) otherwise.
    async fn spend<T>(&mut self, call: impl Future<Output = T>) -> Option<T> {
        let started = tokio::time::Instant::now();
        let done = tokio::time::timeout(self.0, call).await.ok();
        self.0 = self.0.saturating_sub(started.elapsed());
        done
    }

    /// What `a` and `b`, run together, each give in the time left; none for one that doesn't
    /// (and it dropped), keeping the other's answer.
    async fn spend_each<A, B>(
        &mut self,
        a: impl Future<Output = A>,
        b: impl Future<Output = B>,
    ) -> (Option<A>, Option<B>) {
        let (left, started) = (self.0, tokio::time::Instant::now());
        let (a, b) = tokio::join!(tokio::time::timeout(left, a), tokio::time::timeout(left, b));
        self.0 = self.0.saturating_sub(started.elapsed());
        (a.ok(), b.ok())
    }
}

/// The least time left for a Progress write to be sent at all: with less, it couldn't be
/// confirmed, so it isn't sent.
const MIN_WRITE_TIME: Duration = Duration::from_millis(250);

/// Whether `playback`, Group `group`'s, plays `stream`, a Handoff's; [`ApiError::NoHandoff`]
/// if not.
fn ensure_plays(
    group: &str,
    playback: &sonos::Playback,
    stream: &store::StreamId,
) -> Result<(), ApiError> {
    if plays_stream(playback, stream) {
        return Ok(());
    }
    tracing::info!(
        group,
        "a control for a Group that plays something other than its Handoff"
    );
    Err(ApiError::NoHandoff)
}

/// Whether `playback` plays `stream`, a Handoff's.
fn plays_stream(playback: &sonos::Playback, stream: &store::StreamId) -> bool {
    let played = playback.uri.as_deref().and_then(stream::stream_in_url);
    played.is_some_and(|played| played == *stream)
}

/// `base_url` (already normalized), if it is one of `library_servers`'.
fn configured_in<'a>(library_servers: &'a [LibraryServer], base_url: &str) -> Option<&'a str> {
    library_servers
        .iter()
        .map(|server| server.base_url.as_str())
        .find(|&configured| configured == base_url)
}

/// Revokes `api_key` for `user` because of `reason`, ending the sessions and Handoffs acting
/// with it; logs that (`"API key revoked, {ended}"`) or the storage failure. Returns whether
/// storage did it (or a concurrent revoke had, which logged it).
async fn revoke_key<S: Store>(
    store: &S,
    user: &LibraryUser,
    api_key: &abs::ApiKey,
    reason: &str,
    ended: &str,
) -> bool {
    let (library_server, user_id) = (&user.library_server, &user.user_id);
    match store.revoke_api_key(user, api_key).await {
        Ok(revoked) => {
            if revoked {
                tracing::info!(library_server, user_id, reason, "API key revoked, {ended}");
            }
            true
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
            false
        }
    }
}

impl<S: Store> Bridge for AppApi<S> {
    fn library_servers(&self) -> Vec<LibraryServer> {
        self.library_servers.to_vec()
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
        from_start: bool,
    ) -> BoxFuture<'_, Result<Handoff, ApiError>> {
        Box::pin(self.start_handoff(token, item, group, from_start))
    }

    fn handoffs(&self, token: SessionToken) -> BoxFuture<'_, Result<Vec<Handoff>, ApiError>> {
        Box::pin(self.handoffs(token))
    }

    fn pause_group(
        &self,
        token: SessionToken,
        group: String,
    ) -> BoxFuture<'_, Result<Playback, ApiError>> {
        Box::pin(self.control(token, group, Control::Pause))
    }

    fn resume_group(
        &self,
        token: SessionToken,
        group: String,
    ) -> BoxFuture<'_, Result<Playback, ApiError>> {
        Box::pin(self.control(token, group, Control::Resume))
    }

    fn skip_group(
        &self,
        token: SessionToken,
        group: String,
        skip: Skip,
    ) -> BoxFuture<'_, Result<Playback, ApiError>> {
        Box::pin(self.control(token, group, Control::Skip(skip)))
    }

    fn set_group_volume(
        &self,
        token: SessionToken,
        group: String,
        volume: u8,
    ) -> BoxFuture<'_, Result<u8, ApiError>> {
        Box::pin(self.set_group_volume(token, group, volume))
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

/// A Group as the App API describes it, with its active Handoff, if any.
fn group(group: sonos::Group, handoff: Option<Handoff>) -> Group {
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
        state: group.state.map(api_state),
        now_playing: group.track.map(|track| NowPlaying {
            title: track.title,
            creator: track.creator,
            position: track.position.map(|position| position.as_secs_f64()),
            duration: track.duration.map(|duration| duration.as_secs_f64()),
        }),
        volume: group.volume,
        handoff,
    }
}

/// A transport state as the App API names it.
fn api_state(state: sonos::TransportState) -> TransportState {
    match state {
        sonos::TransportState::Playing => TransportState::Playing,
        sonos::TransportState::Paused => TransportState::Paused,
        sonos::TransportState::Stopped => TransportState::Stopped,
        sonos::TransportState::Transitioning => TransportState::Transitioning,
        sonos::TransportState::Unknown => TransportState::Unknown,
    }
}

/// A control of a Group's Handoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Control {
    Pause,
    Resume,
    Skip(Skip),
}

/// Where `skip` from `position` lands, in whole seconds: never before the start, nor past
/// the end of an Item `duration` seconds long (if known).
fn skipped(position: Duration, skip: Skip, duration: Option<f64>) -> Duration {
    let mut target = (position.as_secs_f64() + f64::from(skip.seconds())).max(0.0);
    if let Some(duration) = duration {
        target = target.min(duration);
    }
    // `as` rounds down; the target is finite and not negative.
    Duration::from_secs(target as u64)
}

/// `length`, in seconds, if it is one an Item can have: finite and more than zero.
fn known_length(length: Option<f64>) -> Option<f64> {
    length.filter(|length| length.is_finite() && *length > 0.0)
}

/// The length `playback` tells, in seconds, if it is one an Item can have.
fn told_length(playback: &sonos::Playback) -> Option<f64> {
    known_length(playback.duration.map(|duration| duration.as_secs_f64()))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn progress(current_time: f64, finished: bool) -> Option<Progress> {
        Some(Progress {
            current_time,
            finished,
        })
    }

    #[test]
    fn a_start_begins_at_the_whole_seconds_of_unfinished_progress_unless_from_start() {
        let secs = Duration::from_secs;
        let cases = [
            (None, false, None, secs(0)),
            (progress(754.6, false), false, None, secs(754)),
            (progress(754.6, false), true, None, secs(0)),
            (progress(754.6, true), false, None, secs(0)),
            (progress(0.9, false), false, None, secs(0)),
            (progress(-3.0, false), false, None, secs(0)),
            (progress(f64::NAN, false), false, None, secs(0)),
            (progress(f64::INFINITY, false), false, None, secs(0)),
            // Within the Item's length; at or past its end.
            (progress(754.6, false), false, Some(754.3), secs(754)),
            (progress(754.6, false), false, Some(754.0), secs(0)),
            (progress(900.0, false), false, Some(754.3), secs(0)),
            (progress(754.6, false), false, Some(f64::NAN), secs(754)),
        ];
        for (progress, from_start, duration, expected) in cases {
            assert_eq!(
                start_position(progress.as_ref(), from_start, duration),
                expected,
                "{progress:?} {from_start} {duration:?}"
            );
        }
    }
}
