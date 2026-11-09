//! Supervision of active Handoffs between App API calls: while a Group plays its Handoff's
//! stream, its Progress is written for the starter about every [`CADENCE`] on the Bridge's
//! clock (not on the ticks that fall among the reads near the end of the Item); when the Group
//! stops at the end of the Item, Progress at the end is written Finished and the Handoff ends.
//!
//! One supervisor runs per active Handoff, from its start until it ends for any reason: played
//! to its end, replaced by a start on its Group ([`Supervisors::start`] replaces it,
//! [`Supervisors::stop`] stops it), its key revoked, or ended otherwise (a supervisor that
//! finds its Handoff gone from the store stops). Supervision is in memory; a restarted Bridge
//! doesn't resume it. A supervisor that panics is logged and forgotten; its Handoff stays,
//! unsupervised.
//!
//! Positions are in the whole Item: a Group playing a later one of the Handoff's files is read
//! at that file's start plus its position in it (`ItemFiles::whole`), so reaching the end of a
//! file that isn't the last is not the end of the Item. End-of-Item detection assumes a Group
//! stops (`STOPPED`) on one of the Handoff's files at the end of the last, as `sonos-fake`
//! models it. It never relies on the position, or the file, a stopped Group tells (real
//! hardware may tell 0 then, and its queue may be back at its first file; unverified): from
//! about [`END_SLACK`] before the
//! expected end the Group is read every [`END_RECHECK`], without asking whom the key acts for
//! first unless a Finished write is pending (a cadence tick among those reads, judged at the
//! tick or at the last sighting if that came after it, is one of them, writing nothing; a
//! visit for a tick judged before them that runs late into them still asks, and writes), and a
//! Group found stopped counts as played to its end only when the position it was last seen
//! playing at, moved on by the time since (at most [`PLAYED_SINCE_MAX`]) and by
//! [`POSITION_GRANULARITY`], reaches the end ([`played_to_end`]). A Group stopped earlier gets
//! nothing written and keeps its Handoff; so may one played to its end whose reads failed for
//! more than about two seconds before it: the time since it was last seen playing then can't
//! tell a natural end from a stop. So, rarely, may one whose last cadence visit before those
//! reads ran late into them: having asked first (up to [`CHECK_TIME`]), its read and write
//! hold the turn up to [`GROUP_TIME`] + [`WRITE_TIME`], so the next read may come that long
//! after its read, past [`PLAYED_SINCE_MAX`]. The Finished write for a Group found at its end
//! is made at once: by the visit that found it when that one asked whom the key acts for
//! first (a cadence visit before the reads near the end began), else by a visit right after
//! it, which asks first. The end is known only with the Item's length: as its Library Server
//! told it at the start, else as the Group first tells it; without either, the Handoff never
//! ends this way.
//!
//! The Finished write at the end is tried at most [`FINISH_TRIES`] times, again at the next
//! cadence tick after one that didn't land (failed, unconfirmed, or the key's user not known),
//! and each time only while the Group is still found stopped on the Handoff's stream, not
//! seen playing there since: found playing or paused there again, the Handoff goes on as
//! before, without it (and so on something else, logged). After the last failure the Handoff
//! ends all the same (logged as an error, the Finished lost). A key revoked meanwhile ends it
//! same rule. A control that leaves the Group playing drops what is left of the end at once,
//! under the Group's turn ([`Supervisors::seen`]), and a visit acts only on what is left of it
//! once it holds the turn. A start replacing a Handoff whose Finished write is still pending
//! makes that write itself; one replacing a Handoff whose Group stopped on its stream since
//! the supervisor last read it judges by the same rule, on what the supervisor last saw,
//! whether it was played to its end ([`Supervisors::replaced`]).

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
    Acting, Budget, CONTROL_TURN_TIME, Controls, ItemFiles, Turns, Wanted, Wrote, told_length,
};

