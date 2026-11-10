//! Local playback: playing an Item on the phone from the Bridge's stream, through a
//! [`LocalPlayer`], and reporting where it is so the Bridge writes Progress (PRD "Progress
//! cadence"): every [`REPORT_EVERY`] while playing, and at once on pause, skip and stop; at the
//! end of the Item, or when the player fails, the Local session ends with its last position.
//!
//! [`LocalPlayback`] runs all of it on one task, one step at a time: the App's buttons, the
//! player's events (lock screen and Control Center, interruptions, the end of the Item), the
//! timer and the Bridge's answers go through the same steps. Each step acts on the player at
//! once; what it tells the Bridge goes to a second task, which sends one call at a time, so a
//! slow Bridge never holds back the player. A report still waiting to be sent gives way to a
//! newer one, or to the end. Every call names the Local session by its stream's path, so one
//! that arrives late can't touch a Local session started since.
//!
//! App logic depends only on the [`LocalPlayer`] trait: the iOS App plugs in AVPlayer
//! (`crate::ios`), tests a fake. Where no player is plugged in (any App built for something
//! other than iOS), Local playback is not offered.
//!
//! A report that fails doesn't stop playback and isn't sent again (buffering it is #24); the
//! Now playing screen says why. One that tells the Local session is gone on the Bridge
//! (`api_key_rejected`, `session_invalid`, `no_local_session`) stops playback here too; so does
//! a player that can't play the stream. Why it stopped is kept ([`LocalPlayback::take_halted`])
//! for the App to tell the person.

use std::collections::VecDeque;
use std::fmt::{self, Debug};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use api::{ApiError, ItemId, Skip};
use clock::Clock;
use tokio::sync::{mpsc, oneshot};

/// How often the App reports its position while playing.
pub const REPORT_EVERY: Duration = Duration::from_secs(30);

/// What a [`LocalPlayer`] loads: an Item's stream with what the lock screen shows of it.
/// Times are seconds. Its `Debug` output leaves out the URLs, which let anyone holding them
/// fetch the Item while its Local session lasts.
#[derive(Clone, PartialEq)]
pub struct Media {
    /// The stream's full URL on the Bridge.
    pub url: String,
    /// Where to start playing.
    pub start_at: f64,
    pub title: String,
    /// An audiobook's author, or an episode's podcast.
    pub artist: Option<String>,
    /// The cover's full URL on the Bridge, if the Item has one.
    pub artwork_url: Option<String>,
    pub duration: Option<f64>,
}

impl Debug for Media {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Media")
            .field("url", &"REDACTED")
            .field("start_at", &self.start_at)
            .field("title", &self.title)
            .field("artist", &self.artist)
            .field(
                "artwork_url",
                &self.artwork_url.as_ref().map(|_| "REDACTED"),
            )
            .field("duration", &self.duration)
            .finish()
    }
}

/// What happened to a [`LocalPlayer`] outside the App's own calls, for App logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerEvent {
    /// Play from the lock screen, Control Center or headphones.
    Play,
    /// Pause from there.
    Pause,
    /// Play/pause toggle from there.
    Toggle,
    /// 15 s back or 30 s forward from there.
    Skip(Skip),
    /// iOS interrupted playback (a phone call, say); the player stopped playing.
    InterruptionBegan,
    /// The interruption ended; `resume` if iOS says playback should go on.
    InterruptionEnded { resume: bool },
    /// The Item played to its end.
    Ended,
    /// The player can't play the stream.
    Failed,
}

/// Where [`LocalPlayer`] events go.
pub type PlayerEvents = mpsc::UnboundedSender<PlayerEvent>;

/// Plays a stream on the phone, and sends what happens to it otherwise as [`PlayerEvent`]s.
/// Only [`LocalPlayer::pause`] waits for the main thread, and only the Local playback task
/// calls it, never the main thread; [`LocalPlayer::position`] is a cached read, also made while
/// screens render.
pub trait LocalPlayer: Debug + Send + Sync + 'static {
    /// Loads `media`, replacing whatever was loaded, and plays it from `media.start_at`.
    fn load(&self, media: &Media);
    fn play(&self);
    /// Pauses, and answers where it paused, in seconds: exactly, as the next play goes on
    /// from there.
    fn pause(&self) -> f64;
    /// Moves to `position` seconds in, playing or paused as it was.
    fn seek(&self, position: f64);
    /// Stops and forgets what was loaded; the lock screen shows nothing of it any more.
    fn unload(&self);
    /// Seconds into what is loaded, as of the last second or so; 0 without anything.
    fn position(&self) -> f64;
}

