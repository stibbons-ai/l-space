//! Searching for players is timed by the injected clock: a player added to the household is
//! found at the next search, [`REFRESH`] later on the clock, and one that stops answering is
//! forgotten [`EXPIRY`] after it last answered. Against a `sonos-fake` household on loopback
//! sharing a manual clock with [`Upnp`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use sonos::{EXPIRY, Error, Group, REFRESH, Sonos, Upnp};
use sonos_fake::Household;

/// The names of the Groups `sonos` lists, or its error.
async fn names(sonos: &Upnp) -> Result<Vec<String>, Error> {
    let groups: Vec<Group> = sonos.groups().await?;
    Ok(groups.into_iter().map(|group| group.name).collect())
}

/// Waits (in real time, for a search to finish) until `sonos` lists `expected`.
async fn lists(sonos: &Upnp, expected: Result<Vec<String>, Error>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let listed = names(sonos).await;
        if listed == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "lists {listed:?}, not {expected:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn players_are_searched_for_on_the_clocks_refresh_and_expire() {
    let (household, clock) = Household::with_manual_clock();
    let sonos = Upnp::start(household.ssdp(), Arc::new(clock.clone()));

    // The first search, at start, finds nobody.
    assert_eq!(names(&sonos).await, Ok(Vec::new()));

    // A player plugged in now is not searched for until the clock says so...
    let kitchen = household.add_player("Kitchen");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(names(&sonos).await, Ok(Vec::new()));

    // ...which is one refresh later.
    clock.advance(REFRESH);
    lists(&sonos, Ok(vec!["Kitchen".into()])).await;

    // Unplugged, it is still known but answers nothing...
    household.remove(&kitchen);
    assert_eq!(names(&sonos).await, Err(Error::Unreachable));
    clock.advance(EXPIRY - Duration::from_secs(1));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(names(&sonos).await, Err(Error::Unreachable));

    // ...until it has not answered a search for the expiry time.
    clock.advance(Duration::from_secs(1) + REFRESH);
    lists(&sonos, Ok(Vec::new())).await;
}
