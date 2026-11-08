//! Supervision of active Handoffs between App API calls: while a Group plays its Handoff's
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
//! keeps its Handoff. The end is known only with the Item's length: as its Library Server told
//! it at the start, else as the Group first tells it; without either, the Handoff never ends
//! this way.
//!
//! The Finished write at the end is tried at most [`FINISH_TRIES`] times, again at the next
//! cadence tick after one that didn't land (failed, unconfirmed, or the key's user not known),
//! and each time only while the Group is still found stopped on the Handoff's stream: found
//! playing or paused there again, the Handoff goes on as before, without it (and so on
//! something else, logged). After the last failure the Handoff ends all the same (logged as an
//! error, the Finished lost). A key revoked meanwhile ends it at once. A Handoff whose ending
//! fails in storage is ended again at the next tick, under the same rule. A start replacing a
//! Handoff whose Finished write is still pending makes that write itself
//! ([`Supervisors::replaced`]).

use std::collections::HashMap;
use std::ops::ControlFlow::{self, Break, Continue};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use abs::ProgressUpdate;
use api::{Playback, TransportState};
use clock::Clock;
use store::{GroupHandoff, Handoff, Store, StreamId};
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

/// What a supervisor knows of its Handoff's Item, shared with a start that replaces it.
#[derive(Debug, Default)]
struct Known {
    /// The Item's length in seconds: as its Library Server told it at the start, else as the
    /// Group first told it, if either did.
    length: Option<f64>,
    /// What is left to do for a Handoff played to the end of its Item, if anything.
    ending: Option<Ending>,
}

/// What a start replacing a Handoff learns from its supervisor ([`Supervisors::replaced`]).
#[derive(Debug, Default)]
pub(super) struct Replaced {
    /// The Item's length in seconds, if known.
    pub(super) length: Option<f64>,
    /// The Finished write at the end of the Item, if it is still to be made.
    pub(super) finished: Option<ProgressUpdate>,
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
    known: Arc<Mutex<Known>>,
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

    /// What supervisors, controls and starts act with.
    pub(super) fn controls(&self) -> &Controls<S> {
        &self.controls
    }