/// How often a playing Handoff's Progress is written, on the Bridge's clock. A tick that finds
/// the Group playing without having asked whom the key acts for first (it wasn't seen playing
/// before: resumed outside the App, say) has a visit right after it ask and write, unless the
/// reads near the end of the Item have begun by then. A tick falling among those reads (from
/// [`END_SLACK`] before the end, judged at the tick, or at the last sighting when that came
/// after it) neither asks nor writes, so those reads stay about [`END_RECHECK`] apart:
/// Progress there comes from the Finished write at the end, a control, or the next tick once
/// those reads stop (the Item's length was wrong). A visit for a tick judged before those
/// reads that runs late into them still asks and writes. A failed Finished write at the end
/// is tried again on these ticks too: the first retry at the next tick, at most a cadence
/// after the try at the end and at least [`END_RECHECK`] after the start of the visit that
/// made it (which asked and read before trying, so the retry may follow the try almost at
/// once), later ones a cadence apart.
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
/// playing and finding it stopped. Near the end, a visit with no Finished write pending (one
/// at a cadence tick too) reads the Group without asking the Library Server first, and the
/// time of each sighting is taken just before its read; visits are spaced [`END_RECHECK`]
/// from when each began, so the reads' own time falls within that spacing, and the rest (one
/// more [`GROUP_TIME`]) is slack for the next visit's wait for the Group's turn and for
/// storage. A longer gap, as after a failed or skipped read near the end, counts only this
/// much: the Group may as well have been stopped early in it.
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
    /// What is left to do for a Handoff played to the end of its Item, if anything. Set only
    /// by the supervisor's visits, holding the Group's turn; dropped by them or by a control
    /// that leaves the Group playing ([`Supervisors::seen`]), also holding it.
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
    handoff: Handoff,
    stream: StreamId,
    known: Arc<Mutex<Known>>,
    /// Wakes the supervisor once a control saw the Group; dropping it stops the supervisor.
    seen: watch::Sender<()>,
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
        let (sender, receiver) = watch::channel(());
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
            files: ItemFiles::of(stream, &handoff.files),
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
            handoff: handoff.clone(),
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
    /// it, holding the Group's turn: nothing without a supervisor. What is left of the end is
    /// set and dropped only under the turn, so `finished` is a Finished write still due: the
    /// Group not seen playing on the Handoff's stream since it was found stopped at the end.
    /// What was last seen of the Group includes a control's sighting ([`Self::seen`]).
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
    /// the Group from now on, until a visit reads it again; playing, it drops what was left of
    /// the end of the Item at once (logged), so neither a visit waiting for the turn nor a
    /// start replacing the Handoff acts on it.
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
            let mut known = lock(&running.known);
            known.last = seen;
            if playback.state == TransportState::Playing {
                drop_ending(
                    &mut known,
                    &running.handoff,
                    "a control has the Group play the Handoff",
                );
            }
            drop(known);
            running.seen.send_replace(());
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
    /// Its stream and files, where they lie in the whole Item.
    files: ItemFiles,
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

/// What a visit leaves for the next one, which then comes at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Then {
    /// Nothing: the next visit comes at its time.
    Wait,
    /// The Finished write it found due without asking whom the key acts for first: the next
    /// visit asks, then writes it, the Group still found stopped there.
    Finish,
    /// The cadence write it found due without asking whom the key acts for first (the Group
    /// not seen playing before it): the next visit asks, then writes it as the cadence's, if
    /// it still finds the Group playing there. Not once the reads near the end of the Item
    /// have begun for the Group as last seen: the next visit is one of them then, neither
    /// asking nor writing. Nor if a control paused the Group in between: that visit, not
    /// seeing it playing when it began, neither asks nor writes.
    Write,
}

