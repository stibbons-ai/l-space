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

use crate::{
    ActiveHandoff, GroupHandoff, Handoff, HandoffFile, LibraryUser, Session, SessionId, Store,
    StreamId,
};

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
            a_second_key_for_the_same_user_keeps_the_first_keys_sessions,
            revoking_a_key_ends_all_its_sessions_and_nothing_else,
            revoking_one_key_keeps_the_users_other_keys,
            a_new_key_never_inherits_the_sessions_of_a_revoked_one,
            revoking_is_scoped_to_the_library_server,
            revoked_keys_stay_revoked_after_reopening,
            revoking_a_key_for_one_user_keeps_it_for_another,
            users_are_distinct_per_library_server,
            a_started_handoff_streams_with_its_starters_key,
            an_unknown_stream_is_no_handoff,
            a_new_handoff_on_a_group_replaces_its_old_one,
            handoffs_on_other_groups_stay_apart,
            handoffs_survive_reopening,
            a_handoff_outlives_the_session_that_started_it,
            starting_a_handoff_keeps_the_starters_sessions,
            ending_a_handoff_ends_only_it,
            revoking_a_key_ends_the_handoffs_acting_with_it,
            ending_a_replaced_stream_leaves_its_replacement,
            a_groups_handoff_is_found_with_its_stream_and_starters_key,
            a_group_without_a_handoff_has_none,
            a_groups_handoff_is_the_one_that_replaced_its_old_one,
            a_groups_handoff_ends_with_its_stream_or_its_key,
            a_groups_handoff_outlives_the_session_that_started_it,
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

pub async fn a_second_key_for_the_same_user_keeps_the_first_keys_sessions(backend: &impl Backend) {
    let store = backend.open().await;
    open(&store, session_id(1), &alice(), "phone-key").await;
    open(&store, session_id(2), &alice(), "tablet-key").await;

    assert_eq!(
        session(&store, session_id(1)).await,
        acting_as(&alice(), "phone-key")
    );
    assert_eq!(
        session(&store, session_id(2)).await,
        acting_as(&alice(), "tablet-key")
    );
}

pub async fn revoking_a_key_ends_all_its_sessions_and_nothing_else(backend: &impl Backend) {
    let store = backend.open().await;
    open(&store, session_id(1), &alice(), "alice-key").await;
    open(&store, session_id(2), &alice(), "alice-key").await;
    open(&store, session_id(3), &bob(), "bob-key").await;

    assert!(
        store
            .revoke_api_key(&alice(), &key("alice-key"))
            .await
            .expect("revoke")
    );
    assert!(
        !store
            .revoke_api_key(&alice(), &key("alice-key"))
            .await
            .expect("revoke again"),
        "a revoked key is no longer stored"
    );

    assert_eq!(session(&store, session_id(1)).await, None);
    assert_eq!(session(&store, session_id(2)).await, None);
    assert_eq!(
        session(&store, session_id(3)).await,
        acting_as(&bob(), "bob-key")
    );

    // The user can log in again afterwards.
    open(&store, session_id(4), &alice(), "alice-key").await;
    assert_eq!(
        session(&store, session_id(4)).await,
        acting_as(&alice(), "alice-key")
    );
}

pub async fn revoking_one_key_keeps_the_users_other_keys(backend: &impl Backend) {
    let store = backend.open().await;
    open(&store, session_id(1), &alice(), "phone-key").await;
    open(&store, session_id(2), &alice(), "tablet-key").await;

    store
        .revoke_api_key(&alice(), &key("phone-key"))
        .await
        .expect("revoke");

    assert_eq!(session(&store, session_id(1)).await, None);
    assert_eq!(
        session(&store, session_id(2)).await,
        acting_as(&alice(), "tablet-key")
    );
}

pub async fn a_new_key_never_inherits_the_sessions_of_a_revoked_one(backend: &impl Backend) {
    let store = backend.open().await;
    open(&store, session_id(2), &bob(), "bob-key").await;
    // Stored last, so a backend that reuses the newest row's identity would hand its sessions
    // to the next key.
    open(&store, session_id(1), &alice(), "old-key").await;
    store
        .revoke_api_key(&alice(), &key("old-key"))
        .await
        .expect("revoke");

    open(&store, session_id(3), &alice(), "new-key").await;

    assert_eq!(session(&store, session_id(1)).await, None);
    assert_eq!(
        session(&store, session_id(3)).await,
        acting_as(&alice(), "new-key")
    );
}

