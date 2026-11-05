//! Monotonic time behind one seam, shared by the Bridge and `sonos-fake` (PRD "Testing
//! Decisions"): [`SystemClock`] in production, [`ManualClock`] in tests, whose time moves only
//! when the test advances it, so a refresh interval or a playback position minutes away is
//! reached in an instant.
//!
//! Code that waits for time to pass takes an `Arc<dyn Clock>` and never calls
//! `tokio::time::sleep` or `Instant::now` for that itself. Timeouts on network I/O are not time
//! passing in the household and stay on the runtime's timer.

use std::fmt::Debug;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;

/// A wait on a [`Clock`]; it borrows nothing, so it can move into a task.
pub type Sleep = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// A source of monotonic time.
pub trait Clock: Debug + Send + Sync + 'static {
    /// The current point in time; never earlier than a previous answer.
    fn now(&self) -> Instant;

    /// Completes once [`Clock::now`] has reached `deadline`; at once if it has already.
    fn sleep_until(&self, deadline: Instant) -> Sleep;

    /// Completes once `duration` has passed on this clock.
    fn sleep(&self, duration: Duration) -> Sleep {
        self.sleep_until(self.now() + duration)
    }
}

/// The operating system's monotonic clock, waited on with the Tokio timer.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep_until(&self, deadline: Instant) -> Sleep {
        Box::pin(tokio::time::sleep_until(deadline.into()))
    }
}

/// A clock that stands still until [`ManualClock::advance`] moves it, waking every wait that
/// is then due. Clones share one time.
#[derive(Debug, Clone)]
pub struct ManualClock {
    now: Arc<watch::Sender<Instant>>,
}

impl ManualClock {
    pub fn new() -> Self {
        Self {
            now: Arc::new(watch::Sender::new(Instant::now())),
        }
    }

    /// Moves time forward by `duration`.
    pub fn advance(&self, duration: Duration) {
        self.now.send_modify(|now| *now += duration);
    }

    /// How many sleeps on this clock exist: created by [`Clock::sleep`] or
    /// [`Clock::sleep_until`] and not yet dropped, whether or not they are due or awaited. A
    /// test can wait for a task to park on the clock (create its next sleep) before advancing
    /// it.
    pub fn sleepers(&self) -> usize {
        self.now.receiver_count()
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        *self.now.borrow()
    }

    fn sleep_until(&self, deadline: Instant) -> Sleep {
        let mut now = self.now.subscribe();
        Box::pin(async move {
            if now.wait_for(|now| *now >= deadline).await.is_err() {
                // Every handle to the clock is gone, so its time never moves again.
                std::future::pending::<()>().await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Long enough for a due wait to complete on a test runtime.
    const SETTLE: Duration = Duration::from_millis(50);

    async fn completes(sleep: &mut Sleep) -> bool {
        tokio::time::timeout(SETTLE, sleep).await.is_ok()
    }

    #[tokio::test]
    async fn manual_time_moves_only_when_advanced() {
        let clock = ManualClock::new();
        let before = clock.now();
        tokio::time::sleep(SETTLE).await;
        assert_eq!(clock.now(), before);

        clock.advance(Duration::from_secs(90));
        assert_eq!(clock.now(), before + Duration::from_secs(90));
        assert_eq!(clock.clone().now(), clock.now());
    }

    #[tokio::test]
    async fn a_manual_sleep_completes_once_its_deadline_is_reached() {
        let clock = ManualClock::new();
        let mut short = clock.sleep(Duration::from_secs(30));
        let mut long = clock.sleep(Duration::from_secs(60));
        assert!(!completes(&mut short).await);

        clock.advance(Duration::from_secs(29));
        assert!(!completes(&mut short).await);

        // A clone advances the same time.
        clock.clone().advance(Duration::from_secs(1));
        assert!(completes(&mut short).await);
        assert!(!completes(&mut long).await);

        clock.advance(Duration::from_secs(60));
        assert!(completes(&mut long).await);
    }

    #[tokio::test]
    async fn a_manual_sleep_that_is_already_due_completes_at_once() {
        let clock = ManualClock::new();
        assert!(completes(&mut clock.sleep(Duration::ZERO)).await);
        assert!(completes(&mut clock.sleep_until(clock.now())).await);
    }

    #[tokio::test]
    async fn a_manual_sleep_wakes_a_task_waiting_on_it() {
        let clock = ManualClock::new();
        assert_eq!(clock.sleepers(), 0);
        let sleep = clock.sleep(Duration::from_secs(5));
        let waiting = tokio::spawn(sleep);
        tokio::time::sleep(SETTLE).await;
        assert!(!waiting.is_finished());
        assert_eq!(clock.sleepers(), 1);

        clock.advance(Duration::from_secs(5));
        tokio::time::timeout(SETTLE, waiting)
            .await
            .expect("woken")
            .expect("task");
        assert_eq!(clock.sleepers(), 0);
    }

    #[tokio::test]
    async fn system_sleep_waits_real_time() {
        let clock = SystemClock;
        let start = clock.now();
        clock.sleep(Duration::from_millis(20)).await;
        assert!(clock.now() >= start + Duration::from_millis(20));
    }
}
