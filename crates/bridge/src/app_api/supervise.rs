//! Supervision of active Handoffs between App API calls: while a Handoff's Group plays its
//! stream, its Progress is written for the starter about every [`CADENCE`] on the Bridge's
//! clock; when the Group stops at the end of the Item, Progress at the end is written Finished
//! and the Handoff ends.
//!
//! One supervisor runs per active Handoff, from its start until it ends for any reason: played
//! to its end, replaced by a start on its Group ([`Supervisors::start`] replaces it,
//! [`Supervisors::stop`] stops it), its key revoked, or ended otherwise (a supervisor that
//! finds its Handoff gone from the store stops). Supervision is in memory; a restarted Bridge
//! doesn't resume it.
//!
//! End-of-Item detection assumes a Group stops (`STOPPED`) on the Handoff's stream at the end
//! of a single URL, as `sonos-fake` models it; it relies on where the Group was last seen
//! playing and the clock since, never on the position a stopped Group tells (real hardware
//! may tell 0 then; unverified).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use abs::ProgressUpdate;
use clock::Clock;
use store::{GroupHandoff, Store, StreamId};
use tokio::sync::watch;

use super::{Acting, Budget, CONTROL_TURN_TIME, Controls, Turns, Wanted, known_length};
use crate::stream;

/// How often a playing Handoff's Progress is written, on the Bridge's clock.
pub(super) const CADENCE: Duration = Duration::from_secs(30);

/// How long the starter's Library Server may take to tell whom the Handoff's key acts for,
/// before a supervisor takes the Group's turn.
const CHECK_TIME: Duration = Duration::from_secs(3);

/// How long the Group may take to tell what it plays, holding its turn.
const GROUP_TIME: Duration = Duration::from_secs(1);

/// How long a Progress write may take, holding the Group's turn. With [`GROUP_TIME`], a
/// supervisor holds the turn at most 2.5 seconds (plus local storage), under the
/// [`CONTROL_TURN_TIME`] a control waits for it.
const WRITE_TIME: Duration = Duration::from_millis(1500);

/// How far short of the Item's end a Group found stopped may have been expected and still
/// count as played to its end.
const END_SLACK: Duration = Duration::from_secs(5);

/// The least time until a Group expected at the end of the Item is read again, when it still
/// plays there.
const END_RECHECK: Duration = Duration::from_secs(1);

/// Where a Group was last seen on its Handoff's stream, and whether it was playing on from
/// there, on the Bridge's clock.
#[derive(Debug, Clone, Copy)]
pub(super) struct Seen {
    pub(super) playing: bool,
    pub(super) position: Duration,
    pub(super) at: Instant,
}

impl Seen {
    /// Where the Group is expected at `now`.
    fn expected(&self, now: Instant) -> Duration {
        if self.playing {
            self.position + now.saturating_duration_since(self.at)
        } else {
            self.position
        }
    }
}

/// The supervisors running, one per Group with an active Handoff.
#[derive(Debug, Default)]
pub(super) struct Supervisors {
    running: Mutex<HashMap<String, Running>>,
}

#[derive(Debug)]
struct Running {
    stream: StreamId,
    /// Tells the supervisor what a control saw; dropping it stops the supervisor.
    seen: watch::Sender<Seen>,
    task: tokio::task::AbortHandle,
}

impl Supervisors {
    /// Starts `supervisor`, the Group last `seen` as given, stopping the Group's previous one.
    pub(super) fn start<S: Store>(self: &Arc<Self>, supervisor: Supervisor<S>, seen: Seen) {
        let (sender, receiver) = watch::channel(seen);
        let (group, stream) = (supervisor.group.clone(), supervisor.stream);
        let mut running = self.lock();
        let supervisors = Arc::clone(self);
        let task = tokio::spawn(async move {
            supervisor.run(receiver).await;
            supervisors.ended(&supervisor.group, &supervisor.stream);
        });
        let started = Running {
            stream,
            seen: sender,
            task: task.abort_handle(),
        };
        if let Some(previous) = running.insert(group, started) {
            previous.task.abort();
        }
    }

