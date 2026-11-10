//! The App's [`LocalPlayer`] on iOS (ADR 0007): AVPlayer, the audio session (`playback`, so it
//! plays on in the background and locked, with the audio background mode in `Dioxus.toml`),
//! Now Playing on the lock screen and in Control Center (title, artist, artwork, elapsed time,
//! duration, rate) and its remote commands (play, pause, toggle, 15 s back, 30 s forward), all
//! through `objc2`, as the device-verified spike for #2 did.
//!
//! What iOS does on its own reaches App logic as [`PlayerEvent`]s, never acted on here: remote
//! commands, interruptions (a call pauses AVPlayer; its end says whether to resume), headphones
//! going away (iOS pauses AVPlayer) and the end of the Item. AirPlay and Bluetooth route
//! changes need nothing: AVPlayer keeps its position across them.
//!
//! AVFoundation and MediaPlayer objects live on the main thread, in a thread-local there.
//! [`AvPlayer`] is the handle any thread may call: each call runs on the main queue in turn,
//! and the position comes from a copy the main thread keeps current (a pause reads it from
//! AVPlayer itself, waiting for the main queue).

// Calls into Objective-C are `unsafe`; this module is the App's only such code.
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, PoisonError};

use api::Skip;
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::{Retained, Weak};
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::{AnyThread, MainThreadMarker, msg_send};
use objc2_av_foundation::{
    AVPlayer, AVPlayerItem, AVPlayerItemDidPlayToEndTimeNotification,
    AVPlayerItemFailedToPlayToEndTimeErrorKey, AVPlayerItemFailedToPlayToEndTimeNotification,
    AVPlayerItemStatus,
};
use objc2_avf_audio::{
    AVAudioSession, AVAudioSessionCategoryPlayback, AVAudioSessionInterruptionNotification,
    AVAudioSessionInterruptionOptionKey, AVAudioSessionInterruptionOptions,
    AVAudioSessionInterruptionType, AVAudioSessionInterruptionTypeKey,
    AVAudioSessionRouteChangeNotification, AVAudioSessionRouteChangeReason,
    AVAudioSessionRouteChangeReasonKey,
};
use objc2_core_foundation::CGSize;
use objc2_core_media::CMTime;
use objc2_foundation::{
    NSArray, NSData, NSDictionary, NSError, NSNotification, NSNotificationCenter,
    NSNotificationName, NSNumber, NSObjectProtocol, NSOperationQueue, NSString, NSURL,
};
use objc2_media_player::{
    MPMediaItemArtwork, MPMediaItemPropertyArtist, MPMediaItemPropertyArtwork,
    MPMediaItemPropertyPlaybackDuration, MPMediaItemPropertyTitle, MPNowPlayingInfoCenter,
    MPNowPlayingInfoPropertyElapsedPlaybackTime, MPNowPlayingInfoPropertyPlaybackRate,
    MPRemoteCommand, MPRemoteCommandCenter, MPRemoteCommandEvent, MPRemoteCommandHandlerStatus,
};
use objc2_ui_kit::UIImage;
use tokio::sync::mpsc;

use crate::flow::{App, CALL_TIMEOUT};
use crate::local::{LocalPlayer, Media, PlayerEvent, PlayerEvents};

/// How often AVPlayer tells the main thread where it is, in seconds.
const POSITION_EVERY: f64 = 1.0;

/// `app` offering Local playback through AVPlayer, reporting on the system clock.
pub fn with_player(app: App) -> App {
    let (events, received) = mpsc::unbounded_channel();
    let position = Position::default();
    let shared = position.clone();
    on_main(move |_| init(shared, events));
    let player = Arc::new(AvPlayer { position });
    app.with_local_player(player, received, Arc::new(clock::SystemClock))
}

/// AVPlayer as the App's [`LocalPlayer`]; see the module.
#[derive(Debug)]
struct AvPlayer {
    position: Position,
}

