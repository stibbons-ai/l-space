//! A real Audiobookshelf in a container, seeded with a one-book Library, that tests share.
//!
//! [`Audiobookshelf::shared`] starts the container on first use and hands every test the same
//! one while any test holds it; the last holder's drop removes it, so no container outlives
//! the test run. Tests stay apart by each creating their own users with
//! [`Audiobookshelf::new_user`] and acting only on those. The seeded Library is shared by all
//! tests, and users are never deleted while the container runs.
//!
//! Setup goes through Audiobookshelf's REST API as its root user, the way an admin would,
//! never through its database: users are created there, and API keys created, moved and
//! deleted.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};

/// Pinned, so a new Audiobookshelf release changes nothing until we move to it.
const IMAGE: (&str, &str) = ("ghcr.io/advplyr/audiobookshelf", "2.37.2");

/// Image pull and first start can be slow on a cold machine.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(300);

/// How long the seeded book may take to appear after a scan.
const SCAN_TIMEOUT: Duration = Duration::from_secs(60);

const ROOT_PASSWORD: &str = "root-password";

static SHARED: Mutex<Weak<Audiobookshelf>> = Mutex::new(Weak::new());

/// An Audiobookshelf user a test created, with an API key for it.
#[derive(Debug)]
pub struct AbsUser {
    pub id: String,
    pub username: String,
    pub api_key: IssuedKey,
}

/// An API key as Audiobookshelf issued it.
#[derive(Debug)]
pub struct IssuedKey {
    /// Audiobookshelf's id for the key, to delete it by.
    pub id: String,
    /// The key itself.
    pub key: String,
}

/// A running Audiobookshelf, removed on drop.
#[derive(Debug)]
pub struct Audiobookshelf {
    container: Option<Container<GenericImage>>,
    base_url: String,
    root_token: String,
    http: reqwest::Client,
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

    /// Its `host:port`, for a forwarder in front of it.
    pub fn address(&self) -> &str {
        self.base_url.trim_start_matches("http://")
    }

    /// Creates a user with an API key, named uniquely across the test run.
    pub async fn new_user(&self) -> AbsUser {
        static USERS: AtomicUsize = AtomicUsize::new(0);
        let username = format!("listener-{}", USERS.fetch_add(1, Ordering::Relaxed));
        let created = self
            .admin(
                reqwest::Method::POST,
                "/api/users",
                Some(json!({
                    "username": username,
                    "password": "password",
                    "type": "user",
                    "isActive": true,
                })),
            )
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
        self.http
            .get(format!("{}/api/me", self.base_url))
            .bearer_auth(&api_key.key)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .expect("GET /api/me")
            .json()
            .await
            .expect("decode /api/me")
    }

    async fn admin(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
        let mut request = self
            .http
            .request(method.clone(), format!("{}{path}", self.base_url))
            .bearer_auth(&self.root_token);
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
        let container = GenericImage::new(IMAGE.0, IMAGE.1)
            .with_exposed_port(80.tcp())
            .with_wait_for(WaitFor::message_on_stdout("Listening on port"))
            .with_copy_to("/audiobooks/L-Space/Silence/silence.wav", silent_wav())
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
        let abs = Self {
            container: Some(container),
            base_url,
            root_token,
            http,
        };
        runtime.block_on(abs.seed_library());
        abs
    }

    /// Creates a book Library holding the copied-in audio file, and waits until a scan has
    /// found the book.
    async fn seed_library(&self) {
        let library = self
            .admin(
                reqwest::Method::POST,
                "/api/libraries",
                Some(json!({
                    "name": "Audiobooks",
                    "folders": [{"fullPath": "/audiobooks"}],
                    "mediaType": "book",
                })),
            )
            .await;
        let library_id = library["id"].as_str().expect("library id");
        self.admin(
            reqwest::Method::POST,
            &format!("/api/libraries/{library_id}/scan"),
            None,
        )
        .await;

        let deadline = Instant::now() + SCAN_TIMEOUT;
        loop {
            let items = self
                .admin(
                    reqwest::Method::GET,
                    &format!("/api/libraries/{library_id}/items"),
                    None,
                )
                .await;
            if items["total"] == 1 {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the seeded book did not appear within {SCAN_TIMEOUT:?}: {items}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
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

/// One second of 8 kHz, 16-bit mono silence as a WAV file: the smallest audio Audiobookshelf
/// scans as a book, generated so the repository holds no binary.
fn silent_wav() -> Vec<u8> {
    const RATE: u32 = 8_000;
    let data_len = RATE * 2;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 2).to_le_bytes()); // bytes per second
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.resize(44 + data_len as usize, 0);
    wav
}
