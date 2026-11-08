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
//! expected end the Group is read every [`END_RECHECK`], without asking whom the key acts for
//! first unless a write is due, and a Group found stopped counts as played to its end only
//! when the position it was last seen playing at, moved on by the time since (at most
//! [`PLAYED_SINCE_MAX`]) and by [`POSITION_GRANULARITY`], reaches the end ([`played_to_end`]).
//! A Group stopped earlier gets nothing written and keeps its Handoff; so may one played to
//! its end whose reads failed for more than about a second before it: the time since it was
//! last seen playing then can't tell a natural end from a stop. The Finished write for a Group
//! found at its end is made at once, by a visit right after the one that found it, which asks
//! whom the key acts for first. The end is known only with the Item's length: as its Library
//! Server told it at the start, else as the Group first tells it; without either, the Handoff
//! never ends this way.
//!
//! The Finished write at the end is tried at most [`FINISH_TRIES`] times, again at the next
//! cadence tick after one that didn't land (failed, unconfirmed, or the key's user not known),
//! and each time only while the Group is still found stopped on the Handoff's stream, not
//! seen playing there since: found playing or paused there again, the Handoff goes on as
//! before, without it (and so on something else, logged). After the last failure the Handoff
//! ends all the same (logged as an error, the Finished lost). A key revoked meanwhile ends it
//! at once. A Handoff whose ending fails in storage is ended again at the next tick, under the
//! same rule. A start replacing a Handoff whose Finished write is still pending makes that
//! write itself; one replacing a Handoff whose Group stopped on its stream since the
//! supervisor last read it judges by the same rule, on what the supervisor last saw, whether
//! it was played to its end ([`Supervisors::replaced`]).

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

/// How often a playing Handoff's Progress is written, on the Bridge's clock. A failed
/// Finished write at the end is tried again on these ticks too: the first retry at the next
/// tick, anywhere from 1 second to a cadence after the try at the end, later ones a cadence
/// apart.
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

/// How far before the expected end of the Item the Group is read every [`END_RECHECK`]. Only
/// when that starts: a Group found stopped counts as played to its end by
/// [`played_to_end`]'s rule alone.
const END_SLACK: Duration = Duration::from_secs(5);

/// The least time between two visits to a Group: how often it is read near the end of the
/// Item, and how soon a visit that learned nothing is made again.
const END_RECHECK: Duration = Duration::from_secs(1);

/// The most time [`played_to_end`] counts as played between the last sighting of a Group
/// playing and finding it stopped. Near the end, a visit with no write due reads the Group
/// without asking the Library Server first, so two reads are about [`END_RECHECK`] plus one
/// Group read (at most [`GROUP_TIME`]) apart. A longer gap, as after a failed or skipped read
/// near the end, counts only this much: the Group may as well have been stopped early in it.
const PLAYED_SINCE_MAX: Duration = END_RECHECK.saturating_add(GROUP_TIME);

/// How far a Group may have played beyond the position it tells: Sonos tells positions in
/// whole seconds (`H:MM:SS`), so one telling 599 may be at 599.9.
const POSITION_GRANULARITY: Duration = Duration::from_secs(1);

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

/// What a supervisor knows of its Handoff's Item and Group, shared with a start that replaces
/// it.
#[derive(Debug)]
struct Known {
    /// The Item's length in seconds: as its Library Server told it at the start, else as the
    /// Group first told it, if either did.
    length: Option<f64>,
    /// What is left to do for a Handoff played to the end of its Item, if anything.
    ending: Option<Ending>,
    /// What was last seen of the Group: by the start, a visit, or a control
    /// ([`Supervisors::seen`]).
    last: Seen,
}

/// What a start replacing a Handoff learns from its supervisor ([`Supervisors::replaced`]).
#[derive(Debug, Default)]
pub(super) struct Replaced {
    /// The Item's length in seconds, if known.
    pub(super) length: Option<f64>,
    /// The Finished write at the end of the Item, if it is still to be made.
    pub(super) finished: Option<ProgressUpdate>,
    /// What the supervisor last saw of the Group; none without a supervisor.
    last: Option<Seen>,
}

