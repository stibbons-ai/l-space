//! Native AVPlayer playback driven from Rust through `objc2`.
//!
//! Everything here runs on the main thread: Dioxus event handlers, remote command handlers and
//! notification observers registered on the main queue. Player state therefore lives in a
//! thread-local instead of behind a lock.

use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2_av_foundation::{
    AVPlayer, AVPlayerItemFailedToPlayToEndTimeNotification,
    AVPlayerItemPlaybackStalledNotification, AVPlayerTimeControlStatus,
};
use objc2_avf_audio::{
    AVAudioSession, AVAudioSessionCategoryPlayback, AVAudioSessionInterruptionNotification,
    AVAudioSessionInterruptionOptionKey, AVAudioSessionInterruptionOptions,
    AVAudioSessionInterruptionType, AVAudioSessionInterruptionTypeKey,
    AVAudioSessionRouteChangeNotification, AVAudioSessionRouteChangeReasonKey,
};
use objc2_core_media::CMTime;
use objc2_foundation::{
    NSDictionary, NSNotification, NSNotificationCenter, NSNotificationName, NSNumber,
    NSObjectProtocol, NSOperationQueue, NSString, NSURL,
};
use objc2_media_player::{
    MPMediaItemPropertyTitle, MPNowPlayingInfoCenter, MPNowPlayingInfoPropertyElapsedPlaybackTime,
    MPNowPlayingInfoPropertyPlaybackRate, MPRemoteCommand, MPRemoteCommandCenter,
    MPRemoteCommandEvent, MPRemoteCommandHandlerStatus,
};

/// How often the player logs a heartbeat, so a locked-phone run leaves a gap-free trail.
const HEARTBEAT_SECONDS: f64 = 60.0;

static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Appends a timestamped line to the in-app log and to stderr (visible in the device console).
pub fn log(message: impl AsRef<str>) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let line = format!(
        "{:02}:{:02}:{:02}Z {}",
        secs / 3600 % 24,
        secs / 60 % 60,
        secs % 60,
        message.as_ref()
    );
    eprintln!("[spike] {line}");
    LOG.lock().unwrap().push(line);
}

pub fn log_lines() -> Vec<String> {
    LOG.lock().unwrap().clone()
}

/// Objects that must stay alive for callbacks to keep firing.
struct State {
    player: Option<Retained<AVPlayer>>,
    periodic_observer: Option<Retained<AnyObject>>,
    _command_targets: Vec<Retained<AnyObject>>,
    _observers: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn main_thread() -> MainThreadMarker {
    MainThreadMarker::new().expect("player calls must run on the main thread")
}

fn with_player<R>(f: impl FnOnce(&AVPlayer) -> R) -> Option<R> {
    STATE.with_borrow(|state| state.as_ref()?.player.as_deref().map(f))
}

/// Configures the audio session and registers remote commands and notification observers.
/// Idempotent; call once at startup.
pub fn init() {
    let _ = main_thread();
    if STATE.with_borrow(Option::is_some) {
        return;
    }

    let session = unsafe { AVAudioSession::sharedInstance() };
    let category = unsafe { AVAudioSessionCategoryPlayback }.expect("playback category");
    match unsafe { session.setCategory_error(category) } {
        Ok(()) => log("audio session: category playback"),
        Err(e) => log(format!("audio session: setCategory failed: {e}")),
    }

    let center = unsafe { MPRemoteCommandCenter::sharedCommandCenter() };
    let (play, pause, toggle) = unsafe {
        (
            center.playCommand(),
            center.pauseCommand(),
            center.togglePlayPauseCommand(),
        )
    };
    let command_targets = vec![
        add_command(&play, "play", |p| unsafe { p.play() }),
        add_command(&pause, "pause", |p| unsafe { p.pause() }),
        add_command(&toggle, "toggle", |p| {
            if unsafe { p.rate() } > 0.0 {
                unsafe { p.pause() }
            } else {
                unsafe { p.play() }
            }
        }),
    ];

    let observers = vec![
        observe(
            unsafe { AVAudioSessionInterruptionNotification },
            on_interruption,
        ),
        observe(
            unsafe { AVAudioSessionRouteChangeNotification },
            on_route_change,
        ),
        observe(
            Some(unsafe { AVPlayerItemPlaybackStalledNotification }),
            |_| {
                log("item: playback stalled");
            },
        ),
        observe(
            Some(unsafe { AVPlayerItemFailedToPlayToEndTimeNotification }),
            |_| {
                log("item: failed to play to end");
            },
        ),
    ];

    STATE.set(Some(State {
        player: None,
        periodic_observer: None,
        _command_targets: command_targets,
        _observers: observers,
    }));
    log(format!("route: {}", current_outputs()));
}

fn add_command(
    command: &MPRemoteCommand,
    name: &'static str,
    action: fn(&AVPlayer),
) -> Retained<AnyObject> {
    let handler = RcBlock::new(move |_event: NonNull<MPRemoteCommandEvent>| {
        log(format!("remote command reached Rust: {name}"));
        if with_player(action).is_none() {
            return MPRemoteCommandHandlerStatus::NoSuchContent;
        }
        if name != "pause" {
            activate_session();
        }
        update_now_playing();
        MPRemoteCommandHandlerStatus::Success
    });
    unsafe {
        command.setEnabled(true);
        command.addTargetWithHandler(&handler)
    }
}

fn observe(
    name: Option<&NSNotificationName>,
    handler: fn(&NSNotification),
) -> Retained<ProtocolObject<dyn NSObjectProtocol>> {
    let block = RcBlock::new(move |notification: NonNull<NSNotification>| {
        handler(unsafe { notification.as_ref() });
    });
    unsafe {
        NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
            name,
            None,
            Some(&NSOperationQueue::mainQueue()),
            &block,
        )
    }
}