/// Local playback as the screens show it. Times are seconds. Its `Debug` output leaves out the
/// cover's URL, as [`Media`]'s does.
#[derive(Clone, PartialEq)]
pub struct LocalStatus {
    pub item: ItemId,
    pub title: String,
    /// An audiobook's author, or an episode's podcast.
    pub artist: Option<String>,
    /// The cover's full URL, if the Item has one.
    pub artwork_url: Option<String>,
    pub duration: Option<f64>,
    pub position: f64,
    pub playing: bool,
    /// Why the last report failed, until one succeeds.
    pub report_failed: Option<ApiError>,
}

impl Debug for LocalStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalStatus")
            .field("item", &self.item)
            .field("title", &self.title)
            .field("artist", &self.artist)
            .field(
                "artwork_url",
                &self.artwork_url.as_ref().map(|_| "REDACTED"),
            )
            .field("duration", &self.duration)
            .field("position", &self.position)
            .field("playing", &self.playing)
            .field("report_failed", &self.report_failed)
            .finish()
    }
}

/// Why Local playback stopped on its own, rather than by [`LocalPlayback::stop`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Halted {
    /// The Bridge answered this to a report: the Local session, or the session, is gone. Or,
    /// at the end of the Item or after the player failed, the last report failed with it.
    Api(ApiError),
    /// The player couldn't play the stream.
    PlayerFailed,
}

impl Halted {
    /// Whether it ends the session, which then matters more than anything else.
    fn ends_session(&self) -> bool {
        matches!(
            self,
            Self::Api(ApiError::ApiKeyRejected | ApiError::SessionInvalid)
        )
    }
}

/// Local playback: a handle on the task that runs it. Clones share it; the task ends once
/// every clone is gone.
#[derive(Debug, Clone)]
pub struct LocalPlayback {
    commands: mpsc::UnboundedSender<Command>,
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    player: Arc<dyn LocalPlayer>,
    /// What plays, if anything.
    now: Mutex<Option<Now>>,
    /// Why Local playback last stopped on its own, until the App takes it.
    halted: Mutex<Option<Halted>>,
}

/// What plays, apart from the player's position.
struct Now {
    item: ItemId,
    /// The path of its Local session's stream, which names it to the Bridge.
    stream: String,
    media: Media,
    playing: bool,
    /// Paused by an interruption while playing: it resumes if iOS says so.
    interrupted: bool,
    report_failed: Option<ApiError>,
}

impl Debug for Now {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Now")
            .field("item", &self.item)
            .field("media", &self.media)
            .field("playing", &self.playing)
            .field("interrupted", &self.interrupted)
            .field("report_failed", &self.report_failed)
            .finish_non_exhaustive()
    }
}

/// A step asked of the task, with where it says it's done: once it acted on the player, not
/// once the Bridge answered; a stop once the Bridge answered its end, or, with nothing
/// playing, every call still on its way.
#[derive(Debug)]
enum Command {
    Start(Box<Now>, oneshot::Sender<()>),
    Toggle(oneshot::Sender<()>),
    Skip(Skip, oneshot::Sender<()>),
    Stop(oneshot::Sender<Result<(), ApiError>>),
}

/// A position to tell the Bridge, in the Local session whose stream's path is `stream`.
struct Report {
    stream: String,
    position: f64,
    duration: Option<f64>,
}

impl Debug for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Report")
            .field("position", &self.position)
            .field("duration", &self.duration)
            .finish_non_exhaustive()
    }
}

/// A call for the sender task.
#[derive(Debug)]
enum Outgoing {
    /// Reports where it is.
    Report(Report),
    /// Ends the Local session there; the answer goes to whoever waits for it, if anyone, else
    /// back to the task.
    End(Report, Option<oneshot::Sender<Result<(), ApiError>>>),
    /// Nothing to tell: answers, through the task, once every call before it is answered
    /// (a report still queued then is dropped instead, as any is with a call after it).
    Settle(oneshot::Sender<Result<(), ApiError>>),
}