    /// Stops the supervisor of Group `group`'s Handoff, if any.
    pub(super) fn stop(&self, group: &str) {
        if let Some(stopped) = self.lock().remove(group) {
            stopped.task.abort();
        }
    }

    /// Tells the supervisor of Group `group`'s Handoff, if any, where a control saw the Group.
    pub(super) fn seen(&self, group: &str, seen: Seen) {
        if let Some(running) = self.lock().get(group) {
            running.seen.send_replace(seen);
        }
    }

    /// Forgets the supervisor of `stream` on Group `group`, which ended by itself.
    fn ended(&self, group: &str, stream: &StreamId) {
        let mut running = self.lock();
        if running
            .get(group)
            .is_some_and(|running| running.stream == *stream)
        {
            running.remove(group);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Running>> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// What one Handoff's supervisor acts with.
pub(super) struct Supervisor<S> {
    pub(super) controls: Controls<S>,
    pub(super) turns: Arc<Turns>,
    pub(super) clock: Arc<dyn Clock>,
    pub(super) group: String,
    pub(super) stream: StreamId,
    /// The Item's length in seconds, as its Library Server told it at the start, if known.
    pub(super) length: Option<f64>,
}

/// Whether a supervisor goes on after a visit to its Group.
enum Next {
    Continue,
    /// The Handoff ended.
    Stop,
}

/// The supervised Handoff, as the store tells it.
enum Found {
    Active(GroupHandoff),
    Ended,
    /// Storage failed (logged).
    Unknown,
}

impl<S: Store> Supervisor<S> {
    /// Supervises until the Handoff ends, or `seen` is dropped.
    async fn run(&self, mut seen: watch::Receiver<Seen>) {
        let mut last = *seen.borrow_and_update();
        let mut tick = last.at + CADENCE;
        loop {
            let wake = self.end_due(&last).map_or(tick, |end| end.min(tick));
            tokio::select! {
                () = self.clock.sleep_until(wake) => {}
                changed = seen.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    last = *seen.borrow_and_update();
                    continue;
                }
            }
            let now = self.clock.now();
            let cadence = now >= tick;
            while tick <= now {
                tick += CADENCE;
            }
            if let Next::Stop = self.visit(&mut last, cadence).await {
                return;
            }
        }
    }

    /// When the Group, `last` seen as given, is expected at the end of the Item; none unless
    /// it was playing and the length is known.
    fn end_due(&self, last: &Seen) -> Option<Instant> {
        let length = Duration::try_from_secs_f64(known_length(self.length)?).ok()?;
        if !last.playing {
            return None;
        }
        Some(last.at + length.saturating_sub(last.position).max(END_RECHECK))
    }

    /// Reads what the Group plays, holding its turn, and acts on it: on a `cadence` visit, a
    /// Group playing the Handoff's stream gets its position written as Progress; a Group that
    /// stopped there, having been seen playing on to about the end of the Item, gets Progress
    /// at the end written Finished, and the Handoff ends. A Group paused, stopped elsewhere,
    /// or playing something else gets nothing written. Updates `last`.
    async fn visit(&self, last: &mut Seen, cadence: bool) -> Next {
        let found = match self.handoff().await {
            Found::Active(found) => found,
            Found::Ended => return Next::Stop,
            Found::Unknown => return Next::Continue,
        };
        // Checked before taking the turn, as the Library Server may be slow; only when a write
        // may follow, as only a Group seen playing gets one.
        let checked = if last.playing {
            let mut time = Budget(CHECK_TIME);
            let checked = self
                .controls
                .check(&found.active, Wanted::No, &mut time)
                .await;
            if let Acting::Revoked = checked.acting {
                return Next::Stop;
            }
            Some(checked.acting)
        } else {
            None
        };
        let turn = self.turns.of(&self.group);
        let not_done = "a supervision visit not made";
        let Ok(_turn) = turn.take(&self.group, CONTROL_TURN_TIME, not_done).await else {
            return Next::Continue;
        };
        // A start or control may have come first.
        let found = match self.handoff().await {
            Found::Active(found) => found,
            Found::Ended => return Next::Stop,
            Found::Unknown => return Next::Continue,
        };
        let Some(playback) = self.playback().await else {
            return Next::Continue;
        };
        let played = playback.uri.as_deref().and_then(stream::stream_in_url);
        if played != Some(self.stream) {
            tracing::debug!(
                group = self.group,
                "a supervised Group plays something other than its Handoff"
            );
            last.playing = false;
            return Next::Continue;
        }
        let now = self.clock.now();
        let length = known_length(self.length)
            .or_else(|| known_length(playback.duration.map(|length| length.as_secs_f64())));
        match playback.state {
            sonos::TransportState::Playing => {
                let Some(position) = playback.position else {
                    return Next::Continue;
                };
                *last = Seen {
                    playing: true,
                    position,
                    at: now,
                };
                if !cadence {
                    return Next::Continue;
                }
                let Some(acting) = checked else {
                    // Seen not playing before: the key is checked before the next write.
                    return Next::Continue;
                };
                let update = ProgressUpdate::at(position.as_secs_f64(), length);
                self.write(&found, acting, update, "cadence").await;
            }
            sonos::TransportState::Paused => {
                last.playing = false;
                if let Some(position) = playback.position {
                    last.position = position;
                }
            }
            sonos::TransportState::Stopped => {
                let ended = length.zip(checked).filter(|(length, _)| {
                    let expected = last.expected(now) + END_SLACK;
                    last.playing && expected.as_secs_f64() >= *length
                });
                last.playing = false;
                if let Some((length, acting)) = ended {
                    let update = ProgressUpdate {
                        current_time: length,
                        duration: Some(length),
                        finished: true,
                    };
                    self.write(&found, acting, update, "end of Item").await;
                    self.end(&found).await;
                    return Next::Stop;
                }
            }
            sonos::TransportState::Transitioning | sonos::TransportState::Unknown => {}
        }
        Next::Continue
    }

    /// The Group's active Handoff, if it is still the supervised one.
    async fn handoff(&self) -> Found {
        match self.controls.store.group_handoff(&self.group).await {
            Ok(Some(found)) if found.stream == self.stream => Found::Active(found),
            Ok(_) => Found::Ended,
            Err(error) => {
                super::log_storage_error("look up a supervised Group's Handoff", &error);
                Found::Unknown
            }
        }
    }

    /// What the Group plays, within [`GROUP_TIME`]; none (logged) if it doesn't tell.
    async fn playback(&self) -> Option<sonos::Playback> {
        let group = &self.group;
        match Budget(GROUP_TIME)
            .spend(self.controls.sonos.playback(group))
            .await
        {
            Some(Ok(playback)) => Some(playback),
            Some(Err(error)) => {
                tracing::warn!(group, %error, "cannot read what a supervised Group plays");
                None
            }
            None => {
                tracing::warn!(
                    group,
                    "a supervised Group did not tell its playback in time"
                );
                None
            }
        }
    }

    async fn write(
        &self,
        found: &GroupHandoff,
        acting: Acting<'_>,
        update: ProgressUpdate,
        cause: &'static str,
    ) {
        let mut time = Budget(WRITE_TIME);
        self.controls
            .write_progress(&found.active, acting, update, cause, &mut time)
            .await;
    }

    /// Ends `found`, played to its end.
    async fn end(&self, found: &GroupHandoff) {
        let handoff = &found.active.handoff;
        match self.controls.store.end_handoff(&found.stream).await {
            Ok(_) => tracing::info!(
                group = handoff.group,
                library_item = handoff.library_item,
                episode = handoff.episode,
                "Handoff played to the end of its Item, ended"
            ),
            Err(error) => super::log_storage_error("end a Handoff played to its end", &error),
        }
    }
}
