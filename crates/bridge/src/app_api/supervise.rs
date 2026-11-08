//! Supervision of active Handoffs between App API calls: while a Handoff's Group plays its
//! stream, its Progress is written for the starter about every [`CADENCE`] on the Bridge's
//! clock; when the Group stops at the end of the Item, Progress at the end is written Finished
//! and the Handoff ends.
//!
//! One supervisor runs per active Handoff, from its start until it ends for any reason: played
//! to its end, replaced by a start on its Group ([`Supervisors::start`] replaces it,
//! [`Supervisors::stop`] stops it), its key revoked, or ended otherwise (a supervisor that
//! finds its Handoff gone from the store stops). Supervision is in memory; a restarted Bridge
//! doesn't resume it. A supervisor that panics is logged and forgotten; its Handoff stays,
//! unsupervised.
//!
//! End-of-Item detection assumes a Group stops (`STOPPED`) on the Handoff's stream at the end
//! of a single URL, as `sonos-fake` models it. It never relies on the position a stopped Group
//! tells (real hardware may tell 0 then; unverified): from about [`END_SLACK`] before the
//! expected end the Group is read every [`END_RECHECK`], and a Group found stopped counts as
//! played to its end only when it was last seen playing within [`END_SLACK`] of the end,
//! moved on by at most [`END_RECHECK`] since. A Group stopped earlier gets nothing written and
//! keeps its Handoff.
//!
//! The Finished write at the end is tried at most [`FINISH_TRIES`] times, again at the next
//! cadence tick after one that didn't land (failed, unconfirmed, or the key's user not known),
//! while the Handoff stays on the stopped Group; after the last failure the Handoff ends all
//! the same (logged as an error, the Finished lost). A key revoked meanwhile ends it at once.
//! A Handoff whose ending fails in storage is ended again at the next tick.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use abs::ProgressUpdate;
use api::{Playback, TransportState};
use clock::Clock;
use store::{GroupHandoff, Store, StreamId};
use tokio::sync::watch;

use super::{
    Acting, Budget, CONTROL_TURN_TIME, Controls, Turns, Wanted, Wrote, plays_stream, told_length,
};

/// How often a playing Handoff's Progress is written, on the Bridge's clock; also how far
/// apart the Finished write at the end is tried again.
const CADENCE: Duration = Duration::from_secs(30);

/// How long the starter's Library Server may take to tell whom the Handoff's key acts for,
/// before a supervisor takes the Group's turn.
const CHECK_TIME: Duration = Duration::from_secs(3);

/// How long the Group may take to tell what it plays, holding its turn.
const GROUP_TIME: Duration = Duration::from_secs(1);

/// How long a Progress write may take, holding the Group's turn. With [`GROUP_TIME`], a
/// supervisor holds the turn at most 2.5 seconds (plus local storage), under the
/// [`CONTROL_TURN_TIME`] a control waits for it.
const WRITE_TIME: Duration = Duration::from_millis(1500);

/// How far before the expected end of the Item the Group is read every [`END_RECHECK`]; and
/// how far short of the end a Group last seen playing (moved on by at most [`END_RECHECK`])
/// may have been and still count as played to its end once found stopped.
const END_SLACK: Duration = Duration::from_secs(5);

/// The least time between two visits to a Group: how often it is read near the end of the
/// Item, and how soon a visit that learned nothing is made again.
const END_RECHECK: Duration = Duration::from_secs(1);

/// How often the Finished write at the end of the Item is tried before the Handoff ends
/// without it.
const FINISH_TRIES: u8 = 3;

/// What a supervisor last learned of its Group, on the Bridge's clock: when (`at`), and, if
/// it was playing the Handoff's stream on from a known position, that position then.
#[derive(Debug, Clone, Copy)]
struct Seen {
    at: Instant,
    playing: Option<Duration>,
}

/// The supervisors running, one per Group with an active Handoff, and what they act with.
pub(super) struct Supervisors<S> {
    running: Mutex<HashMap<String, Running>>,
    controls: Controls<S>,
    turns: Arc<Turns>,
    clock: Arc<dyn Clock>,
}

impl<S> std::fmt::Debug for Supervisors<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisors")
            .field("running", &self.running)
            .field("clock", &self.clock)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct Running {
    stream: StreamId,
    /// The Item's length, as the supervisor knows it.
    length: Option<f64>,
    /// Tells the supervisor what a control saw; dropping it stops the supervisor.
    seen: watch::Sender<Seen>,
    task: tokio::task::AbortHandle,
}