/// The Bridge's answer to a call, back to the task. A report's or an end's names the Local
/// session by its stream's path, which its `Debug` output leaves out.
enum Answer {
    Reported(String, Result<(), ApiError>),
    /// An end nobody waited for that failed.
    EndFailed(String, ApiError),
    /// Every call before a [`Outgoing::Settle`] is answered or dropped.
    Settled(oneshot::Sender<Result<(), ApiError>>),
}

impl Debug for Answer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reported(_, reported) => f.debug_tuple("Reported").field(reported).finish(),
            Self::EndFailed(_, error) => f.debug_tuple("EndFailed").field(error).finish(),
            Self::Settled(_) => f.write_str("Settled"),
        }
    }
}

impl LocalPlayback {
    /// Starts the task playing through `player`, taking its `events`, timing reports on
    /// `clock`, and giving up on a call to the Bridge after `call_timeout`. Must run within a
    /// Tokio runtime.
    pub fn new(
        player: Arc<dyn LocalPlayer>,
        events: mpsc::UnboundedReceiver<PlayerEvent>,
        clock: Arc<dyn Clock>,
        call_timeout: Duration,
    ) -> Self {
        let (commands, received) = mpsc::unbounded_channel();
        let (outgoing, to_send) = mpsc::unbounded_channel();
        let (answers, answered) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            player,
            now: Mutex::new(None),
            halted: Mutex::new(None),
        });
        let task = Task {
            shared: Arc::clone(&shared),
            clock,
            outgoing,
            next_report: None,
            started: None,
        };
        tokio::spawn(task.run(received, events, answered));
        tokio::spawn(send_calls(to_send, answers, call_timeout));
        Self { commands, shared }
    }

    /// What plays now, if anything.
    pub fn status(&self) -> Option<LocalStatus> {
        let position = self.shared.player.position();
        self.shared.read(|now| LocalStatus {
            item: now.item.clone(),
            title: now.media.title.clone(),
            artist: now.media.artist.clone(),
            artwork_url: now.media.artwork_url.clone(),
            duration: now.media.duration,
            position,
            playing: now.playing,
            report_failed: now.report_failed.clone(),
        })
    }

    /// Whether something plays (or is paused).
    pub fn is_active(&self) -> bool {
        self.shared.read(|_| ()).is_some()
    }

    /// Whether Local playback stopped on its own since the App last asked
    /// ([`LocalPlayback::take_halted`]).
    pub fn has_halted(&self) -> bool {
        lock(&self.shared.halted).is_some()
    }

    /// Whether why Local playback stopped on its own, not yet taken, ends the session; a probe
    /// for tests that leaves it for [`LocalPlayback::take_halted`].
    #[doc(hidden)]
    pub fn halt_ends_session(&self) -> bool {
        lock(&self.shared.halted)
            .as_ref()
            .is_some_and(Halted::ends_session)
    }

    /// Why Local playback stopped on its own since the App last asked, if it did.
    pub fn take_halted(&self) -> Option<Halted> {
        lock(&self.shared.halted).take()
    }

    /// Loads `started`, the Bridge's answer to a start, from the Bridge at `bridge_url`, and
    /// plays it from where the answer says; replaces what played without reporting on it, and
    /// forgets why Local playback last stopped on its own: an answer about an earlier Local
    /// session that comes later is dropped.
    pub async fn start(&self, started: api::LocalStart, bridge_url: &str) -> Result<(), ApiError> {
        let artist = started.author.or(started.podcast);
        let now = Now {
            item: started.item,
            media: Media {
                url: format!("{bridge_url}{}", started.stream),
                start_at: started.start_at,
                title: started.title,
                artist,
                artwork_url: started.artwork.map(|path| format!("{bridge_url}{path}")),
                duration: started.duration,
            },
            stream: started.stream,
            playing: true,
            interrupted: false,
            report_failed: None,
        };
        self.ask(|done| Command::Start(Box::new(now), done)).await
    }

    /// Pauses if it plays, else plays on; reports where it paused.
    pub async fn toggle(&self) -> Result<(), ApiError> {
        self.ask(Command::Toggle).await
    }

    /// Moves by `skip`, kept within the Item, and reports where to.
    pub async fn skip(&self, skip: Skip) -> Result<(), ApiError> {
        self.ask(|done| Command::Skip(skip, done)).await
    }

    /// Ends Local playback: stops the player, then ends the Local session on the Bridge with
    /// where it stopped as the last report, and answers how that went. A report being sent
    /// finishes first; one still queued isn't sent. With nothing playing, waits until every
    /// call still on its way (an end at the end of the Item, say) is answered, and answers
    /// `Ok`; [`LocalPlayback::take_halted`] then tells if that end failed. Either way answers
    /// `Err(Internal)` should the task, or the one sending its calls, be gone.
    pub async fn stop(&self) -> Result<(), ApiError> {
        self.ask(Command::Stop).await?
    }

    /// Asks `command` of the task, and waits until it says it's done.
    async fn ask<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<T>) -> Command,
    ) -> Result<T, ApiError> {
        let (done, answered) = oneshot::channel();
        if self.commands.send(command(done)).is_ok()
            && let Ok(answer) = answered.await
        {
            return Ok(answer);
        }
        tracing::error!("the Local playback task is gone");
        Err(ApiError::Internal)
    }
}

