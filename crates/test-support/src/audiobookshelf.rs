//! A real Audiobookshelf in a container, seeded with an audiobook Library and a podcast
//! Library ([`Seeded`]), that tests share.
//!
//! [`Audiobookshelf::shared`] starts the container on first use and hands every test the same
//! one while any test holds it; the last holder's drop removes it, so no container outlives
//! the test run. Tests stay apart by each creating their own users with
//! [`Audiobookshelf::new_user`] and acting only on those, Progress included
//! ([`Audiobookshelf::set_progress`]). The seeded Libraries are shared by all tests, and users
//! are never deleted while the container runs. A test user sees only the seeded Libraries,
//! explicit content included, unless the test says otherwise; so a test may add a Library
//! ([`Audiobookshelf::crowded_podcast`]) or mark a seeded Item explicit or tag it without
//! hiding or showing anything to other tests' users, as long as it undoes the mark or tags.
//!
//! Setup goes through Audiobookshelf's REST API as its root user, the way an admin would,
//! never through its database: users are created there, API keys created, moved and deleted,
//! and metadata a scan can't find in silent audio files (descriptions, publish dates) set.
//! The audio is generated near-silence, so the repository holds no binary.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, ContainerRequest, GenericImage, ImageExt};

/// Pinned, so a new Audiobookshelf release changes nothing until we move to it.
const IMAGE: (&str, &str) = ("ghcr.io/advplyr/audiobookshelf", "2.37.2");

/// Image pull and first start can be slow on a cold machine.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(300);

/// How long the seeded audiobooks and podcast may take to appear after a scan.
const SCAN_TIMEOUT: Duration = Duration::from_secs(60);

/// The seeded audio files: path in the container, length in seconds. Silence's one file has a
/// name with spaces, non-ASCII and URL-reserved characters.
const AUDIO_FILES: [(&str, u32); 7] = [
    (
        "/audiobooks/L-Space/Silence/Folge 3 – Über & Co #1 (50%).wav",
        1,
    ),
    (
        "/audiobooks/Pratchett/The Long Book/01 - Part One.wav",
        2400,
    ),
    (
        "/audiobooks/Pratchett/The Long Book/02 - Part Two.wav",
        1800,
    ),
    (
        "/audiobooks/Pratchett/The Long Book/03 - Part Three.wav",
        120,
    ),
    ("/podcasts/The Clacks Hour/Episode 1.wav", 600),
    ("/podcasts/The Clacks Hour/Episode 2.wav", 900),
    ("/podcasts/The Clacks Hour/Episode 3.wav", 3960),
];

/// The seeded ebook without audio, in the audiobook Library.
const EBOOK_FILE: &str = "/audiobooks/Rincewind/Only Words/only.pdf";

/// Enough of a PDF for Audiobookshelf to scan it as an ebook: it reads only the extension.
const MINIMAL_PDF: &[u8] = b"%PDF-1.4\n%%EOF\n";

/// The folder of [`Audiobookshelf::crowded_podcast`]: a podcast with
/// [`CROWDED_EPISODES`] one-second episodes, in no Library until a test asks for it.
const CROWDED_FOLDER: &str = "/crowded";

/// How many episodes [`Audiobookshelf::crowded_podcast`] has.
const CROWDED_EPISODES: usize = 81;

/// The Library [`Audiobookshelf::crowded_podcast`] created, its podcast and its episodes.
#[derive(Debug, Clone)]
pub struct Crowded {
    pub library: String,
    pub podcast: String,
    pub episodes: Vec<String>,
}

/// The seeded episodes' publish dates, oldest first: 2026-01-01, 2026-02-15 and 2026-03-31,
/// midnight UTC, in milliseconds since the Unix epoch.
pub const EPISODE_PUBLISHED_AT: [i64; 3] =
    [1_767_225_600_000, 1_771_113_600_000, 1_774_915_200_000];

/// The seeded episodes' durations in seconds, oldest first.
pub const EPISODE_DURATIONS: [f64; 3] = [600.0, 900.0, 3960.0];