impl Replaced {
    /// Progress at the end of the Item, Finished, if the Group, found stopped on the Handoff's
    /// stream at `at` on the Bridge's clock, played to the end of the Item: the pending
    /// Finished write, else one by what the supervisor last saw of it ([`played_to_end`]),
    /// judged with the Item's length as the supervisor knows it, else as the Group tells it
    /// (`told`), as a visit does.
    pub(super) fn finished_at_stop(
        &self,
        at: Instant,
        told: Option<f64>,
    ) -> Option<ProgressUpdate> {
        self.finished
            .or_else(|| played_to_end(self.last?, at, self.length.or(told)))
    }
}

/// Whether a Group `last` seen playing the Handoff's stream, and found stopped there at `at`,
/// played to the end of the Item, `length` seconds long: Progress at the end, Finished, if so.
/// It counts as played to the end when the position it was last seen playing at, moved on by
/// the time since (at most [`PLAYED_SINCE_MAX`], about how far apart two reads near the end
/// are) and by [`POSITION_GRANULARITY`], reaches the end. So a Group stopped by hand within a
/// read or two of the end may count as played to it, and one that played to its end after its
/// reads failed for longer than [`PLAYED_SINCE_MAX`] may not. The position the stopped Group
/// tells is not used: real hardware may tell 0 then.
fn played_to_end(last: Seen, at: Instant, length: Option<f64>) -> Option<ProgressUpdate> {
    let (position, length) = (last.playing?, length?);
    let played = at.saturating_duration_since(last.at).min(PLAYED_SINCE_MAX);
    let reached = (position + played + POSITION_GRANULARITY).as_secs_f64();
    ProgressUpdate::finished(length).filter(|_| reached >= length)
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

    /// The Bridge's clock, which supervisors are timed on.
    pub(super) fn clock(&self) -> &dyn Clock {
        self.clock.as_ref()
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
            last: seen,
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
    /// it, holding the Group's turn: nothing without a supervisor. A pending ending is set only
    /// under the turn, but a control's playing sighting clears it without the turn (in
    /// `Supervisor::run`), so `finished` may still tell a Finished write the supervisor is
    /// about to drop, on a sighting it hasn't handled yet; the start reads the Group itself
    /// before writing it. What was last seen of the Group includes a control's sighting
    /// ([`Self::seen`]) the supervisor hasn't handled yet.
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
            last: Some(known.last),
        }
    }

    /// Tells the supervisor of Group `group`'s Handoff, if any, how a control just left the
    /// Group (`playback`), holding the Group's turn: playing on from a position, or not (also
    /// when playing without telling where). Taken under the turn, it is what was last seen of
    /// the Group from now on, until a visit reads it again.
    pub(super) fn seen(&self, group: &str, playback: &Playback) {
        let playing = playback
            .position
            .filter(|_| playback.state == TransportState::Playing)
            .and_then(|position| Duration::try_from_secs_f64(position).ok());
        if let Some(running) = self.lock().get(group) {
            let seen = Seen {
                at: self.clock.now(),
                playing,
            };
            lock(&running.known).last = seen;
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
        let started = seen.borrow_and_update().at;
        let mut tick = started + CADENCE;
        let mut not_before = started;
        // The next visit comes at once: the last found a Finished write due without asking
        // whom the key acts for first.
        let mut at_once = false;
        loop {
            let Known { ending, last, .. } = *self.known();
            let wake = match ending {
                _ if at_once => not_before,
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
                    // Already what was last seen ([`Supervisors::seen`]), unless a visit saw
                    // the Group later.
                    let told = *seen.borrow_and_update();
                    if told.playing.is_some() && told.at >= self.known().last.at {
                        self.drop_ending("the Group plays the Handoff again");
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
            if self.visit(cadence).await.is_break() {
                return;
            }
            // Only after the visit that left a first Finished write pending (a failed checked
            // one leaves the second); one that then learns nothing leaves it so, never spinning.
            at_once = ending.is_none()
                && matches!(self.known().ending, Some(Ending::Finish { attempt: 1, .. }));
            if at_once {
                not_before = now;
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
    /// stopped there, having played to the end of the Item ([`played_to_end`]), gets Progress
    /// at the end written Finished, and the Handoff ends; on a visit that didn't check the key
    /// first, that write is left pending for the next visit, which comes at once. One still
    /// stopped there, not seen playing since, gets what is left of that done. A Group paused,
    /// stopped elsewhere, or playing something else gets nothing written; what was left of the
    /// end is dropped once it is seen playing or paused on the Handoff's stream, or on
    /// something else, and kept while it is between states. Updates what was last seen of the
    /// Group, unless the visit learned nothing; breaks once the Handoff ended.
    async fn visit(&self, cadence: bool) -> ControlFlow<()> {
        let group = self.handoff.group.as_str();
        let Known { ending, last, .. } = *self.known();
        // A cadence write is due: logged when it is skipped.
        let due = cadence && last.playing.is_some();
        // Checked before taking the turn, as the Library Server may be slow; only when a write
        // is due: a cadence write, or a Finished write pending. Other visits, every second
        // near the end, read the Group without, so their reads stay about a second apart
        // ([`PLAYED_SINCE_MAX`]).
        let checked = if due || matches!(ending, Some(Ending::Finish { .. })) {
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
            self.saw(at, None);
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
                self.saw(at, Some(position));
                // Not checked, not due: seen playing before only by a control, or not at all.
                if let (true, Some(acting)) = (cadence, checked) {
                    let update = ProgressUpdate::at(position.as_secs_f64(), length);
                    if let Wrote::Revoked = self.write(&found, acting, update, "cadence").await {
                        return Break(());
                    }
                }
            }
            sonos::TransportState::Paused => {
                self.drop_ending("the Group is paused on the Handoff");
                self.saw(at, None);
            }
            sonos::TransportState::Stopped => {
                let previous = self.saw(at, None);
                let Some(position) = previous.playing else {
                    // Only this visit changes what is left of the end: still `ending`.
                    return match (ending, checked) {
                        (Some(Ending::Store), _) => self.end(&found).await,
                        (Some(Ending::Finish { update, attempt }), Some(acting)) => {
                            self.finish(&found, acting, update, attempt).await
                        }
                        _ => Continue(()),
                    };
                };
                // Seen playing since what is left of the end was found, if anything: by a
                // control, while this visit waited for the turn.
                self.drop_ending("the Group played the Handoff again");
                match (played_to_end(previous, at, length), checked) {
                    (Some(update), Some(acting)) => {
                        return self.finish(&found, acting, update, 1).await;
                    }
                    // The key not checked first: the next visit, at once, checks it and
                    // writes Finished, the Group still found stopped there.
                    (Some(update), None) => {
                        self.known().ending = Some(Ending::Finish { update, attempt: 1 });
                    }
                    (None, _) => {
                        let gap = at.saturating_duration_since(previous.at);
                        self.stopped_short(position, gap, length);
                    }
                }
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

    /// Records what a visit saw of the Group at `at`: playing on from `playing`, or not.
    /// Returns what was seen of it before.
    fn saw(&self, at: Instant, playing: Option<Duration>) -> Seen {
        std::mem::replace(&mut self.known().last, Seen { at, playing })
    }

    /// Logs that the Group, last seen playing at `position` `gap` before it was found stopped
    /// on the Handoff's stream, did so without having played to the end of the Item, `length`
    /// seconds long if known.
    fn stopped_short(&self, position: Duration, gap: Duration, length: Option<f64>) {
        let handoff = &self.handoff;
        let (position, gap) = (position.as_secs(), gap.as_secs_f64());
        match length {
            Some(length) => tracing::info!(
                group = handoff.group,
                library_item = handoff.library_item,
                episode = handoff.episode,
                position,
                gap,
                length,
                "a Group stopped short of the end of its Handoff's Item, or was not read often \
                 enough near it to tell: nothing written, the Handoff stays"
            ),
            None => tracing::info!(
                group = handoff.group,
                library_item = handoff.library_item,
                episode = handoff.episode,
                position,
                gap,
                "a Group stopped before the end of its Handoff's Item was known to be reached: \
                 nothing written, the Handoff stays"
            ),
        }
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
            Ending::Store => "ending the Handoff, after Finished was written or its tries ran out",
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
