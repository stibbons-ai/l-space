//! The contract every [`Store`] implementation must pass (ADR 0008).
//!
//! An implementation's tests expand [`store_contract_tests!`](crate::store_contract_tests)
//! with an expression that makes a fresh [`Backend`]; it generates one `#[tokio::test]` per
//! contract test, so the implementing crate needs `tokio` (with `macros` and a runtime) as a
//! dev-dependency.
//!
//! ```ignore
//! store::store_contract_tests!(MyBackend::new());
//! ```

use abs::ApiKey;

use crate::{LibraryUser, Session, SessionId, Store};

/// Empty storage that a store can be opened on, any number of times in turn: what one opened
/// store wrote, the next one opened must see.
pub trait Backend {
    type Store: Store;

    fn open(&self) -> impl Future<Output = Self::Store>;
}

/// Generates the contract suite as tests, each on a fresh backend from `$backend`.
#[macro_export]
macro_rules! store_contract_tests {
    ($backend:expr) => {
        $crate::store_contract_tests!(
            @tests $backend;
            an_open_session_acts_as_its_user_with_its_key,
            an_unknown_session_is_not_open,
            closing_a_session_ends_only_that_session,
            closing_an_unknown_session_reports_it_was_not_open,
            sessions_and_keys_survive_reopening,
            logging_in_again_with_the_same_key_keeps_other_sessions,
            a_new_key_ends_the_sessions_of_the_replaced_key,
            revoking_a_key_ends_all_its_sessions_and_nothing_else,
            revoking_a_key_that_was_since_replaced_changes_nothing,
            users_are_distinct_per_library_server,
        );
    };
    (@tests $backend:expr; $($test:ident),* $(,)?) => {
        $(
            #[::tokio::test]
            async fn $test() {
                $crate::contract::$test(&$backend).await;
            }
        )*
    };
}

fn user(library_server: &str, user_id: &str) -> LibraryUser {
    LibraryUser {
        library_server: library_server.into(),
        user_id: user_id.into(),
    }
}

fn alice() -> LibraryUser {
    user("https://abs.lan", "alice-id")
}

fn bob() -> LibraryUser {
    user("https://abs.lan", "bob-id")
}

fn key(key: &str) -> ApiKey {
    ApiKey::new(key)
}

fn session_id(n: u8) -> SessionId {
    SessionId::of_token(&format!("token-{n}"))
}

async fn open(store: &impl Store, id: SessionId, user: &LibraryUser, api_key: &str) {
    store
        .open_session(&id, user, &key(api_key))
        .await
        .expect("open session");
}

async fn session(store: &impl Store, id: SessionId) -> Option<Session> {
    store.session(&id).await.expect("look up session")
}

fn acting_as(user: &LibraryUser, api_key: &str) -> Option<Session> {
    Some(Session {
        user: user.clone(),
        api_key: key(api_key),
    })
}

pub async fn an_open_session_acts_as_its_user_with_its_key(backend: &impl Backend) {
    let store = backend.open().await;
    open(&store, session_id(1), &alice(), "alice-key").await;

    assert_eq!(
        session(&store, session_id(1)).await,
        acting_as(&alice(), "alice-key")
    );
}

pub async fn an_unknown_session_is_not_open(backend: &impl Backend) {
    let store = backend.open().await;
    assert_eq!(session(&store, session_id(1)).await, None);

    open(&store, session_id(1), &alice(), "alice-key").await;
    assert_eq!(session(&store, session_id(2)).await, None);
}

pub async fn closing_a_session_ends_only_that_session(backend: &impl Backend) {
    let store = backend.open().await;
    open(&store, session_id(1), &alice(), "alice-key").await;
    open(&store, session_id(2), &alice(), "alice-key").await;

    assert!(store.close_session(&session_id(1)).await.expect("close"));

    assert_eq!(session(&store, session_id(1)).await, None);
    assert_eq!(
        session(&store, session_id(2)).await,
        acting_as(&alice(), "alice-key")
    );
}

pub async fn closing_an_unknown_session_reports_it_was_not_open(backend: &impl Backend) {
    let store = backend.open().await;
    assert!(!store.close_session(&session_id(1)).await.expect("close"));

    open(&store, session_id(1), &alice(), "alice-key").await;
    assert!(store.close_session(&session_id(1)).await.expect("close"));
    assert!(
        !store
            .close_session(&session_id(1))
            .await
            .expect("close again")
    );
}