/// Where AVPlayer is, in seconds, as it last told or a load or seek set; shared between the
/// main thread and [`AvPlayer`]. While a load's or seek's move hasn't landed, AVPlayer still
/// tells the time from before it, which then doesn't count.
#[derive(Debug, Clone, Default)]
struct Position(Arc<Mutex<Spot>>);

#[derive(Debug, Default)]
struct Spot {
    at: f64,
    /// Counts loads and unloads, so a seek's landing for an earlier load, or after an unload,
    /// is told apart.
    load: u64,
    /// Seeks asked of the current load that haven't landed.
    seeking: usize,
}

impl Position {
    fn spot(&self) -> std::sync::MutexGuard<'_, Spot> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn get(&self) -> f64 {
        self.spot().at
    }

    /// A new load, moving to `at`: answers the load's number for its landing.
    fn load_at(&self, at: f64) -> u64 {
        let mut spot = self.spot();
        spot.at = at;
        spot.load += 1;
        spot.seeking = 1;
        spot.load
    }

    /// A seek of the current load to `at`: answers the load's number for its landing.
    fn seek_to(&self, at: f64) -> u64 {
        let mut spot = self.spot();
        spot.at = at;
        spot.seeking += 1;
        spot.load
    }

    /// A seek of load `load` landed (or gave way to a later one): answers whether it was the
    /// last one of the current load still on its way.
    fn landed(&self, load: u64) -> bool {
        let mut spot = self.spot();
        if spot.load != load {
            return false;
        }
        spot.seeking = spot.seeking.saturating_sub(1);
        spot.seeking == 0
    }

    /// AVPlayer tells it's at `at`: taken unless a seek is still on its way.
    fn observed(&self, at: f64) {
        let mut spot = self.spot();
        if spot.seeking == 0 && at.is_finite() {
            spot.at = at;
        }
    }

    /// Nothing loaded.
    fn clear(&self) {
        let mut spot = self.spot();
        spot.at = 0.0;
        spot.seeking = 0;
        // A seek still on its way landing later is about nothing loaded now.
        spot.load += 1;
    }
}

impl LocalPlayer for AvPlayer {
    fn load(&self, media: &Media) {
        let number = self.position.load_at(media.start_at);
        let loaded = media.clone();
        on_main(move |mtm| load(mtm, loaded, number));
        if let Some(url) = media.artwork_url.clone() {
            tokio::spawn(async move {
                let Some(bytes) = crate::client::fetch(&url, CALL_TIMEOUT).await else {
                    tracing::info!("no artwork for Now Playing");
                    return;
                };
                on_main(move |_| set_artwork(&url, bytes));
            });
        }
    }

    fn play(&self) {
        on_main(|_| {
            activate_session();
            with_main(|main| {
                if let Some(player) = &main.player {
                    unsafe { player.play() };
                }
                main.playing = true;
            });
            update_now_playing(None);
        });
    }

    fn pause(&self) -> f64 {
        on_main_sync(|| {
            with_main(|main| {
                if let Some(player) = &main.player {
                    unsafe { player.pause() };
                    // An item that never got ready (it failed to load) is at 0, not where it
                    // was asked to start, and one still seeking is where it was: the position
                    // kept stays then.
                    let ready = unsafe { player.currentItem() }.is_some_and(|item| unsafe {
                        item.status() == AVPlayerItemStatus::ReadyToPlay
                    });
                    if ready {
                        main.position
                            .observed(unsafe { player.currentTime().seconds() });
                    }
                }
                main.playing = false;
            });
            update_now_playing(None);
        });
        self.position.get()
    }

    fn seek(&self, position: f64) {
        let number = self.position.seek_to(position);
        let landed = self.position.clone();
        on_main(move |_| {
            with_main(|main| {
                if let Some(player) = &main.player {
                    let time = unsafe { CMTime::with_seconds(position, 1000) };
                    unsafe { player.seekToTime_completionHandler(time, &landing(landed, number)) };
                }
            });
            // AVPlayer seeks in the background: until then it still tells the old time.
            update_now_playing(Some(position));
        });
    }