impl<S: Store> Supervisors<S> {
    /// None running yet; they act with `controls`, take `turns` and are timed on `clock`.
    pub(super) fn new(controls: Controls<S>, turns: Arc<Turns>, clock: Arc<dyn Clock>) -> Self {
        Self {
            running: Mutex::default(),
            controls,
            turns,
            clock,
        }
    }

    /// Starts supervising `stream`'s Handoff on Group `group`, of an Item `length` seconds
    /// long (if known), which the Group just started playing at `position`; stops the Group's
    /// previous supervisor.
    pub(super) fn start(
        self: &Arc<Self>,
        group: String,
        stream: StreamId,
        length: Option<f64>,
        position: Duration,
    ) {
        if length.is_none() {
            tracing::info!(
                group,
                "a Handoff's Item has no known length: it is supervised, but never ends at the end"
            );
        }
        let seen = Seen {
            at: self.clock.now(),
            playing: Some(position),
        };
        let (sender, receiver) = watch::channel(seen);
        let supervisor = Supervisor {
            controls: self.controls.clone(),
            turns: Arc::clone(&self.turns),
            clock: Arc::clone(&self.clock),
            group: group.clone(),
            stream,
            length,
        };
        let forget = Forget {
            supervisors: Arc::clone(self),
            group: group.clone(),
            stream,
        };
        // Held until the entry is in, so a supervisor ending at once forgets it after.
        let mut running = self.lock();
        let task = tokio::spawn(async move {
            let _forget = forget;
            supervisor.run(receiver).await;
        });
        let started = Running {
            stream,
            length,
            seen: sender,
            task: task.abort_handle(),
        };
        if let Some(previous) = running.insert(group, started) {
            previous.task.abort();
        }
    }

    /// Stops the supervisor of Group `group`'s Handoff, if any.
    pub(super) fn stop(&self, group: &str) {
        let stopped = self.lock().remove(group);
        if let Some(stopped) = stopped {
            stopped.task.abort();
        }
    }

    /// The length of the Item of `stream`'s Handoff on Group `group`, as its supervisor knows
    /// it; none without a supervisor or a known length.
    pub(super) fn length(&self, group: &str, stream: &StreamId) -> Option<f64> {
        let running = self.lock();
        let running = running
            .get(group)
            .filter(|running| running.stream == *stream)?;
        running.length
    }

    /// Tells the supervisor of Group `group`'s Handoff, if any, how a control just left the
    /// Group (`playback`): playing on from a position, or not (also when playing without
    /// telling where).
    pub(super) fn seen(&self, group: &str, playback: &Playback) {
        let playing = playback
            .position
            .filter(|_| playback.state == TransportState::Playing)
            .and_then(|position| Duration::try_from_secs_f64(position).ok());
        if let Some(running) = self.lock().get(group) {
            running.seen.send_replace(Seen {
                at: self.clock.now(),
                playing,
            });
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

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Running>> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Forgets a supervisor once its task is done, however: returned, aborted or panicked (which
/// it logs).
struct Forget<S: Store> {
    supervisors: Arc<Supervisors<S>>,
    group: String,
    stream: StreamId,
}

impl<S: Store> Drop for Forget<S> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            tracing::error!(
                group = self.group,
                "a Handoff's supervisor panicked: the Handoff stays, unsupervised"
            );
        }
        self.supervisors.ended(&self.group, &self.stream);
    }
}

/// What one Handoff's supervisor acts with.
struct Supervisor<S> {
    controls: Controls<S>,
    turns: Arc<Turns>,
    clock: Arc<dyn Clock>,
    group: String,
    stream: StreamId,
    /// The Item's length in seconds, as its Library Server told it at the start, if known.
    length: Option<f64>,
}

/// What a supervisor does after a visit to its Group.
enum Next {
    Continue,
    /// The Group played to the end of the Item, but the Handoff isn't ended yet.
    Ending(Ending),
    /// The Handoff ended.
    Stop,
}

/// What is left to do for a Handoff played to the end of its Item.
#[derive(Debug, Clone, Copy)]
enum Ending {
    /// Writing Progress at the end (`length`), Finished, for the `attempt`th time.
    Finish { length: f64, attempt: u8 },
    /// Ending the Handoff in the store, after a storage failure.
    Store,
}

/// Why the supervised Handoff wasn't found active.
enum NotFound {
    /// Ended, or another one on the Group.
    Gone,
    /// Storage failed (logged).
    Unknown,
}

impl<S: Store> Supervisor<S> {
    /// Supervises until the Handoff ends, or `seen` is dropped.
    async fn run(&self, mut seen: watch::Receiver<Seen>) {
        let mut last = *seen.borrow_and_update();
        let mut tick = last.at + CADENCE;
        let mut not_before = last.at;
        let mut ending = None;
        loop {
            let wake = match ending {
                Some(_) => tick,
                None => self
                    .end_due(&last, not_before)
                    .map_or(tick, |end| end.min(tick)),
            };
            tokio::select! {
                () = self.clock.sleep_until(wake.max(not_before)) => {}
                changed = seen.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    // A control's sighting may be older than the visit's after it.
                    let told = *seen.borrow_and_update();
                    if told.at >= last.at {
                        last = told;
                    }
                    continue;
                }
            }
            let now = self.clock.now();
            let cadence = now >= tick;
            while tick <= now {
                tick += CADENCE;
            }
            // Spaced from when this visit began, so a visit that learns nothing never spins.
            not_before = now + END_RECHECK;
            let next = match ending.take() {
                Some(pending) => self.end_again(pending).await,
                None => self.visit(&mut last, cadence).await,
            };
            match next {
                Next::Continue => {}
                Next::Ending(pending) => ending = Some(pending),
                Next::Stop => return,
            }
        }
    }