/// What the shared Audiobookshelf holds, by Audiobookshelf id.
#[derive(Debug, Clone, Default)]
pub struct Seeded {
    /// The audiobook Library "Audiobooks": [`Seeded::long_book`], [`Seeded::silence`] and the
    /// ebook [`Seeded::ebook_only`].
    pub audiobook_library: String,
    /// The podcast Library "Podcasts": [`Seeded::podcast`].
    pub podcast_library: String,
    /// "The Long Book" by Pratchett, described as "A book that goes on.": three audio files of
    /// 40, 30 and 2 minutes, one chapter each ("01 - Part One", "02 - Part Two",
    /// "03 - Part Three"), 72 minutes in all.
    pub long_book: String,
    /// "Silence" by L-Space: one audio file of one second, named
    /// `Folge 3 – Über & Co #1 (50%).wav`.
    pub silence: String,
    /// "Only Words" by Rincewind: an ebook (a PDF) without audio, so not an audiobook.
    pub ebook_only: String,
    /// "The Clacks Hour" by Ankh-Morpork Radio.
    pub podcast: String,
    /// Its episodes "Episode 1" (10 min), "Episode 2" (15 min) and "Episode 3" (66 min),
    /// oldest first, published at [`EPISODE_PUBLISHED_AT`]; "Episode 1" is described as
    /// "The first one.".
    pub episodes: [String; 3],
}

const ROOT_PASSWORD: &str = "root-password";

static SHARED: Mutex<Weak<Audiobookshelf>> = Mutex::new(Weak::new());

/// An Audiobookshelf user a test created, with an API key for it.
#[derive(Debug)]
pub struct AbsUser {
    pub id: String,
    pub username: String,
    pub api_key: IssuedKey,
}

/// An API key as Audiobookshelf issued it. `Debug` shows only its id, so a failing
/// assertion never prints the key.
pub struct IssuedKey {
    /// Audiobookshelf's id for the key, to delete or move it by.
    pub id: String,
    /// The key itself.
    pub key: String,
}

impl std::fmt::Debug for IssuedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedKey")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// A running Audiobookshelf, removed on drop. `Debug` leaves out its root token.
pub struct Audiobookshelf {
    container: Option<Container<GenericImage>>,
    base_url: String,
    root_token: String,
    http: reqwest::Client,
    seeded: Seeded,
}