/// What `mutex` guards, poisoned or not.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    /// What `read` makes of what plays, if anything.
    fn read<T>(&self, read: impl FnOnce(&Now) -> T) -> Option<T> {
        lock(&self.now).as_ref().map(read)
    }

    fn update(&self, change: impl FnOnce(&mut Now)) {
        if let Some(now) = lock(&self.now).as_mut() {
            change(now);
        }
    }

    fn set(&self, now: Option<Now>) {
        *lock(&self.now) = now;
    }

    /// Whether it plays; none if nothing is loaded.
    fn playing(&self) -> Option<bool> {
        self.read(|now| now.playing)
    }

    /// Keeps `halted` as why Local playback stopped, unless an earlier reason not yet taken
    /// says more: the first one, or one that ends the session.
    fn halted(&self, halted: Halted) {
        let mut kept = lock(&self.halted);
        if kept.is_none() || halted.ends_session() {
            *kept = Some(halted);
        }
    }
}

/// The task running Local playback.
struct Task {
    shared: Arc<Shared>,
    clock: Arc<dyn Clock>,
    /// Where calls to the Bridge go.
    outgoing: mpsc::UnboundedSender<Outgoing>,
    /// When the next report on the timer is due: only while playing.
    next_report: Option<Instant>,
    /// The stream's path of the Local session started last, playing or not: answers about
    /// any other are old news.
    started: Option<String>,
}