pub async fn sessions_and_keys_survive_reopening(backend: &impl Backend) {
    {
        let store = backend.open().await;
        open(&store, session_id(1), &alice(), "alice-key").await;
        open(&store, session_id(2), &bob(), "bob-key").await;
        assert!(store.close_session(&session_id(2)).await.expect("close"));
    }

    let store = backend.open().await;
    assert_eq!(
        session(&store, session_id(1)).await,
        acting_as(&alice(), "alice-key")
    );
    assert_eq!(session(&store, session_id(2)).await, None);
}

pub async fn logging_in_again_with_the_same_key_keeps_other_sessions(backend: &impl Backend) {
    let store = backend.open().await;
    open(&store, session_id(1), &alice(), "alice-key").await;
    open(&store, session_id(2), &alice(), "alice-key").await;

    assert_eq!(
        session(&store, session_id(1)).await,
        acting_as(&alice(), "alice-key")
    );
    assert_eq!(
        session(&store, session_id(2)).await,
        acting_as(&alice(), "alice-key")
    );
}

pub async fn a_new_key_ends_the_sessions_of_the_replaced_key(backend: &impl Backend) {
    let store = backend.open().await;
    open(&store, session_id(2), &bob(), "bob-key").await;
    // Opened last, so a backend that reuses the newest row's identity would hand the old
    // session to the new key.
    open(&store, session_id(1), &alice(), "old-key").await;

    open(&store, session_id(3), &alice(), "new-key").await;

    assert_eq!(session(&store, session_id(1)).await, None);
    assert_eq!(
        session(&store, session_id(3)).await,
        acting_as(&alice(), "new-key")
    );
    assert_eq!(
        session(&store, session_id(2)).await,
        acting_as(&bob(), "bob-key")
    );
}

pub async fn revoking_a_key_ends_all_its_sessions_and_nothing_else(backend: &impl Backend) {
    let store = backend.open().await;
    open(&store, session_id(1), &alice(), "alice-key").await;
    open(&store, session_id(2), &alice(), "alice-key").await;
    open(&store, session_id(3), &bob(), "bob-key").await;

    store
        .revoke_api_key(&alice(), &key("alice-key"))
        .await
        .expect("revoke");

    assert_eq!(session(&store, session_id(1)).await, None);
    assert_eq!(session(&store, session_id(2)).await, None);
    assert_eq!(
        session(&store, session_id(3)).await,
        acting_as(&bob(), "bob-key")
    );

    // The user can log in again afterwards, with the same key or another.
    open(&store, session_id(4), &alice(), "alice-key").await;
    assert_eq!(
        session(&store, session_id(4)).await,
        acting_as(&alice(), "alice-key")
    );
}

pub async fn revoking_a_key_that_was_since_replaced_changes_nothing(backend: &impl Backend) {
    let store = backend.open().await;
    open(&store, session_id(1), &alice(), "old-key").await;
    open(&store, session_id(2), &alice(), "new-key").await;

    store
        .revoke_api_key(&alice(), &key("old-key"))
        .await
        .expect("revoke");

    assert_eq!(
        session(&store, session_id(2)).await,
        acting_as(&alice(), "new-key")
    );
}

pub async fn users_are_distinct_per_library_server(backend: &impl Backend) {
    let store = backend.open().await;
    let home = user("https://abs.lan", "same-id");
    let office = user("https://abs.office.lan", "same-id");
    open(&store, session_id(1), &home, "home-key").await;
    open(&store, session_id(2), &office, "office-key").await;

    assert_eq!(
        session(&store, session_id(1)).await,
        acting_as(&home, "home-key")
    );
    assert_eq!(
        session(&store, session_id(2)).await,
        acting_as(&office, "office-key")
    );

    store
        .revoke_api_key(&home, &key("home-key"))
        .await
        .expect("revoke");
    assert_eq!(session(&store, session_id(1)).await, None);
    assert_eq!(
        session(&store, session_id(2)).await,
        acting_as(&office, "office-key")
    );
}