/// Forgets what was left of the end of `handoff`'s Item in `known`, if anything, as `why`
/// says it no longer is (logged).
fn drop_ending(known: &mut Known, handoff: &Handoff, why: &'static str) {
    let Some(ending) = known.ending.take() else {
        return;
    };
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

impl<S: Store> Supervisor<S> {
    /// Supervises until the Handoff ends, or `seen` is dropped.
    async fn run(&self, mut seen: watch::Receiver<()>) {
        let started = self.known().last.at;
        let mut tick = started + CADENCE;
        let mut not_before = started;
        let mut then = Then::Wait;
        loop {
            let Known { ending, last, .. } = *self.known();
            // The next read for the end of the Item, when that is all there is to do: also for
            // a visit left to make a Finished write that a control dropped, the Group playing.
            let end = match (then, ending) {
                (Then::Wait | Then::Finish, None) => self.end_due(&last, not_before),
                _ => None,
            };
            let wake = match (then, ending) {
                (Then::Finish | Then::Write, _) => not_before,
                (Then::Wait, Some(_)) => tick,
                (Then::Wait, None) => end.map_or(tick, |end| end.min(tick)),
            };
            tokio::select! {
                // A control's sighting first, so a visit on a passed wake acts on it.
                biased;
                changed = seen.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    // What a control saw is already what was last seen ([`Supervisors::seen`]).
                    seen.mark_unchanged();
                    continue;
                }
                () = self.clock.sleep_until(wake.max(not_before)) => {}
            }
            let now = self.clock.now();
            // Whether the reads near the end began by `at`, for the Group as last seen.
            let reading_end_by = |at: Instant| self.end_due(&last, at) == Some(at);
            // A tick among the reads near the end is one of them, as is a revisit for a write
            // once they began: it neither asks whom the key acts for nor writes, so those reads
            // stay about a second apart. Judged at the tick, not at this visit's time, which
            // falls after the tick unless the tick came on a read's time exactly; or at the
            // last sighting, when that came after the tick (a visit that began before the tick
            // read after it, having waited for the Group's turn).
            let (wanted, cadence) = match then {
                Then::Write => (true, !reading_end_by(now)),
                Then::Wait | Then::Finish => {
                    let ticked = now >= tick;
                    let at = tick.max(last.at);
                    (ticked, ticked && !end.is_some_and(|_| reading_end_by(at)))
                }
            };
            if wanted && !cadence {
                tracing::debug!(
                    group = self.handoff.group,
                    user_id = self.handoff.started_by.user_id,
                    library_item = self.handoff.library_item,
                    episode = self.handoff.episode,
                    "a cadence Progress write not made: among the reads near the end of the Item"
                );
            }
            while tick <= now {
                tick += CADENCE;
            }
            // Spaced from when this visit began, so a visit that learns nothing never spins.
            not_before = now + END_RECHECK;
            let Continue(next) = self.visit(cadence).await else {
                return;
            };
            // Only after a visit that found a write due without asking first, never after one
            // that asked: no visit that learns nothing comes at once.
            then = next;
            if then != Then::Wait {
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
    /// Group playing the Handoff's stream gets its position written as Progress; one not seen
    /// playing when the visit began, so the key not asked about first, gets it written by the
    /// next visit, at once ([`Then::Write`]), unless the reads near the end of the Item have
    /// begun for it by then or a control paused it in between. A Group that stopped there,
    /// having played to the end of the Item ([`played_to_end`]), gets Progress at the end
    /// written Finished, and the Handoff ends; on a visit that didn't check the key first,
    /// that write is left pending for the next visit, which comes at once ([`Then::Finish`]).
    /// One still stopped there, not seen playing since, gets what is left of that done, as it
    /// is once the visit holds the turn. A Group paused, stopped elsewhere, or playing
    /// something else gets nothing written; what was left of the end is dropped once it is
    /// seen playing or paused on the Handoff's stream, or on something else, and kept while it
    /// is between states. Updates what was last seen of the Group, unless the visit learned
    /// nothing; breaks once the Handoff ended.
    async fn visit(&self, cadence: bool) -> ControlFlow<(), Then> {
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
                return Continue(Then::Wait);
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
            return Continue(Then::Wait);
        };
        // A start or control may have come first.
        let Some(found) = self.active().await? else {
            self.skipped(due, "storage failed");
            return Continue(Then::Wait);
        };
        // Where the Group is when asked.
        let at = self.clock.now();
        let Ok(playback) = self.controls.playback(group, &mut Budget(GROUP_TIME)).await else {
            self.skipped(due, "the Group did not tell what it plays");
            return Continue(Then::Wait);
        };
        if !self.files.plays(&playback) {
            tracing::debug!(
                group,
                "a supervised Group plays something other than its Handoff"
            );
            self.drop_ending("the Group plays something other than the Handoff");
            self.saw(at, None);
            return Continue(Then::Wait);
        }
        let playback = self.files.whole(playback);
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
                    return Continue(Then::Wait);
                };
                self.saw(at, Some(position));
                match (cadence, checked) {
                    (true, Some(acting)) => {
                        let update = ProgressUpdate::at(position.as_secs_f64(), length);
                        if let Wrote::Revoked = self.write(&found, acting, update, "cadence").await
                        {
                            return Break(());
                        }
                    }
                    // Not seen playing when the visit began (resumed outside the App, say, or
                    // by a control while it waited for the turn), so not checked.
                    (true, None) => return Continue(Then::Write),
                    (false, _) => {}
                }
            }
            sonos::TransportState::Paused => {
                self.drop_ending("the Group is paused on the Handoff");
                self.saw(at, None);
            }
            sonos::TransportState::Stopped => {
                let previous = self.saw(at, None);
                let Some(position) = previous.playing else {
                    // What is left of the end now, holding the turn: a control that left the
                    // Group playing while the visit waited dropped it.
                    let ending = self.known().ending;
                    match (ending, checked) {
                        (Some(Ending::Store), _) => self.end(&found).await?,
                        (Some(Ending::Finish { update, attempt }), Some(acting)) => {
                            self.finish(&found, acting, update, attempt).await?;
                        }
                        _ => {}
                    }
                    return Continue(Then::Wait);
                };
                // Nothing is left of the end here: seen playing since it was found, by a visit
                // or a control, either dropped it.
                match (played_to_end(previous, at, length), checked) {
                    (Some(update), Some(acting)) => {
                        self.finish(&found, acting, update, 1).await?;
                    }
                    (Some(update), None) => {
                        self.known().ending = Some(Ending::Finish { update, attempt: 1 });
                        return Continue(Then::Finish);
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
        Continue(Then::Wait)
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
        drop_ending(&mut self.known(), &self.handoff, why);
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
            Ok(Some(found)) if found.stream == *self.files.stream() => Continue(Some(found)),
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