impl Task {
    async fn run(
        mut self,
        mut commands: mpsc::UnboundedReceiver<Command>,
        mut events: mpsc::UnboundedReceiver<PlayerEvent>,
        mut answers: mpsc::UnboundedReceiver<Answer>,
    ) {
        loop {
            let due = self.next_report.map(|at| self.clock.sleep_until(at));
            let report_due = async {
                match due {
                    Some(due) => due.await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else {
                        break;
                    };
                    self.command(command);
                }
                Some(event) = events.recv() => self.event(event),
                Some(answer) = answers.recv() => self.answered(answer),
                () = report_due => {
                    self.schedule();
                    self.report(self.shared.player.position());
                }
            }
        }
        // The App is gone; so is what it played.
        self.shared.player.unload();
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Start(now, done) => {
                // Whatever stopped before is old news now. Defensive: `App::play_on_phone`
                // stops (or settles) and takes it first, so nothing is left here to drop.
                lock(&self.shared.halted).take();
                self.started = Some(now.stream.clone());
                self.shared.player.load(&now.media);
                self.shared.set(Some(*now));
                self.schedule();
                let _ = done.send(());
            }
            Command::Toggle(done) => {
                self.toggle();
                let _ = done.send(());
            }
            Command::Skip(skip, done) => {
                self.skip(skip);
                let _ = done.send(());
            }
            Command::Stop(answer) => {
                if self.shared.playing().is_some() {
                    let position = self.shared.player.pause();
                    self.end_at(position, Some(answer));
                } else {
                    self.send(Outgoing::Settle(answer));
                }
            }
        }
    }

    fn event(&mut self, event: PlayerEvent) {
        tracing::debug!(?event, "Local player event");
        let Some((playing, interrupted, duration)) = self
            .shared
            .read(|now| (now.playing, now.interrupted, now.media.duration))
        else {
            return;
        };
        match event {
            PlayerEvent::Play => self.play(),
            PlayerEvent::Pause => self.pause(),
            PlayerEvent::Toggle => self.toggle(),
            PlayerEvent::Skip(skip) => self.skip(skip),
            PlayerEvent::InterruptionBegan if playing => {
                self.pause();
                self.shared.update(|now| now.interrupted = true);
            }
            PlayerEvent::InterruptionBegan => {}
            PlayerEvent::InterruptionEnded { resume } => {
                if resume && interrupted {
                    self.play();
                }
                self.shared.update(|now| now.interrupted = false);
            }
            PlayerEvent::Ended => {
                let end = duration.unwrap_or_else(|| self.shared.player.position());
                self.end_at(end, None);
            }
            PlayerEvent::Failed => {
                tracing::warn!("the Local player can't play the stream; ending Local playback");
                let position = self.shared.player.position();
                self.end_at(position, None);
                self.shared.halted(Halted::PlayerFailed);
            }
        }
    }

    /// Takes the Bridge's answer to a call on the Local session that plays now (or, for an end,
    /// was started last); one on another (sent before a new start) is old news.
    fn answered(&mut self, answer: Answer) {
        let (stream, reported) = match answer {
            Answer::Settled(answer) => {
                let _ = answer.send(Ok(()));
                return;
            }
            Answer::EndFailed(stream, error) => {
                // The stream check is defensive, unreachable through the App:
                // `App::play_on_phone` settles before it starts another, so no end answers
                // after a later start.
                if self.started.as_ref() == Some(&stream) {
                    self.shared.halted(Halted::Api(error));
                }
                return;
            }
            Answer::Reported(stream, reported) => (stream, reported),
        };
        // A report sent before Stop can answer with nothing playing, or, in a race, after the
        // next start: Stop's own answer doesn't wait for the task to take this one. The
        // settle in `App::play_on_phone` closes that race only when nothing played.
        if self.shared.read(|now| now.stream == stream) != Some(true) {
            return;
        }
        match reported {
            Err(
                error @ (ApiError::ApiKeyRejected
                | ApiError::SessionInvalid
                | ApiError::NoLocalSession),
            ) => {
                tracing::info!(
                    %error,
                    "the Local session is gone on the Bridge; stopping Local playback"
                );
                self.halt();
                self.shared.halted(Halted::Api(error));
            }
            reported => self.shared.update(|now| now.report_failed = reported.err()),
        }
    }

    /// Reports on the timer again in [`REPORT_EVERY`].
    fn schedule(&mut self) {
        self.next_report = Some(self.clock.now() + REPORT_EVERY);
    }

    fn toggle(&mut self) {
        if self.shared.playing() == Some(true) {
            self.pause();
        } else {
            self.play();
        }
    }

    /// Plays on, if paused.
    fn play(&mut self) {
        if self.shared.playing() == Some(false) {
            self.shared.player.play();
            self.shared.update(|now| now.playing = true);
            self.schedule();
        }
    }

    /// Pauses, if it plays, and reports where. Either way, an interruption's end no longer
    /// resumes: a pause the person asked for, also after playing during the interruption,
    /// comes first.
    fn pause(&mut self) {
        let playing = self.shared.playing() == Some(true);
        self.shared.update(|now| {
            now.interrupted = false;
            now.playing = false;
        });
        if playing {
            let position = self.shared.player.pause();
            self.next_report = None;
            self.report(position);
        }
    }

    fn skip(&mut self, skip: Skip) {
        let Some((playing, duration)) = self.shared.read(|now| (now.playing, now.media.duration))
        else {
            return;
        };
        let mut target = (self.shared.player.position() + f64::from(skip.seconds())).max(0.0);
        if let Some(duration) = duration {
            target = target.min(duration);
        }
        self.shared.player.seek(target);
        if playing {
            self.schedule();
        }
        self.report(target);
    }

    /// Ends Local playback at `position`: stops the player, and has its Local session end
    /// with that as the last report, its answer going to `answer` if given.
    fn end_at(&mut self, position: f64, answer: Option<oneshot::Sender<Result<(), ApiError>>>) {
        let Some(report) = self.report_at(position) else {
            if let Some(answer) = answer {
                let _ = answer.send(Ok(()));
            }
            return;
        };
        self.halt();
        self.send(Outgoing::End(report, answer));
    }

    /// Stops the player and forgets what played, without telling the Bridge.
    fn halt(&mut self) {
        self.shared.player.unload();
        self.shared.set(None);
        self.next_report = None;
    }

    /// Has `position` reported, if something plays.
    fn report(&self, position: f64) {
        if let Some(report) = self.report_at(position) {
            self.send(Outgoing::Report(report));
        }
    }

    /// Hands `call` to the sender task; should that be gone, says so as its answer would.
    fn send(&self, call: Outgoing) {
        let Err(mpsc::error::SendError(call)) = self.outgoing.send(call) else {
            return;
        };
        tracing::error!("the Local playback sender task is gone; the Bridge isn't told");
        match call {
            Outgoing::Report(_) => self
                .shared
                .update(|now| now.report_failed = Some(ApiError::Internal)),
            Outgoing::End(_, Some(answer)) | Outgoing::Settle(answer) => {
                let _ = answer.send(Err(ApiError::Internal));
            }
            Outgoing::End(_, None) => self.shared.halted(Halted::Api(ApiError::Internal)),
        }
    }

    /// `position` as a report on what plays, if anything.
    fn report_at(&self, position: f64) -> Option<Report> {
        self.shared.read(|now| Report {
            stream: now.stream.clone(),
            position,
            duration: now.media.duration,
        })
    }
}