    fn unload(&self) {
        self.position.clear();
        on_main(|_| {
            with_main(|main| {
                main.release_player();
                main.media = None;
                main.artwork = None;
                main.playing = false;
            });
            unsafe { MPNowPlayingInfoCenter::defaultCenter().setNowPlayingInfo(None) };
        });
    }

    fn position(&self) -> f64 {
        self.position.get()
    }
}

/// Runs `work` on the main queue, after what was asked of it before.
fn on_main(work: impl FnOnce(MainThreadMarker) + Send + 'static) {
    DispatchQueue::main().exec_async(move || {
        let mtm = MainThreadMarker::new().expect("the main queue runs on the main thread");
        work(mtm);
    });
}

/// Runs `work` on the main queue, after what was asked of it before, and waits for it. On the
/// main thread, where waiting would deadlock, it runs at once instead, ahead of work already
/// queued. Only [`LocalPlayer::pause`] uses it, called only from the Local playback task,
/// never from the main thread.
fn on_main_sync(work: impl FnOnce() + Send) {
    if MainThreadMarker::new().is_some() {
        work();
    } else {
        DispatchQueue::main().exec_sync(work);
    }
}

/// What the player keeps on the main thread.
struct Main {
    position: Position,
    events: PlayerEvents,
    player: Option<Retained<AVPlayer>>,
    time_observer: Option<Retained<AnyObject>>,
    /// Observing the end and failure of the player's item, that one's only.
    item_observers: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    media: Option<Media>,
    /// The Now Playing artwork of `media`, once fetched.
    artwork: Option<Retained<MPMediaItemArtwork>>,
    /// Whether App logic last had it play.
    playing: bool,
    /// Kept so their callbacks keep firing.
    _command_targets: Vec<Retained<AnyObject>>,
    _observers: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

impl Main {
    /// Pauses and drops the player, if any, with its observers.
    fn release_player(&mut self) {
        if let Some(player) = self.player.take() {
            unsafe { player.pause() };
            if let Some(observer) = self.time_observer.take() {
                unsafe { player.removeTimeObserver(&observer) };
            }
        }
        let center = NSNotificationCenter::defaultCenter();
        for observer in self.item_observers.drain(..) {
            unsafe { center.removeObserver(observer.as_ref()) };
        }
    }
}

thread_local! {
    static MAIN: RefCell<Option<Main>> = const { RefCell::new(None) };
}

/// Runs `work` on what the main thread keeps, once set up.
fn with_main<R>(work: impl FnOnce(&mut Main) -> R) -> Option<R> {
    MAIN.with_borrow_mut(|main| main.as_mut().map(work))
}

/// Tells App logic `event`, from a callback on the main thread.
fn send(event: PlayerEvent) {
    // Nobody listens once the App is gone.
    with_main(|main| main.events.send(event));
}

/// Sets up the audio session, remote commands and observers, once: a second App (a hot
/// reload in development) takes over the player and its events, rather than registering
/// another set whose handlers would send every event twice.
fn init(position: Position, events: PlayerEvents) {
    let taken_over = with_main(|main| {
        main.release_player();
        main.media = None;
        main.artwork = None;
        main.playing = false;
        main.position = position.clone();
        main.events = events.clone();
    });
    if taken_over.is_some() {
        return;
    }
    let session = unsafe { AVAudioSession::sharedInstance() };
    match unsafe { AVAudioSessionCategoryPlayback } {
        Some(category) => {
            if let Err(error) = unsafe { session.setCategory_error(category) } {
                tracing::warn!(%error, "the audio session refused the playback category");
            }
        }
        None => tracing::warn!(
            "iOS has no playback category; Local playback stops in the background and locked"
        ),
    }

    let center = unsafe { MPRemoteCommandCenter::sharedCommandCenter() };
    let (back, forward) = unsafe { (center.skipBackwardCommand(), center.skipForwardCommand()) };
    let interval = |skip: Skip| {
        let seconds = NSNumber::new_f64(f64::from(skip.seconds().unsigned_abs()));
        NSArray::from_retained_slice(&[seconds])
    };
    unsafe {
        back.setPreferredIntervals(&interval(Skip::Back15));
        forward.setPreferredIntervals(&interval(Skip::Forward30));
    }
    let command_targets = unsafe {
        vec![
            command(&center.playCommand(), PlayerEvent::Play),
            command(&center.pauseCommand(), PlayerEvent::Pause),
            command(&center.togglePlayPauseCommand(), PlayerEvent::Toggle),
            command(&back, PlayerEvent::Skip(Skip::Back15)),
            command(&forward, PlayerEvent::Skip(Skip::Forward30)),
        ]
    };

    let observers = unsafe {
        vec![
            observe(
                AVAudioSessionInterruptionNotification,
                None,
                on_interruption,
            ),
            observe(AVAudioSessionRouteChangeNotification, None, on_route_change),
        ]
    };

    MAIN.set(Some(Main {
        position,
        events,
        player: None,
        time_observer: None,
        item_observers: Vec::new(),
        media: None,
        artwork: None,
        playing: false,
        _command_targets: command_targets,
        _observers: observers,
    }));
}

/// Has `command` tell App logic `event`; the target to keep.
fn command(command: &MPRemoteCommand, event: PlayerEvent) -> Retained<AnyObject> {
    let handler = RcBlock::new(move |_: NonNull<MPRemoteCommandEvent>| {
        send(event);
        if with_main(|main| main.player.is_some()) == Some(true) {
            MPRemoteCommandHandlerStatus::Success
        } else {
            MPRemoteCommandHandlerStatus::NoSuchContent
        }
    });
    unsafe {
        command.setEnabled(true);
        command.addTargetWithHandler(&handler)
    }
}

/// Runs `handler` on the main queue for every notification named `name` (from `object` only,
/// if given); the observer to keep.
fn observe(
    name: Option<&NSNotificationName>,
    object: Option<&AnyObject>,
    handler: fn(&NSNotification),
) -> Retained<ProtocolObject<dyn NSObjectProtocol>> {
    let block = RcBlock::new(move |notification: NonNull<NSNotification>| {
        handler(unsafe { notification.as_ref() });
    });
    unsafe {
        NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
            name,
            object,
            Some(&NSOperationQueue::mainQueue()),
            &block,
        )
    }
}

