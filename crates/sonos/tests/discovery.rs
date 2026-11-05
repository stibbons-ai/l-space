//! Searching for players is timed by the injected clock: a player added to the household is
//! found at the next search, [`REFRESH`] later on the clock, and one that stops answering is
//! forgotten [`EXPIRY`] after it last answered. Against a `sonos-fake` household on loopback
//! sharing a manual clock with [`Upnp`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use clock::ManualClock;
use sonos::{EXPIRY, Error, Group, REFRESH, Sonos, Upnp};
use sonos_fake::Household;

/// The names of the Groups `sonos` lists, or its error.
async fn names(sonos: &Upnp) -> Result<Vec<String>, Error> {
    let groups: Vec<Group> = sonos.groups().await?;
    Ok(groups.into_iter().map(|group| group.name).collect())
}

#[tokio::test(flavor = "multi_thread")]
async fn players_are_searched_for_on_the_clocks_refresh_and_expire() {
    let (household, clock) = Household::with_manual_clock();
    let sonos = Upnp::start(household.ssdp(), Arc::new(clock.clone()));

    // The first search, at start, finds nobody.
    parked(&household, &clock, 1).await;
    assert_eq!(names(&sonos).await, Ok(Vec::new()));

    // A player plugged in now is not searched for while the search is parked on the clock...
    let kitchen = household.add_player("Kitchen");
    assert_eq!(names(&sonos).await, Ok(Vec::new()));

    // ...until one refresh later.
    clock.advance(REFRESH);
    parked(&household, &clock, 2).await;
    assert_eq!(names(&sonos).await, Ok(vec!["Kitchen".into()]));

    // Unplugged, it is still known but answers nothing...
    household.remove(&kitchen);
    assert_eq!(names(&sonos).await, Err(Error::Unreachable));
    clock.advance(EXPIRY - Duration::from_secs(1));
    parked(&household, &clock, 3).await;
    assert_eq!(names(&sonos).await, Err(Error::Unreachable));

    // ...until it has not answered a search for the expiry time.
    clock.advance(Duration::from_secs(1) + REFRESH);
    parked(&household, &clock, 4).await;
    assert_eq!(names(&sonos).await, Ok(Vec::new()));
}

/// Waits until search number `count` reached `household` and is over: once it has recorded
/// what it found, the search loop creates its next sleep on `clock`, the only sleep on it.
async fn parked(household: &Household, clock: &ManualClock, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while household.searches() < count || clock.sleepers() != 1 {
        assert!(
            Instant::now() < deadline,
            "{} searches, {} sleeps on the clock",
            household.searches(),
            clock.sleepers()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!((household.searches(), clock.sleepers()), (count, 1));
}