/// Sends the calls `outgoing` brings to the Bridge, one at a time, each within
/// `call_timeout`, and hands the answers on: to `answers`, or to whoever waits for an end. A
/// report still queued when anything comes after it isn't sent.
async fn send_calls(
    mut outgoing: mpsc::UnboundedReceiver<Outgoing>,
    answers: mpsc::UnboundedSender<Answer>,
    call_timeout: Duration,
) {
    let mut queued = VecDeque::new();
    loop {
        if queued.is_empty() {
            let Some(call) = outgoing.recv().await else {
                return;
            };
            queued.push_back(call);
        }
        while let Ok(call) = outgoing.try_recv() {
            queued.push_back(call);
        }
        let Some(call) = queued.pop_front() else {
            continue;
        };
        match call {
            // Something newer comes after it.
            Outgoing::Report(_) if !queued.is_empty() => {}
            Outgoing::Report(Report {
                stream,
                position,
                duration,
            }) => {
                let call = api::report_local(stream.clone(), position, duration);
                let reported = send(call_timeout, "report", position, call).await;
                let _ = answers.send(Answer::Reported(stream, reported));
            }
            Outgoing::End(
                Report {
                    stream,
                    position,
                    duration,
                },
                answer,
            ) => {
                let call = api::end_local(stream.clone(), position, duration);
                let ended = send(call_timeout, "end", position, call).await;
                match (answer, ended) {
                    (Some(answer), ended) => {
                        let _ = answer.send(ended);
                    }
                    (None, Err(error)) => {
                        let _ = answers.send(Answer::EndFailed(stream, error));
                    }
                    (None, Ok(())) => {}
                }
            }
            // Through the task, so it took every answer before this one first.
            Outgoing::Settle(answer) => {
                let _ = answers.send(Answer::Settled(answer));
            }
        }
    }
}

/// Makes `call`, which tells the Bridge `position` (a `what`, for the log), within
/// `call_timeout`.
async fn send(
    call_timeout: Duration,
    what: &'static str,
    position: f64,
    call: impl Future<Output = Result<(), ApiError>>,
) -> Result<(), ApiError> {
    let sent = crate::client::bridge_call(call_timeout, call).await;
    if let Err(error) = &sent {
        tracing::warn!(%error, what, position, "Local playback's position not reported");
    }
    sent
}