impl std::fmt::Debug for Audiobookshelf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Audiobookshelf")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl Audiobookshelf {
    /// The running Audiobookshelf, started (and seeded) if no test holds one.
    pub fn shared() -> Arc<Self> {
        let mut shared = SHARED.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(running) = shared.upgrade() {
            return running;
        }
        // The sync runner blocks on its own runtime, which a test's async context forbids.
        let started = Arc::new(
            std::thread::spawn(Self::start)
                .join()
                .expect("start Audiobookshelf"),
        );
        *shared = Arc::downgrade(&started);
        started
    }

    /// Its base URL, as an operator would configure it on the Bridge.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// What it was seeded with.
    pub fn seeded(&self) -> &Seeded {
        &self.seeded
    }

    /// Records `user`'s Progress in audiobook `library_item`, or in its episode `episode`, as
    /// their own player would: `current_time` seconds in, Finished or not.
    pub async fn set_progress(
        &self,
        user: &AbsUser,
        library_item: &str,
        episode: Option<&str>,
        current_time: f64,
        finished: bool,
    ) {
        let path = match episode {
            Some(episode) => format!("/api/me/progress/{library_item}/{episode}"),
            None => format!("/api/me/progress/{library_item}"),
        };
        self.call(
            &user.api_key.key,
            reqwest::Method::PATCH,
            &path,
            Some(json!({"currentTime": current_time, "isFinished": finished})),
        )
        .await;
    }

    /// `user`'s Progress in audiobook `library_item`, or in its episode `episode`, read
    /// straight from Audiobookshelf with their key (`/api/me`): `(currentTime, isFinished)`,
    /// none if they have none.
    pub async fn progress(
        &self,
        user: &AbsUser,
        library_item: &str,
        episode: Option<&str>,
    ) -> Option<(f64, bool)> {
        let me = self.whose_key(&user.api_key).await;
        me["mediaProgress"]
            .as_array()
            .expect("mediaProgress")
            .iter()
            .find(|entry| match episode {
                Some(episode) => entry["episodeId"] == episode,
                None => entry["libraryItemId"] == library_item && entry["episodeId"].is_null(),
            })
            .map(|entry| {
                (
                    entry["currentTime"].as_f64().expect("currentTime"),
                    entry["isFinished"].as_bool().expect("isFinished"),
                )
            })
    }

    /// Its `host:port`, for a forwarder in front of it.
    pub fn address(&self) -> &str {
        self.base_url.trim_start_matches("http://")
    }

    /// Creates a user with an API key, named uniquely across the test run.
    pub async fn new_user(&self) -> AbsUser {
        self.new_user_with(json!({})).await
    }

    /// Creates a user with an API key who may see only the Libraries `library_ids`, as an
    /// admin restricting them would.
    pub async fn new_user_seeing(&self, library_ids: &[&str]) -> AbsUser {
        self.new_user_with(json!({"librariesAccessible": library_ids}))
            .await
    }

    /// Changes user `user_id`'s settings to `settings` (for example `permissions`,
    /// `librariesAccessible` or `itemTagsSelected`), as an admin editing them would.
    /// Audiobookshelf changes `librariesAccessible` and `itemTagsSelected` only alongside
    /// `permissions`.
    pub async fn update_user(&self, user_id: &str, settings: Value) {
        self.admin(
            reqwest::Method::PATCH,
            &format!("/api/users/{user_id}"),
            Some(settings),
        )
        .await;
    }

    /// Marks the audiobook or podcast `library_item` explicit or not, as an admin editing
    /// its details would.
    pub async fn mark_explicit(&self, library_item: &str, explicit: bool) {
        self.edit_media(library_item, json!({"metadata": {"explicit": explicit}}))
            .await;
    }

    /// Gives the audiobook or podcast `library_item` exactly the tags `tags`, as an admin
    /// editing its details would.
    pub async fn set_tags(&self, library_item: &str, tags: &[&str]) {
        self.edit_media(library_item, json!({"tags": tags})).await;
    }

    /// Changes the audiobook or podcast `library_item`'s details by `update`, as an admin
    /// editing them would.
    async fn edit_media(&self, library_item: &str, update: Value) {
        self.admin(
            reqwest::Method::PATCH,
            &format!("/api/items/{library_item}/media"),
            Some(update),
        )
        .await;
    }

    /// The episodes of `podcast`, as the root user sees them.
    async fn podcast_episodes(&self, podcast: &str) -> Vec<Value> {
        let mut item = self
            .admin(
                reqwest::Method::GET,
                &format!("/api/items/{podcast}?expanded=1"),
                None,
            )
            .await;
        match item["media"]["episodes"].take() {
            Value::Array(episodes) => episodes,
            episodes => panic!("episodes: {episodes}"),
        }
    }

    /// Creates the podcast Library "Crowded", holding one podcast of [`CROWDED_EPISODES`]
    /// episodes without publish dates, and waits until a scan has found them all. No test
    /// user sees it unless created to ([`Audiobookshelf::new_user_seeing`]). At most one test
    /// per test run may call this: the folder takes one Library.
    pub async fn crowded_podcast(&self) -> Crowded {
        let library = self.new_library("Crowded", CROWDED_FOLDER, "podcast").await;
        let deadline = Instant::now() + SCAN_TIMEOUT;
        let podcast = loop {
            let podcasts = self.library_items(&library).await;
            if let [podcast] = podcasts.as_slice()
                && podcast["media"]["numEpisodes"] == CROWDED_EPISODES
            {
                break podcast["id"].as_str().expect("podcast id").to_owned();
            }
            assert!(
                Instant::now() < deadline,
                "the crowded podcast did not appear within {SCAN_TIMEOUT:?}: {podcasts:?}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        let episodes = self
            .podcast_episodes(&podcast)
            .await
            .iter()
            .map(|episode| episode["id"].as_str().expect("episode id").to_owned())
            .collect();
        Crowded {
            library,
            podcast,
            episodes,
        }
    }

    /// Calls `path` as `user`, with their API key, as their own player would; panics unless
    /// it succeeds.
    pub async fn as_user(
        &self,
        user: &AbsUser,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Value {
        self.call(&user.api_key.key, method, path, body).await
    }

    /// Audio file `ino` of `library_item`, fetched whole and straight from Audiobookshelf as
    /// `user`; panics unless that succeeds.
    pub async fn file(&self, user: &AbsUser, library_item: &str, ino: &str) -> Vec<u8> {
        let path = format!("/api/items/{library_item}/file/{ino}");
        self.http
            .get(format!("{}{path}", self.base_url))
            .bearer_auth(&user.api_key.key)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .unwrap_or_else(|error| panic!("GET {path}: {error}"))
            .bytes()
            .await
            .expect("file body")
            .to_vec()
    }

    /// Creates a user with an API key and the Audiobookshelf user settings `extra` (for
    /// example `permissions` or `itemTagsSelected`) over the test defaults (only the seeded
    /// Libraries, explicit content allowed), as an admin would; `permissions` it doesn't name
    /// keep the default.
    pub async fn new_user_with(&self, extra: Value) -> AbsUser {
        static USERS: AtomicUsize = AtomicUsize::new(0);
        let username = format!("listener-{}", USERS.fetch_add(1, Ordering::Relaxed));
        let mut user = json!({
            "username": username,
            "password": "password",
            "type": "user",
            "isActive": true,
            "permissions": {"accessAllLibraries": false, "accessExplicitContent": true},
            "librariesAccessible": [self.seeded.audiobook_library, self.seeded.podcast_library],
        });
        let Value::Object(extra) = extra else {
            panic!("user settings must be an object: {extra}");
        };
        for (key, value) in extra {
            match value {
                Value::Object(permissions) if key == "permissions" => {
                    for (permission, value) in permissions {
                        user["permissions"][permission] = value;
                    }
                }
                value => user[key] = value,
            }
        }
        let created = self
            .admin(reqwest::Method::POST, "/api/users", Some(user))
            .await;
        let id = created["user"]["id"].as_str().expect("user id").to_owned();
        let api_key = self.new_api_key(&id).await;
        AbsUser {
            id,
            username,
            api_key,
        }
    }

    /// Creates another API key for user `user_id`.
    pub async fn new_api_key(&self, user_id: &str) -> IssuedKey {
        let created = self
            .admin(
                reqwest::Method::POST,
                "/api/api-keys",
                Some(json!({"name": "l-space test", "userId": user_id, "isActive": true})),
            )
            .await;
        IssuedKey {
            id: created["apiKey"]["id"].as_str().expect("key id").to_owned(),
            key: created["apiKey"]["apiKey"]
                .as_str()
                .expect("key")
                .to_owned(),
        }
    }

    /// Deletes `api_key`, as an admin revoking it would.
    pub async fn delete_api_key(&self, api_key: &IssuedKey) {
        self.admin(
            reqwest::Method::DELETE,
            &format!("/api/api-keys/{}", api_key.id),
            None,
        )
        .await;
    }

    /// Renames user `user_id`, as an admin editing them would.
    pub async fn rename_user(&self, user_id: &str, username: &str) {
        self.admin(
            reqwest::Method::PATCH,
            &format!("/api/users/{user_id}"),
            Some(json!({"username": username})),
        )
        .await;
    }

    /// Makes `api_key` act for user `user_id` instead, as an admin editing it would.
    pub async fn move_api_key(&self, api_key: &IssuedKey, user_id: &str) {
        self.admin(
            reqwest::Method::PATCH,
            &format!("/api/api-keys/{}", api_key.id),
            Some(json!({"userId": user_id})),
        )
        .await;
    }

    /// Who `api_key` acts for, according to Audiobookshelf itself.
    pub async fn whose_key(&self, api_key: &IssuedKey) -> Value {
        self.call(&api_key.key, reqwest::Method::GET, "/api/me", None)
            .await
    }

    /// Calls `path` as the root user, as an admin would.
    async fn admin(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
        self.call(&self.root_token, method, path, body).await
    }

    /// Calls `path` with `token` and returns the answer; panics unless it succeeds.
    async fn call(
        &self,
        token: &str,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Value {
        let mut request = self
            .http
            .request(method.clone(), format!("{}{path}", self.base_url))
            .bearer_auth(token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .unwrap_or_else(|error| panic!("{method} {path}: {error}"));
        let text = response.text().await.expect("response body");
        // Some admin calls answer with a bare `OK`.
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    }

    fn start() -> Self {
        let mut image: ContainerRequest<GenericImage> = GenericImage::new(IMAGE.0, IMAGE.1)
            .with_exposed_port(80.tcp())
            .with_wait_for(WaitFor::message_on_stdout("Listening on port"))
            .into();
        for (path, seconds) in AUDIO_FILES {
            image = image.with_copy_to(path, silence(seconds));
        }
        image = image.with_copy_to(EBOOK_FILE, MINIMAL_PDF.to_vec());
        for n in 1..=CROWDED_EPISODES {
            image = image.with_copy_to(
                format!("{CROWDED_FOLDER}/The Long Wait/Episode {n}.wav"),
                silence(1),
            );
        }
        let container = image
            .with_startup_timeout(STARTUP_TIMEOUT)
            .start()
            .expect("start the Audiobookshelf container");
        let host = container.get_host().expect("container host");
        let port = container.get_host_port_ipv4(80).expect("container port");
        let base_url = format!("http://{host}:{port}");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("seeding runtime");
        // Tests share this client from their own runtimes; a pooled connection would belong to
        // the runtime that opened it, which may be gone.
        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .build()
            .expect("HTTP client");
        let root_token = runtime.block_on(root_token(&http, &base_url));
        let mut abs = Self {
            container: Some(container),
            base_url,
            root_token,
            http,
            seeded: Seeded::default(),
        };
        abs.seeded = runtime.block_on(abs.seed());
        abs
    }

    /// Creates the audiobook and the podcast Library over the copied-in audio files, waits
    /// until a scan has found everything, and sets the metadata a scan can't find.
    async fn seed(&self) -> Seeded {
        let audiobook_library = self.new_library("Audiobooks", "/audiobooks", "book").await;
        let podcast_library = self.new_library("Podcasts", "/podcasts", "podcast").await;

        let deadline = Instant::now() + SCAN_TIMEOUT;
        let (books, podcasts) = loop {
            let books = self.library_items(&audiobook_library).await;
            let podcasts = self.library_items(&podcast_library).await;
            // Two audiobooks, and the ebook alone, without audio.
            let scanned = books.len() == 3
                && books
                    .iter()
                    .filter(|book| book["media"]["numAudioFiles"].as_u64() > Some(0))
                    .count()
                    == 2
                && podcasts.len() == 1
                && podcasts[0]["media"]["numEpisodes"] == 3;
            if scanned {
                break (books, podcasts);
            }
            assert!(
                Instant::now() < deadline,
                "the seeded audiobooks and podcast did not appear within {SCAN_TIMEOUT:?}: \
                 {books:?} {podcasts:?}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        };

        let id = |item: &Value| item["id"].as_str().expect("library item id").to_owned();
        let long_book = books
            .iter()
            .find(|book| book["media"]["metadata"]["title"] == "The Long Book")
            .map(id)
            .expect("The Long Book");
        let silence = books
            .iter()
            .find(|book| book["media"]["metadata"]["title"] == "Silence")
            .map(id)
            .expect("Silence");
        let ebook_only = books
            .iter()
            .find(|book| book["media"]["metadata"]["title"] == "Only Words")
            .map(id)
            .expect("Only Words");
        let podcast = id(&podcasts[0]);
        self.edit_media(
            &long_book,
            json!({"metadata": {"description": "A book that goes on."}}),
        )
        .await;
        self.edit_media(
            &podcast,
            json!({"metadata": {"author": "Ankh-Morpork Radio"}}),
        )
        .await;

        let found = self.podcast_episodes(&podcast).await;
        let episodes: [String; 3] = std::array::from_fn(|n| {
            let title = format!("Episode {}", n + 1);
            found
                .iter()
                .find(|episode| episode["title"] == title.as_str())
                .map(id)
                .unwrap_or_else(|| panic!("{title}"))
        });
        for (n, (episode, published_at)) in episodes.iter().zip(EPISODE_PUBLISHED_AT).enumerate() {
            let mut update = json!({"publishedAt": published_at});
            if n == 0 {
                update["description"] = "The first one.".into();
            }
            self.admin(
                reqwest::Method::PATCH,
                &format!("/api/podcasts/{podcast}/episode/{episode}"),
                Some(update),
            )
            .await;
        }

        Seeded {
            audiobook_library,
            podcast_library,
            long_book,
            silence,
            ebook_only,
            podcast,
            episodes,
        }
    }

    /// Creates a Library of `media_type` over `folder`, starts a scan, and returns its id.
    async fn new_library(&self, name: &str, folder: &str, media_type: &str) -> String {
        let library = self
            .admin(
                reqwest::Method::POST,
                "/api/libraries",
                Some(json!({
                    "name": name,
                    "folders": [{"fullPath": folder}],
                    "mediaType": media_type,
                })),
            )
            .await;
        let id = library["id"].as_str().expect("library id").to_owned();
        self.admin(
            reqwest::Method::POST,
            &format!("/api/libraries/{id}/scan"),
            None,
        )
        .await;
        id
    }

    /// Everything in Library `library_id`, as the root user sees it.
    async fn library_items(&self, library_id: &str) -> Vec<Value> {
        let mut items = self
            .admin(
                reqwest::Method::GET,
                &format!("/api/libraries/{library_id}/items"),
                None,
            )
            .await;
        match items["results"].take() {
            Value::Array(results) => results,
            results => panic!("library items: {results}"),
        }
    }
}

impl Drop for Audiobookshelf {
    fn drop(&mut self) {
        // Removing the container blocks on the sync runner's runtime: not on a test's thread.
        if let Some(container) = self.container.take() {
            let _ = std::thread::spawn(move || drop(container)).join();
        }
    }
}

/// Creates the root user and logs in as it. Returns its access token.
async fn root_token(http: &reqwest::Client, base_url: &str) -> String {
    http.post(format!("{base_url}/init"))
        .json(&json!({"newRoot": {"username": "root", "password": ROOT_PASSWORD}}))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .expect("POST /init");
    let login: Value = http
        .post(format!("{base_url}/login"))
        .header("x-return-tokens", "true")
        .json(&json!({"username": "root", "password": ROOT_PASSWORD}))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .expect("POST /login")
        .json()
        .await
        .expect("decode /login");
    login["user"]["accessToken"]
        .as_str()
        .expect("access token")
        .to_owned()
}

/// `seconds` of near-silence as a WAV file: 100 Hz, 8-bit mono, so an hour takes only 360 kB.
/// Low as the rate is, Audiobookshelf scans it as audio of that length. The samples are quiet
/// noise rather than silence, so every byte depends on its offset and a byte range served
/// from the wrong place shows.
fn silence(seconds: u32) -> Vec<u8> {
    const RATE: u32 = 100;
    // Unsigned 8-bit samples: 0x80 is silence.
    const SILENT: u8 = 0x80;
    let data_len = RATE * seconds;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&RATE.to_le_bytes()); // bytes per second
    wav.extend_from_slice(&1u16.to_le_bytes()); // block align
    wav.extend_from_slice(&8u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    // A xorshift generator's low four bits, around silence: -8 to +7 of 255.
    let mut state = 0x2545_F491_u32 ^ seconds;
    wav.extend((0..data_len).map(|_| {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        SILENT - 8 + (state & 0x0F) as u8
    }));
    wav
}