pub async fn revoking_is_scoped_to_the_library_server(backend: &impl Backend) {
    let store = backend.open().await;
    let home = user("https://abs.lan", "same-id");
    let office = user("https://abs.office.lan", "same-id");
    open(&store, session_id(1), &home, "same-key").await;
    open(&store, session_id(2), &office, "same-key").await;

    store
        .revoke_api_key(&home, &key("same-key"))
        .await
        .expect("revoke");

    assert_eq!(session(&store, session_id(1)).await, None);
    assert_eq!(
        session(&store, session_id(2)).await,
        acting_as(&office, "same-key")
    );
}

pub async fn revoked_keys_stay_revoked_after_reopening(backend: &impl Backend) {
    {
        let store = backend.open().await;
        open(&store, session_id(1), &alice(), "alice-key").await;
        open(&store, session_id(2), &bob(), "bob-key").await;
        store
            .revoke_api_key(&alice(), &key("alice-key"))
            .await
            .expect("revoke");
    }

    let store = backend.open().await;
    assert_eq!(session(&store, session_id(1)).await, None);
    assert_eq!(
        session(&store, session_id(2)).await,
        acting_as(&bob(), "bob-key")
    );
}

/// An admin can move a key to another user, so one key may be stored for two users.
pub async fn revoking_a_key_for_one_user_keeps_it_for_another(backend: &impl Backend) {
    let store = backend.open().await;
    open(&store, session_id(1), &alice(), "moved-key").await;
    open(&store, session_id(2), &bob(), "moved-key").await;

    store
        .revoke_api_key(&alice(), &key("moved-key"))
        .await
        .expect("revoke");

    assert_eq!(session(&store, session_id(1)).await, None);
    assert_eq!(
        session(&store, session_id(2)).await,
        acting_as(&bob(), "moved-key")
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

fn stream(n: u8) -> StreamId {
    StreamId::of(&format!("stream-{n}"))
}

/// A Handoff of episode `episode` on Group `group`, started by `user` at `started_at`.
fn handoff(group: &str, episode: &str, user: &LibraryUser, started_at: i64) -> Handoff {
    Handoff {
        group: group.into(),
        library_item: "podcast-id".into(),
        episode: Some(episode.into()),
        file: HandoffFile {
            ino: format!("{episode}-ino"),
            name: format!("{episode}.mp3"),
        },
        started_by: user.clone(),
        username: format!("{}-name", user.user_id),
        started_at,
    }
}

async fn start(store: &impl Store, stream: StreamId, handoff: &Handoff, api_key: &str) {
    store
        .start_handoff(&stream, handoff, &key(api_key))
        .await
        .expect("start handoff");
}

async fn active(store: &impl Store, stream: StreamId) -> Option<ActiveHandoff> {
    store.handoff(&stream).await.expect("look up handoff")
}

async fn listed(store: &impl Store) -> Vec<Handoff> {
    store.handoffs().await.expect("list handoffs")
}

fn acting_with(handoff: &Handoff, api_key: &str) -> Option<ActiveHandoff> {
    Some(ActiveHandoff {
        handoff: handoff.clone(),
        api_key: key(api_key),
    })
}

pub async fn a_started_handoff_streams_with_its_starters_key(backend: &impl Backend) {
    let store = backend.open().await;
    let kitchen = handoff("RINCON_K", "ep-1", &alice(), 1_000);
    start(&store, stream(1), &kitchen, "alice-key").await;

    assert_eq!(
        active(&store, stream(1)).await,
        acting_with(&kitchen, "alice-key")
    );
    assert_eq!(listed(&store).await, [kitchen]);
}

pub async fn an_unknown_stream_is_no_handoff(backend: &impl Backend) {
    let store = backend.open().await;
    assert_eq!(active(&store, stream(1)).await, None);
    assert_eq!(listed(&store).await, []);

    start(
        &store,
        stream(1),
        &handoff("RINCON_K", "ep-1", &alice(), 1_000),
        "alice-key",
    )
    .await;
    assert_eq!(active(&store, stream(2)).await, None);
}

pub async fn a_new_handoff_on_a_group_replaces_its_old_one(backend: &impl Backend) {
    let store = backend.open().await;
    let first = handoff("RINCON_K", "ep-1", &alice(), 1_000);
    start(&store, stream(1), &first, "alice-key").await;
    let second = handoff("RINCON_K", "ep-2", &bob(), 2_000);
    start(&store, stream(2), &second, "bob-key").await;

    assert_eq!(active(&store, stream(1)).await, None);
    assert_eq!(
        active(&store, stream(2)).await,
        acting_with(&second, "bob-key")
    );
    assert_eq!(listed(&store).await, [second]);
}

pub async fn handoffs_on_other_groups_stay_apart(backend: &impl Backend) {
    let store = backend.open().await;
    let office = handoff("RINCON_O", "ep-2", &bob(), 2_000);
    let kitchen = handoff("RINCON_K", "ep-1", &alice(), 1_000);
    start(&store, stream(2), &office, "bob-key").await;
    start(&store, stream(1), &kitchen, "alice-key").await;

    assert_eq!(
        active(&store, stream(1)).await,
        acting_with(&kitchen, "alice-key")
    );
    assert_eq!(
        active(&store, stream(2)).await,
        acting_with(&office, "bob-key")
    );
    // The earliest started first.
    assert_eq!(listed(&store).await, [kitchen, office]);
}

pub async fn handoffs_survive_reopening(backend: &impl Backend) {
    let kitchen = handoff("RINCON_K", "ep-1", &alice(), 1_000);
    {
        let store = backend.open().await;
        start(&store, stream(1), &kitchen, "alice-key").await;
    }

    let store = backend.open().await;
    assert_eq!(
        active(&store, stream(1)).await,
        acting_with(&kitchen, "alice-key")
    );
    assert_eq!(listed(&store).await, [kitchen]);
}

pub async fn a_handoff_outlives_the_session_that_started_it(backend: &impl Backend) {
    let store = backend.open().await;
    open(&store, session_id(1), &alice(), "alice-key").await;
    let kitchen = handoff("RINCON_K", "ep-1", &alice(), 1_000);
    start(&store, stream(1), &kitchen, "alice-key").await;

    assert!(store.close_session(&session_id(1)).await.expect("close"));

    assert_eq!(
        active(&store, stream(1)).await,
        acting_with(&kitchen, "alice-key")
    );
}

/// The Handoff and the session share the stored key; each keeps it.
pub async fn starting_a_handoff_keeps_the_starters_sessions(backend: &impl Backend) {
    let store = backend.open().await;
    open(&store, session_id(1), &alice(), "alice-key").await;
    start(
        &store,
        stream(1),
        &handoff("RINCON_K", "ep-1", &alice(), 1_000),
        "alice-key",
    )
    .await;

    assert_eq!(
        session(&store, session_id(1)).await,
        acting_as(&alice(), "alice-key")
    );
}

pub async fn ending_a_handoff_ends_only_it(backend: &impl Backend) {
    let store = backend.open().await;
    let kitchen = handoff("RINCON_K", "ep-1", &alice(), 1_000);
    let office = handoff("RINCON_O", "ep-2", &alice(), 2_000);
    start(&store, stream(1), &kitchen, "alice-key").await;
    start(&store, stream(2), &office, "alice-key").await;

    assert!(store.end_handoff(&stream(1)).await.expect("end"));
    assert!(!store.end_handoff(&stream(1)).await.expect("end again"));

    assert_eq!(active(&store, stream(1)).await, None);
    assert_eq!(listed(&store).await, [office]);
}

pub async fn ending_a_replaced_stream_leaves_its_replacement(backend: &impl Backend) {
    let store = backend.open().await;
    let first = handoff("RINCON_K", "ep-1", &alice(), 1_000);
    start(&store, stream(1), &first, "alice-key").await;
    let second = handoff("RINCON_K", "ep-2", &bob(), 2_000);
    start(&store, stream(2), &second, "bob-key").await;

    assert!(!store.end_handoff(&stream(1)).await.expect("end replaced"));

    assert_eq!(
        active(&store, stream(2)).await,
        acting_with(&second, "bob-key")
    );
    assert_eq!(listed(&store).await, [second]);
}

pub async fn revoking_a_key_ends_the_handoffs_acting_with_it(backend: &impl Backend) {
    let store = backend.open().await;
    start(
        &store,
        stream(1),
        &handoff("RINCON_K", "ep-1", &alice(), 1_000),
        "alice-key",
    )
    .await;
    let office = handoff("RINCON_O", "ep-2", &bob(), 2_000);
    start(&store, stream(2), &office, "bob-key").await;

    assert!(
        store
            .revoke_api_key(&alice(), &key("alice-key"))
            .await
            .expect("revoke")
    );

    assert_eq!(active(&store, stream(1)).await, None);
    assert_eq!(listed(&store).await, [office]);
}

async fn on_group(store: &impl Store, group: &str) -> Option<GroupHandoff> {
    store
        .group_handoff(group)
        .await
        .expect("look up a Group's handoff")
}

fn on_stream(stream: StreamId, handoff: &Handoff, api_key: &str) -> Option<GroupHandoff> {
    acting_with(handoff, api_key).map(|active| GroupHandoff { stream, active })
}

pub async fn a_groups_handoff_is_found_with_its_stream_and_starters_key(backend: &impl Backend) {
    let store = backend.open().await;
    let kitchen = handoff("RINCON_K", "ep-1", &alice(), 1_000);
    let office = handoff("RINCON_O", "ep-2", &bob(), 2_000);
    start(&store, stream(1), &kitchen, "alice-key").await;
    start(&store, stream(2), &office, "bob-key").await;

    assert_eq!(
        on_group(&store, "RINCON_K").await,
        on_stream(stream(1), &kitchen, "alice-key")
    );
    assert_eq!(
        on_group(&store, "RINCON_O").await,
        on_stream(stream(2), &office, "bob-key")
    );
}

pub async fn a_group_without_a_handoff_has_none(backend: &impl Backend) {
    let store = backend.open().await;
    assert_eq!(on_group(&store, "RINCON_K").await, None);

    start(
        &store,
        stream(1),
        &handoff("RINCON_K", "ep-1", &alice(), 1_000),
        "alice-key",
    )
    .await;
    assert_eq!(on_group(&store, "RINCON_O").await, None);
    // Group ids are exact.
    assert_eq!(on_group(&store, "rincon_k").await, None);
}

pub async fn a_groups_handoff_is_the_one_that_replaced_its_old_one(backend: &impl Backend) {
    let store = backend.open().await;
    start(
        &store,
        stream(1),
        &handoff("RINCON_K", "ep-1", &alice(), 1_000),
        "alice-key",
    )
    .await;
    let second = handoff("RINCON_K", "ep-2", &bob(), 2_000);
    start(&store, stream(2), &second, "bob-key").await;

    assert_eq!(
        on_group(&store, "RINCON_K").await,
        on_stream(stream(2), &second, "bob-key")
    );
}

pub async fn a_groups_handoff_ends_with_its_stream_or_its_key(backend: &impl Backend) {
    let store = backend.open().await;
    let kitchen = handoff("RINCON_K", "ep-1", &alice(), 1_000);
    let office = handoff("RINCON_O", "ep-2", &bob(), 2_000);
    start(&store, stream(1), &kitchen, "alice-key").await;
    start(&store, stream(2), &office, "bob-key").await;

    assert!(store.end_handoff(&stream(1)).await.expect("end"));
    assert_eq!(on_group(&store, "RINCON_K").await, None);

    assert!(
        store
            .revoke_api_key(&bob(), &key("bob-key"))
            .await
            .expect("revoke")
    );
    assert_eq!(on_group(&store, "RINCON_O").await, None);
}

pub async fn a_groups_handoff_outlives_the_session_that_started_it(backend: &impl Backend) {
    let kitchen = handoff("RINCON_K", "ep-1", &alice(), 1_000);
    {
        let store = backend.open().await;
        open(&store, session_id(1), &alice(), "alice-key").await;
        start(&store, stream(1), &kitchen, "alice-key").await;
        assert!(store.close_session(&session_id(1)).await.expect("close"));
    }

    let store = backend.open().await;
    assert_eq!(
        on_group(&store, "RINCON_K").await,
        on_stream(stream(1), &kitchen, "alice-key")
    );
}