/// Observes the end and the failure of `item`, until `release_player` removes the observers.
/// A notification already queued on the main queue by then may still arrive, and end what
/// plays by then (very unlikely; #27 may tell loads apart in [`PlayerEvent`]).
fn observe_item(item: &AVPlayerItem) -> Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>> {
    let object: &AnyObject = item.as_ref();
    unsafe {
        vec![
            observe(
                Some(AVPlayerItemDidPlayToEndTimeNotification),
                Some(object),
                |_| send(PlayerEvent::Ended),
            ),
            observe(
                Some(AVPlayerItemFailedToPlayToEndTimeNotification),
                Some(object),
                on_failed,
            ),
        ]
    }
}

fn on_failed(notification: &NSNotification) {
    let error = notification
        .userInfo()
        .and_then(|info| info.objectForKey(unsafe { AVPlayerItemFailedToPlayToEndTimeErrorKey }))
        .and_then(|error| error.downcast::<NSError>().ok());
    match error {
        Some(error) => tracing::warn!(
            error = %error.localizedDescription(),
            code = error.code(),
            "AVPlayer failed to play the stream to its end"
        ),
        None => tracing::warn!("AVPlayer failed to play the stream to its end, saying no why"),
    }
    send(PlayerEvent::Failed);
}

/// A number in `notification`'s user info under `key`.
fn user_info(notification: &NSNotification, key: Option<&NSString>) -> Option<usize> {
    let info = notification.userInfo()?;
    let value = info.objectForKey(key?)?;
    Some(value.downcast::<NSNumber>().ok()?.as_usize())
}