    /// When the Group, `last` seen as given, is next read for the end of the Item, not before
    /// `after`: from [`END_SLACK`] before it is expected there. None unless it was seen playing
    /// and the length is known; nor once it is seen past the end by more than [`END_SLACK`]
    /// (the length was wrong), or a read would come more than a [`CADENCE`] after the expected
    /// end (the Group can't be read): the cadence's visits go on then.
    fn end_due(&self, last: &Seen, after: Instant) -> Option<Instant> {
        let position = last.playing?;
        let length = Duration::try_from_secs_f64(self.length?).ok()?;
        if position > length + END_SLACK {
            return None;
        }
        let end = last.at.checked_add(length.saturating_sub(position))?;
        let from = end.checked_sub(END_SLACK).unwrap_or(last.at).max(last.at);
        let due = from.max(after);
        (due <= end.checked_add(CADENCE)?).then_some(due)
    }

    /// Reads what the Group plays, holding its turn, and acts on it: on a `cadence` visit, a
    /// Group playing the Handoff's stream gets its position written as Progress; a Group that
    /// stopped there, last seen playing within [`END_SLACK`] of the end of the Item (moved on
    /// by at most [`END_RECHECK`] since), gets Progress at the end written Finished, and the
    /// Handoff ends. A Group paused, stopped elsewhere, or playing something else gets nothing
    /// written. Updates `last`, unless the visit learned nothing.
    async fn visit(&self, last: &mut Seen, cadence: bool) -> Next {
        // Checked before taking the turn, as the Library Server may be slow; only when a write
        // may follow, as only a Group seen playing gets one.
        let checked = if last.playing.is_some() {
            let found = match self.handoff().await {
                Ok(found) => found,
                Err(NotFound::Gone) => return Next::Stop,
                Err(NotFound::Unknown) => return Next::Continue,
            };
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
            Ok(found) => found,
            Err(NotFound::Gone) => return Next::Stop,
            Err(NotFound::Unknown) => return Next::Continue,
        };
        // Where the Group is when asked.
        let at = self.clock.now();
        let Ok(playback) = self
            .controls
            .playback(&self.group, &mut Budget(GROUP_TIME))
            .await
        else {
            return Next::Continue;
        };
        if !plays_stream(&playback, &self.stream) {
            tracing::debug!(
                group = self.group,
                "a supervised Group plays something other than its Handoff"
            );
            *last = Seen { at, playing: None };
            return Next::Continue;
        }
        let length = self.length.or_else(|| told_length(&playback));
        match playback.state {
            sonos::TransportState::Playing => {
                let Some(position) = playback.position else {
                    return Next::Continue;
                };
                *last = Seen {
                    at,
                    playing: Some(position),
                };
                // Without a check, seen not playing before: the key is checked before the next.
                if let (true, Some(acting)) = (cadence, checked) {
                    let update = ProgressUpdate::at(position.as_secs_f64(), length);
                    if let Wrote::Revoked = self.write(&found, acting, update, "cadence").await {
                        return Next::Stop;
                    }
                }
            }
            sonos::TransportState::Paused => *last = Seen { at, playing: None },
            sonos::TransportState::Stopped => {
                let played_to_end = last.playing.zip(length).is_some_and(|(position, length)| {
                    let since = at.saturating_duration_since(last.at).min(END_RECHECK);
                    (position + since + END_SLACK).as_secs_f64() >= length
                });
                *last = Seen { at, playing: None };
                // Checked whenever seen playing before.
                if let (true, Some(acting), Some(length)) = (played_to_end, checked, length) {
                    return self.finish(&found, acting, length, 1).await;
                }
            }
            sonos::TransportState::Transitioning | sonos::TransportState::Unknown => {}
        }
        Next::Continue
    }