fn user_info_usize(notification: &NSNotification, key: Option<&NSString>) -> Option<usize> {
    let info = notification.userInfo()?;
    let value = info.objectForKey(key?)?;
    Some(value.downcast::<NSNumber>().ok()?.as_usize())
}

fn on_interruption(notification: &NSNotification) {
    let kind = user_info_usize(notification, unsafe { AVAudioSessionInterruptionTypeKey });
    match kind.map(AVAudioSessionInterruptionType) {
        Some(AVAudioSessionInterruptionType::Began) => {
            log("interruption began");
            update_now_playing();
        }
        Some(AVAudioSessionInterruptionType::Ended) => {
            let options =
                user_info_usize(notification, unsafe { AVAudioSessionInterruptionOptionKey })
                    .map_or(
                        AVAudioSessionInterruptionOptions(0),
                        AVAudioSessionInterruptionOptions,
                    );
            if options.contains(AVAudioSessionInterruptionOptions::ShouldResume) {
                log("interruption ended (shouldResume): resuming");
                activate_session();
                with_player(|p| unsafe { p.play() });
                update_now_playing();
            } else {
                log("interruption ended without shouldResume: staying paused");
            }
        }
        _ => log(format!("interruption with unknown type {kind:?}")),
    }
}

fn on_route_change(notification: &NSNotification) {
    let reason = user_info_usize(notification, unsafe { AVAudioSessionRouteChangeReasonKey });
    log(format!(
        "route change (reason {reason:?}): {}",
        current_outputs()
    ));
}

/// Current audio outputs as `name [port type]`, e.g. `Living Room [AirPlay]`.
pub fn current_outputs() -> String {
    let route = unsafe { AVAudioSession::sharedInstance().currentRoute() };
    let outputs = unsafe { route.outputs() };
    outputs
        .iter()
        .map(|port| unsafe { format!("{} [{}]", port.portName(), port.portType()) })
        .collect::<Vec<_>>()
        .join(", ")
}

fn activate_session() {
    if let Err(e) = unsafe { AVAudioSession::sharedInstance().setActive_error(true) } {
        log(format!("audio session: setActive failed: {e}"));
    }
}

/// Replaces the current item with `url` and starts playing it.
pub fn play_url(url: &str) {
    let mtm = main_thread();
    let Some(ns_url) = NSURL::URLWithString(&NSString::from_str(url)) else {
        log(format!("invalid URL: {url}"));
        return;
    };
    activate_session();

    let player = unsafe { AVPlayer::playerWithURL(&ns_url, mtm) };
    // AVPlayer also fires the observer on start and rate changes; log each whole second once.
    let last_second = Cell::new(f64::NAN);
    let heartbeat = RcBlock::new(move |time: CMTime| {
        let second = unsafe { time.seconds() }.floor();
        if second == last_second.replace(second) {
            return;
        }
        log(format!("heartbeat: t={second}s, {}", status()));
    });
    let periodic_observer = unsafe {
        player.addPeriodicTimeObserverForInterval_queue_usingBlock(
            CMTime::with_seconds(HEARTBEAT_SECONDS, 1),
            None,
            &heartbeat,
        )
    };
    unsafe { player.play() };

    STATE.with_borrow_mut(|state| {
        let state = state.as_mut().expect("init() runs before play_url()");
        if let (Some(old), Some(observer)) = (&state.player, &state.periodic_observer) {
            unsafe {
                old.removeTimeObserver(observer);
                old.pause();
            }
        }
        state.player = Some(player);
        state.periodic_observer = Some(periodic_observer);
    });
    log(format!("play {url}"));
    update_now_playing();
}

pub fn play() {
    activate_session();
    if with_player(|p| unsafe { p.play() }).is_some() {
        log("ui: play");
        update_now_playing();
    }
}

pub fn pause() {
    if with_player(|p| unsafe { p.pause() }).is_some() {
        log("ui: pause");
        update_now_playing();
    }
}

/// A one-line player status for the UI.
pub fn status() -> String {
    with_player(|p| {
        let status = match unsafe { p.timeControlStatus() } {
            AVPlayerTimeControlStatus::Playing => "playing",
            AVPlayerTimeControlStatus::Paused => "paused",
            AVPlayerTimeControlStatus::WaitingToPlayAtSpecifiedRate => "buffering",
            _ => "unknown",
        };
        format!("{status}, t={:.0}s", unsafe { p.currentTime().seconds() })
    })
    .unwrap_or_else(|| "no player".to_owned())
}

/// Publishes title, elapsed time and rate to the lock screen and Control Center.
fn update_now_playing() {
    let Some((elapsed, rate)) = with_player(|p| unsafe { (p.currentTime().seconds(), p.rate()) })
    else {
        return;
    };
    let title = NSString::from_str("l-space iOS audio spike");
    let elapsed = NSNumber::new_f64(if elapsed.is_finite() { elapsed } else { 0.0 });
    let rate = NSNumber::new_f32(rate);
    let keys: [&NSString; 3] = unsafe {
        [
            MPMediaItemPropertyTitle,
            MPNowPlayingInfoPropertyElapsedPlaybackTime,
            MPNowPlayingInfoPropertyPlaybackRate,
        ]
    };
    let values: [&AnyObject; 3] = [&title, &elapsed, &rate];
    let info = NSDictionary::<NSString, AnyObject>::from_slices(&keys, &values);
    unsafe { MPNowPlayingInfoCenter::defaultCenter().setNowPlayingInfo(Some(&info)) };
}