    /// Starts supervising `handoff`, whose stream is `stream`, of an Item `length` seconds long
    /// (if known), which its Group just started playing at `position`; stops the Group's
    /// previous supervisor.
    pub(super) fn start(
        self: &Arc<Self>,
        handoff: &Handoff,
        stream: StreamId,
        length: Option<f64>,
        position: Duration,
    ) {
        if length.is_none() {
            tracing::info!(
                group = handoff.group,
                library_item = handoff.library_item,
                episode = handoff.episode,
                "a Handoff's Item has no known length: it ends at the end of the Item only once \
                 its Group tells one"
            );
        }
        let seen = Seen {
            at: self.clock.now(),
            playing: Some(position),
        };
        let (sender, receiver) = watch::channel(seen);
        let known = Arc::new(Mutex::new(Known {
            length,
            ending: None,
        }));
        let supervisor = Supervisor {
            controls: self.controls.clone(),
            turns: Arc::clone(&self.turns),
            clock: Arc::clone(&self.clock),
            handoff: handoff.clone(),
            stream,
            known: Arc::clone(&known),
        };
        let forget = Forget {
            supervisors: Arc::clone(self),
            handoff: handoff.clone(),
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
            known,
            seen: sender,
            task: task.abort_handle(),
        };
        if let Some(previous) = running.insert(handoff.group.clone(), started) {
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

    /// What the supervisor of `stream`'s Handoff on Group `group` knows for a start replacing
    /// it, holding the Group's turn: nothing without a supervisor.
    pub(super) fn replaced(&self, group: &str, stream: &StreamId) -> Replaced {
        let running = self.lock();
        let Some(running) = running
            .get(group)
            .filter(|running| running.stream == *stream)
        else {
            return Replaced::default();
        };
        let known = lock(&running.known);
        let finished = match known.ending {
            Some(Ending::Finish { update, .. }) => Some(update),
            Some(Ending::Store) | None => None,
        };
        Replaced {
            length: known.length,
            finished,
        }
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
        lock(&self.running)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Forgets a supervisor once its task is done, however: returned, aborted or panicked (which
/// it logs).
struct Forget<S: Store> {
    supervisors: Arc<Supervisors<S>>,
    handoff: Handoff,
    stream: StreamId,
}

impl<S: Store> Drop for Forget<S> {
    fn drop(&mut self) {
        let handoff = &self.handoff;
        if std::thread::panicking() {
            tracing::error!(
                group = handoff.group,
                user_id = handoff.started_by.user_id,
                library_item = handoff.library_item,
                episode = handoff.episode,
                "a Handoff's supervisor panicked: the Handoff stays, unsupervised"
            );
        }
        self.supervisors.ended(&handoff.group, &self.stream);
    }
}

/// What one Handoff's supervisor acts with.
struct Supervisor<S> {
    controls: Controls<S>,
    turns: Arc<Turns>,
    clock: Arc<dyn Clock>,
    /// The supervised Handoff, as it started.
    handoff: Handoff,
    stream: StreamId,
    known: Arc<Mutex<Known>>,
}

/// What is left to do for a Handoff played to the end of its Item.
#[derive(Debug, Clone, Copy)]
enum Ending {
    /// Writing `update`, Progress at the end, Finished, for the `attempt`th time.
    Finish { update: ProgressUpdate, attempt: u8 },
    /// Ending the Handoff in the store, after a storage failure.
    Store,
}

impl<S: Store> Supervisor<S> {
    /// Supervises until the Handoff ends, or `seen` is dropped.
    async fn run(&self, mut seen: watch::Receiver<Seen>) {
        let mut last = *seen.borrow_and_update();
        let mut tick = last.at + CADENCE;
        let mut not_before = last.at;
        loop {
            let ending = self.known().ending;
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
                        if told.playing.is_some() {
                            self.drop_ending("the Group plays the Handoff again");
                        }
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
            if self.visit(&mut last, cadence).await.is_break() {
                return;
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
        let length = Duration::try_from_secs_f64(self.known().length?).ok()?;
        if position > length + END_SLACK {
            return None;
        }
        let end = last.at.checked_add(length.saturating_sub(position))?;
        let from = end.checked_sub(END_SLACK).unwrap_or(last.at).max(last.at);
        let due = from.max(after);
        (due <= end.checked_add(CADENCE)?).then_some(due)
    }

    /// Reads what the Group plays, holding its turn, and acts on it. On a `cadence` visit, a
    /// Group playing the Handoff's stream gets its position written as Progress. A Group that
    /// stopped there, last seen playing within [`END_SLACK`] of the end of the Item (moved on
    /// by at most [`END_RECHECK`] since), gets Progress at the end written Finished, and the
    /// Handoff ends; one still stopped there gets what is left of that done. A Group paused,
    /// stopped elsewhere, or playing something else gets nothing written, and what was left of
    /// the end is dropped once it plays or pauses. Updates `last`, unless the visit learned
    /// nothing; breaks once the Handoff ended.
    async fn visit(&self, last: &mut Seen, cadence: bool) -> ControlFlow<()> {
        let group = self.handoff.group.as_str();
        let ending = self.known().ending;
        // A cadence write is due: logged when it is skipped.
        let due = cadence && last.playing.is_some();
        // Checked before taking the turn, as the Library Server may be slow; only when a write
        // may follow: a Group seen playing, or a Finished write pending.
        let checked = if last.playing.is_some() || matches!(ending, Some(Ending::Finish { .. })) {
            let Some(found) = self.active().await? else {
                self.skipped(due, "storage failed");
                return Continue(());
            };
            let mut time = Budget(CHECK_TIME);
            let checked = self
                .controls
                .check(&found.active, Wanted::No, &mut time)
                .await;
            if let Acting::Revoked = checked.acting {
                return Break(());
            }
            Some(checked.acting)
        } else {
            None
        };
        let turn = self.turns.of(group);
        let not_done = "a supervision visit not made";
        let Ok(_turn) = turn.take(group, CONTROL_TURN_TIME, not_done).await else {
            self.skipped(due, "the Group was busy");
            return Continue(());
        };
        // A start or control may have come first.
        let Some(found) = self.active().await? else {
            self.skipped(due, "storage failed");
            return Continue(());
        };
        // Where the Group is when asked.
        let at = self.clock.now();
        let Ok(playback) = self.controls.playback(group, &mut Budget(GROUP_TIME)).await else {
            self.skipped(due, "the Group did not tell what it plays");
            return Continue(());
        };
        if !plays_stream(&playback, &self.stream) {
            tracing::debug!(
                group,
                "a supervised Group plays something other than its Handoff"
            );
            self.drop_ending("the Group plays something other than the Handoff");
            *last = Seen { at, playing: None };
            return Continue(());
        }
        let length = {
            let mut known = self.known();
            known.length = known.length.or_else(|| told_length(&playback));
            known.length
        };
        match playback.state {
            sonos::TransportState::Playing => {
                self.drop_ending("the Group plays the Handoff again");
                let Some(position) = playback.position else {
                    self.skipped(due, "the Group plays without telling where");
                    return Continue(());
                };
                *last = Seen {
                    at,
                    playing: Some(position),
                };
                // Without a check, seen not playing before: the key is checked before the next.
                if let (true, Some(acting)) = (cadence, checked) {
                    let update = ProgressUpdate::at(position.as_secs_f64(), length);
                    if let Wrote::Revoked = self.write(&found, acting, update, "cadence").await {
                        return Break(());
                    }
                }
            }
            sonos::TransportState::Paused => {
                self.drop_ending("the Group is paused on the Handoff");
                *last = Seen { at, playing: None };
            }
            sonos::TransportState::Stopped => {
                let previous = *last;
                *last = Seen { at, playing: None };
                match (ending, checked) {
                    (Some(Ending::Store), _) => return self.end(&found).await,
                    (Some(Ending::Finish { update, attempt }), Some(acting)) => {
                        return self.finish(&found, acting, update, attempt).await;
                    }
                    _ => {}
                }
                let Some(position) = previous.playing else {
                    return Continue(());
                };
                let since = at.saturating_duration_since(previous.at).min(END_RECHECK);
                let finished = length
                    .filter(|length| (position + since + END_SLACK).as_secs_f64() >= *length)
                    .and_then(ProgressUpdate::finished);
                // Checked whenever seen playing before.
                if let (Some(update), Some(acting)) = (finished, checked) {
                    return self.finish(&found, acting, update, 1).await;
                }
                tracing::info!(
                    group,
                    library_item = self.handoff.library_item,
                    episode = self.handoff.episode,
                    position = position.as_secs(),
                    "a Group stopped short of the end of its Handoff's Item: nothing written, the \
                     Handoff stays"
                );
            }
            sonos::TransportState::Transitioning | sonos::TransportState::Unknown => {
                if due {
                    tracing::info!(
                        group,
                        user_id = self.handoff.started_by.user_id,
                        library_item = self.handoff.library_item,
                        episode = self.handoff.episode,
                        state = ?playback.state,
                        "a cadence Progress write skipped: the Group is between states"
                    );
                }
            }
        }
        Continue(())
    }

    /// Writes `update`, Progress at the end of the Item, Finished, for the `attempt`th time,
    /// holding the Group's turn, and ends the Handoff once written; tried again at the next
    /// cadence tick when not, up to [`FINISH_TRIES`] attempts, after which the Handoff ends
    /// without it. A revoked key ends it at once.
    async fn finish(
        &self,
        found: &GroupHandoff,
        acting: Acting<'_>,
        update: ProgressUpdate,
        attempt: u8,
    ) -> ControlFlow<()> {
        match self.write(found, acting, update, "end of Item").await {
            Wrote::Written => self.end(found).await,
            Wrote::Revoked => Break(()),
            Wrote::Not if attempt < FINISH_TRIES => {
                self.known().ending = Some(Ending::Finish {
                    update,
                    attempt: attempt + 1,
                });
                Continue(())
            }
            Wrote::Not => {
                let handoff = &found.active.handoff;
                tracing::error!(
                    group = handoff.group,
                    user_id = handoff.started_by.user_id,
                    library_item = handoff.library_item,
                    episode = handoff.episode,
                    position = update.current_time(),
                    finished = update.is_finished(),
                    attempts = attempt,
                    "Finished at the end of a Handoff's Item was never written: lost; the Handoff ends without it"
                );
                self.end(found).await
            }
        }
    }

    /// Ends `found`, played to its end, holding the Group's turn; tried again later if storage
    /// fails.
    async fn end(&self, found: &GroupHandoff) -> ControlFlow<()> {
        let handoff = &found.active.handoff;
        match self.controls.store.end_handoff(&found.stream).await {
            Ok(_) => {
                tracing::info!(
                    group = handoff.group,
                    library_item = handoff.library_item,
                    episode = handoff.episode,
                    "Handoff played to the end of its Item, ended"
                );
                Break(())
            }
            Err(error) => {
                self.storage_failed("end a Handoff played to its end", &error);
                self.known().ending = Some(Ending::Store);
                Continue(())
            }
        }
    }

    /// Forgets what was left of the end of the Item, if anything, as `why` says it no longer
    /// is (logged).
    fn drop_ending(&self, why: &'static str) {
        let Some(ending) = self.known().ending.take() else {
            return;
        };
        let handoff = &self.handoff;
        let pending = match ending {
            Ending::Finish { .. } => "the Finished write, not made",
            Ending::Store => "ending the Handoff, after Finished was written",
        };
        tracing::info!(
            group = handoff.group,
            user_id = handoff.started_by.user_id,
            library_item = handoff.library_item,
            episode = handoff.episode,
            why,
            pending,
            "a Handoff played to the end of its Item goes on"
        );
    }

    /// Logs a cadence write skipped for `reason`, if one was `due`.
    fn skipped(&self, due: bool, reason: &'static str) {
        if due {
            let handoff = &self.handoff;
            tracing::warn!(
                group = handoff.group,
                user_id = handoff.started_by.user_id,
                library_item = handoff.library_item,
                episode = handoff.episode,
                reason,
                "a cadence Progress write skipped"
            );
        }
    }

    /// Logs a storage failure during `operation`.
    fn storage_failed(&self, operation: &'static str, error: &store::Error) {
        let handoff = &self.handoff;
        tracing::error!(
            operation,
            group = handoff.group,
            library_item = handoff.library_item,
            episode = handoff.episode,
            error = crate::error_chain(error),
            "storage failed"
        );
    }

    /// The Group's active Handoff, if it is still the supervised one; none when storage
    /// failed (logged). Breaks once the Handoff is gone: ended, or another on the Group.
    async fn active(&self) -> ControlFlow<(), Option<GroupHandoff>> {
        match self.controls.store.group_handoff(&self.handoff.group).await {
            Ok(Some(found)) if found.stream == self.stream => Continue(Some(found)),
            Ok(_) => Break(()),
            Err(error) => {
                self.storage_failed("look up a supervised Group's Handoff", &error);
                Continue(None)
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

    fn known(&self) -> MutexGuard<'_, Known> {
        lock(&self.known)
    }
}