    /// Does what is `pending` for a Handoff played to its end, if it is still active: the
    /// Finished write, checking the key first, holding the Group's turn; or ending it.
    async fn end_again(&self, pending: Ending) -> Next {
        let found = match self.handoff().await {
            Ok(found) => found,
            Err(NotFound::Gone) => return Next::Stop,
            Err(NotFound::Unknown) => return Next::Ending(pending),
        };
        let Ending::Finish { length, attempt } = pending else {
            return self.end(&found).await;
        };
        let mut time = Budget(CHECK_TIME);
        let checked = self
            .controls
            .check(&found.active, Wanted::No, &mut time)
            .await;
        if let Acting::Revoked = checked.acting {
            return Next::Stop;
        }
        let turn = self.turns.of(&self.group);
        let not_done = "a Finished write not tried again";
        let Ok(_turn) = turn.take(&self.group, CONTROL_TURN_TIME, not_done).await else {
            return Next::Ending(pending);
        };
        let found = match self.handoff().await {
            Ok(found) => found,
            Err(NotFound::Gone) => return Next::Stop,
            Err(NotFound::Unknown) => return Next::Ending(pending),
        };
        self.finish(&found, checked.acting, length, attempt).await
    }

    /// Writes Progress at the end of the Item (`length`), Finished, for the `attempt`th time,
    /// and ends the Handoff once written; tried again at the next cadence tick when not, up to
    /// [`FINISH_TRIES`] attempts, after which the Handoff ends without it. A revoked key ends
    /// it at once.
    async fn finish(
        &self,
        found: &GroupHandoff,
        acting: Acting<'_>,
        length: f64,
        attempt: u8,
    ) -> Next {
        let update = ProgressUpdate::finished(length);
        match self.write(found, acting, update, "end of Item").await {
            Wrote::Written => self.end(found).await,
            Wrote::Revoked => Next::Stop,
            Wrote::Not if attempt < FINISH_TRIES => Next::Ending(Ending::Finish {
                length,
                attempt: attempt + 1,
            }),
            Wrote::Not => {
                let handoff = &found.active.handoff;
                tracing::error!(
                    group = self.group,
                    user_id = handoff.started_by.user_id,
                    library_item = handoff.library_item,
                    episode = handoff.episode,
                    attempts = attempt,
                    "Finished at the end of a Handoff's Item was never written: lost; the Handoff ends without it"
                );
                self.end(found).await
            }
        }
    }

    /// The Group's active Handoff, if it is still the supervised one.
    async fn handoff(&self) -> Result<GroupHandoff, NotFound> {
        match self.controls.store.group_handoff(&self.group).await {
            Ok(Some(found)) if found.stream == self.stream => Ok(found),
            Ok(_) => Err(NotFound::Gone),
            Err(error) => {
                super::log_storage_error("look up a supervised Group's Handoff", &error);
                Err(NotFound::Unknown)
            }
        }
    }

    async fn write(
        &self,
        found: &GroupHandoff,
        acting: Acting<'_>,
        update: ProgressUpdate,
        cause: &'static str,
    ) -> Wrote {
        let mut time = Budget(WRITE_TIME);
        self.controls
            .write_progress(&found.active, acting, update, cause, &mut time)
            .await
    }

    /// Ends `found`, played to its end; tried again later if storage fails.
    async fn end(&self, found: &GroupHandoff) -> Next {
        let handoff = &found.active.handoff;
        match self.controls.store.end_handoff(&found.stream).await {
            Ok(_) => {
                tracing::info!(
                    group = handoff.group,
                    library_item = handoff.library_item,
                    episode = handoff.episode,
                    "Handoff played to the end of its Item, ended"
                );
                Next::Stop
            }
            Err(error) => {
                super::log_storage_error("end a Handoff played to its end", &error);
                Next::Ending(Ending::Store)
            }
        }
    }
}