fn on_interruption(notification: &NSNotification) {
    let kind = user_info(notification, unsafe { AVAudioSessionInterruptionTypeKey });
    match kind.map(AVAudioSessionInterruptionType) {
        Some(AVAudioSessionInterruptionType::Began) => send(PlayerEvent::InterruptionBegan),
        Some(AVAudioSessionInterruptionType::Ended) => {
            let options = user_info(notification, unsafe { AVAudioSessionInterruptionOptionKey })
                .map_or(
                    AVAudioSessionInterruptionOptions(0),
                    AVAudioSessionInterruptionOptions,
                );
            let resume = options.contains(AVAudioSessionInterruptionOptions::ShouldResume);
            send(PlayerEvent::InterruptionEnded { resume });
        }
        _ => tracing::debug!(?kind, "an interruption of an unknown type"),
    }
}

fn on_route_change(notification: &NSNotification) {
    let reason = user_info(notification, unsafe { AVAudioSessionRouteChangeReasonKey })
        .map(AVAudioSessionRouteChangeReason);
    tracing::info!(?reason, "the audio route changed");
    // Headphones went away: iOS paused AVPlayer, as App logic now does too.
    if reason == Some(AVAudioSessionRouteChangeReason::OldDeviceUnavailable)
        && with_main(|main| main.playing) == Some(true)
    {
        send(PlayerEvent::Pause);
    }
}

fn activate_session() {
    if let Err(error) = unsafe { AVAudioSession::sharedInstance().setActive_error(true) } {
        tracing::warn!(%error, "the audio session did not activate");
    }
}

/// Replaces what plays with `media`, playing from `media.start_at`; `number` is the load's
/// ([`Position::load_at`]).
fn load(mtm: MainThreadMarker, media: Media, number: u64) {
    let Some(url) = NSURL::URLWithString(&NSString::from_str(&media.url)) else {
        tracing::warn!("a Local playback stream URL AVPlayer doesn't take");
        send(PlayerEvent::Failed);
        return;
    };
    let Some(position) = with_main(|main| main.position.clone()) else {
        tracing::warn!("the Local player isn't set up; nothing loads");
        return;
    };
    activate_session();
    let player = unsafe { AVPlayer::playerWithURL(&url, mtm) };
    let item = unsafe { player.currentItem() };
    // Weak: the player keeps the block, and the item.
    let weak_item = item.as_ref().map(Weak::from);
    let observed = position.clone();
    let tick = RcBlock::new(move |time: CMTime| {
        // Before the item is ready, AVPlayer tells 0, not where it starts.
        let ready = weak_item
            .as_ref()
            .and_then(Weak::load)
            .is_some_and(|item| unsafe { item.status() == AVPlayerItemStatus::ReadyToPlay });
        if ready {
            observed.observed(unsafe { time.seconds() });
        }
    });
    let observer = unsafe {
        player.addPeriodicTimeObserverForInterval_queue_usingBlock(
            CMTime::with_seconds(POSITION_EVERY, 1000),
            None,
            &tick,
        )
    };
    unsafe {
        player.seekToTime_completionHandler(
            CMTime::with_seconds(media.start_at, 1000),
            &landing(position, number),
        );
        player.play();
    }
    let item_observers = item.map(|item| observe_item(&item)).unwrap_or_default();
    let start_at = media.start_at;
    with_main(|main| {
        main.release_player();
        main.player = Some(player);
        main.time_observer = Some(observer);
        main.item_observers = item_observers;
        main.media = Some(media);
        main.artwork = None;
        main.playing = true;
    });
    // A fresh item tells 0 until its seek is done.
    update_now_playing(Some(start_at));
}

/// A seek's completion handler: tells `position` the seek of load `number` landed. AVPlayer
/// answers `NO` for a seek that gave way to a later one, which is fine, and for one it
/// cancelled (say, outside what it can seek to): if that was the newest, reports count from
/// wherever AVPlayer plays, so it is logged.
fn landing(position: Position, number: u64) -> RcBlock<dyn Fn(Bool)> {
    RcBlock::new(move |finished: Bool| {
        if position.landed(number) && !finished.as_bool() {
            tracing::warn!(
                target = position.get(),
                "AVPlayer cancelled the seek; reports count from where it plays"
            );
        }
    })
}

/// Sets `bytes`, the image at `url`, as the Now Playing artwork, if what plays still has it.
fn set_artwork(url: &str, bytes: Vec<u8>) {
    let current = with_main(|main| {
        main.media
            .as_ref()
            .is_some_and(|media| media.artwork_url.as_deref() == Some(url))
    });
    if current != Some(true) {
        return;
    }
    let Some(image) = UIImage::imageWithData(&NSData::from_vec(bytes)) else {
        tracing::info!("the artwork is no image iOS reads");
        return;
    };
    let size = unsafe { image.size() };
    let handler = RcBlock::new(move |_: CGSize| -> NonNull<UIImage> { NonNull::from(&*image) });
    // MediaPlayer's iOS initializer, which `objc2-media-player` binds for macOS only.
    let artwork: Retained<MPMediaItemArtwork> = unsafe {
        msg_send![
            MPMediaItemArtwork::alloc(),
            initWithBoundsSize: size,
            requestHandler: &*handler
        ]
    };
    with_main(|main| main.artwork = Some(artwork));
    update_now_playing(None);
}

/// Publishes what plays to the lock screen and Control Center, `elapsed` seconds in if given
/// (right after a load or seek, which AVPlayer doesn't tell yet), else where AVPlayer is.
fn update_now_playing(elapsed: Option<f64>) {
    let info = with_main(|main| {
        let media = main.media.as_ref()?;
        let elapsed = elapsed
            .or_else(|| {
                main.player
                    .as_ref()
                    .map(|player| unsafe { player.currentTime().seconds() })
                    .filter(|at| at.is_finite())
            })
            .unwrap_or(media.start_at);
        let mut keys: Vec<&NSString> = Vec::new();
        let mut values: Vec<Retained<AnyObject>> = Vec::new();
        let mut add = |key: &'static NSString, value: Retained<AnyObject>| {
            keys.push(key);
            values.push(value);
        };
        unsafe {
            add(
                MPMediaItemPropertyTitle,
                NSString::from_str(&media.title).into(),
            );
            if let Some(artist) = &media.artist {
                add(MPMediaItemPropertyArtist, NSString::from_str(artist).into());
            }
            if let Some(duration) = media.duration {
                add(
                    MPMediaItemPropertyPlaybackDuration,
                    NSNumber::new_f64(duration).into(),
                );
            }
            add(
                MPNowPlayingInfoPropertyElapsedPlaybackTime,
                NSNumber::new_f64(elapsed).into(),
            );
            let rate = if main.playing { 1.0 } else { 0.0 };
            add(
                MPNowPlayingInfoPropertyPlaybackRate,
                NSNumber::new_f64(rate).into(),
            );
            if let Some(artwork) = &main.artwork {
                add(MPMediaItemPropertyArtwork, Retained::clone(artwork).into());
            }
        }
        let values: Vec<&AnyObject> = values.iter().map(|value| &**value).collect();
        Some(NSDictionary::<NSString, AnyObject>::from_slices(
            &keys, &values,
        ))
    })
    .flatten();
    if let Some(info) = info {
        unsafe { MPNowPlayingInfoCenter::defaultCenter().setNowPlayingInfo(Some(&info)) };
    }
}
