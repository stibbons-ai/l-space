//! Every string the App shows a person inside the App, in English and German, from Fluent
//! resources embedded in the binary. The exception are the Info.plist strings (the display
//! name and the local network usage text): they stay English, since dx doesn't generate
//! `InfoPlist.strings`.
//!
//! The language follows the device unless the person picked one in Settings
//! ([`LanguageChoice`]); a device language other than German falls back to English.
//!
//! What the screens say about Items is composed here too, with the words and word order from
//! the same Fluent resources: [`duration`], [`progress`], [`date`], an episode's row
//! ([`episode_summary`]), an Item's facts ([`item_facts`]) and its chapters with their start
//! times ([`chapter_rows`], [`clock`]). So is what the Groups screen says about a Group's
//! playback ([`group_playback`]).

use std::sync::LazyLock;

use api::{Episode, EpisodeFilter, Group, Item, LibraryKind, Progress, TransportState};
use fluent_bundle::concurrent::FluentBundle;
use fluent_bundle::{FluentArgs, FluentResource};
use serde::{Deserialize, Serialize};
use unic_langid::LanguageIdentifier;

/// A language the App is translated into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    English,
    German,
}

impl Language {
    const ALL: [Language; 2] = [Language::English, Language::German];

    /// The language for a device locale such as `de-DE` or `de_AT`: German for any German
    /// locale, otherwise English.
    pub fn for_locale(locale: Option<&str>) -> Self {
        let primary = locale
            .and_then(|locale| locale.split(['-', '_']).next())
            .unwrap_or_default();
        if primary.eq_ignore_ascii_case("de") {
            Self::German
        } else {
            Self::English
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::German => "de",
        }
    }

    fn source(self) -> &'static str {
        match self {
            Self::English => include_str!("../locales/en.ftl"),
            Self::German => include_str!("../locales/de.ftl"),
        }
    }
}

/// The language a person picked in Settings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LanguageChoice {
    /// Follow the device's language.
    #[default]
    System,
    English,
    German,
}

impl LanguageChoice {
    pub const ALL: [LanguageChoice; 3] = [
        LanguageChoice::System,
        LanguageChoice::English,
        LanguageChoice::German,
    ];

    /// The language to show on a device set to `device`.
    pub fn resolve(self, device: Language) -> Language {
        match self {
            Self::System => device,
            Self::English => Language::English,
            Self::German => Language::German,
        }
    }

    /// How Settings names this choice.
    pub fn text(self) -> Text {
        match self {
            Self::System => Text::LanguageSystem,
            Self::English => Text::LanguageEnglish,
            Self::German => Text::LanguageGerman,
        }
    }
}

/// Declares [`Text`] with its Fluent message ids, and [`Text::ALL`] listing every variant.
macro_rules! texts {
    ($($variant:ident => $id:literal,)*) => {
        /// A message the App shows, by its Fluent id.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Text {
            $($variant,)*
        }

        impl Text {
            /// Every message, so a test can check that each language has all of them.
            pub const ALL: &[Text] = &[$(Text::$variant,)*];

            /// The Fluent message id.
            pub fn id(self) -> &'static str {
                match self {
                    $(Text::$variant => $id,)*
                }
            }
        }
    };
}

texts! {
    AppName => "app-name",
    Starting => "starting",
    Busy => "busy",
    BridgeUrlTitle => "bridge-url-title",
    BridgeUrlIntro => "bridge-url-intro",
    BridgeUrlLabel => "bridge-url-label",
    BridgeUrlConnect => "bridge-url-connect",
    LoginTitle => "login-title",
    LoginLibraryServer => "login-library-server",
    LoginApiKey => "login-api-key",
    LoginApiKeyHint => "login-api-key-hint",
    LoginSubmit => "login-submit",
    LoginNoLibraryServers => "login-no-library-servers",
    LoginChangeBridge => "login-change-bridge",
    Bridge => "bridge",
    Retry => "retry",
    Back => "back",
    LibrariesTitle => "libraries-title",
    LibrariesNone => "libraries-none",
    LibraryKindAudiobooks => "library-kind-audiobooks",
    LibraryKindPodcasts => "library-kind-podcasts",
    LibraryEmpty => "library-empty",
    EpisodesUnfinished => "episodes-unfinished",
    EpisodesFinished => "episodes-finished",
    EpisodesAll => "episodes-all",
    EpisodesNone => "episodes-none",
    ItemDuration => "item-duration",
    ItemPublished => "item-published",
    ItemProgress => "item-progress",
    ItemChapters => "item-chapters",
    ProgressFinished => "progress-finished",
    ProgressNotStarted => "progress-not-started",
    ProgressStarted => "progress-started",
    DurationUnderMinute => "duration-under-minute",
    GroupsTitle => "groups-title",
    GroupsNone => "groups-none",
    GroupsRefresh => "groups-refresh",
    GroupPlaying => "group-playing",
    GroupPaused => "group-paused",
    GroupStopped => "group-stopped",
    GroupTransitioning => "group-transitioning",
    GroupStateUnknown => "group-state-unknown",
    SettingsTitle => "settings-title",
    SettingsLibraryServer => "settings-library-server",
    SettingsAccount => "settings-account",
    SettingsLanguage => "settings-language",
    SettingsLogout => "settings-logout",
    LanguageSystem => "language-system",
    LanguageEnglish => "language-english",
    LanguageGerman => "language-german",
    ErrorBridgeUrlInvalid => "error-bridge-url-invalid",
    ErrorBridgeUrlUnreachable => "error-bridge-url-unreachable",
    ErrorSessionLibraryServerNotConfigured => "error-session-library-server-not-configured",
    ErrorStorageLoad => "error-storage-load",
    ErrorStorageStuck => "error-storage-stuck",
    ErrorStorageSave => "error-storage-save",
    ErrorLogoutNotSaved => "error-logout-not-saved",
    ErrorNotABridge => "error-not-a-bridge",
    ErrorBridgeUnreachable => "error-bridge-unreachable",
    ErrorInternal => "error-internal",
    ErrorApiKeyRejected => "error-api-key-rejected",
    ErrorLibraryServerNotConfigured => "error-library-server-not-configured",
    ErrorLibraryServerUnreachable => "error-library-server-unreachable",
    ErrorSessionInvalid => "error-session-invalid",
    ErrorNotFound => "error-not-found",
    ErrorSonosUnreachable => "error-sonos-unreachable",
    ErrorUnknown => "error-unknown",
}

/// Declares [`Phrase`], the messages with variables, with their Fluent message ids and the
/// names of their variables.
macro_rules! phrases {
    ($($variant:ident => $id:literal ($($variable:literal),*),)*) => {
        /// A message with variables, by its Fluent id.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum Phrase {
            $($variant,)*
        }

        impl Phrase {
            /// Every phrase, so a test can check that each language has all of them.
            #[cfg(test)]
            const ALL: &[Phrase] = &[$(Phrase::$variant,)*];

            /// The Fluent message id.
            fn id(self) -> &'static str {
                match self {
                    $(Phrase::$variant => $id,)*
                }
            }

            /// The names of its variables.
            #[cfg(test)]
            fn variables(self) -> &'static [&'static str] {
                match self {
                    $(Phrase::$variant => &[$($variable),*],)*
                }
            }
        }
    };
}

phrases! {
    ProgressLeft => "progress-left" ("duration"),
    DurationHoursMinutes => "duration-hours-minutes" ("hours", "minutes"),
    DurationHours => "duration-hours" ("hours"),
    DurationMinutes => "duration-minutes" ("minutes"),
    Date => "date" ("day", "month", "year"),
}

impl From<&api::ApiError> for Text {
    fn from(error: &api::ApiError) -> Self {
        use api::ApiError;
        match error {
            ApiError::BridgeUnreachable => Self::ErrorBridgeUnreachable,
            ApiError::NotABridge => Self::ErrorNotABridge,
            ApiError::Internal => Self::ErrorInternal,
            ApiError::ApiKeyRejected => Self::ErrorApiKeyRejected,
            ApiError::LibraryServerNotConfigured => Self::ErrorLibraryServerNotConfigured,
            ApiError::LibraryServerUnreachable => Self::ErrorLibraryServerUnreachable,
            ApiError::SessionInvalid => Self::ErrorSessionInvalid,
            ApiError::NotFound => Self::ErrorNotFound,
            ApiError::SonosUnreachable => Self::ErrorSonosUnreachable,
            ApiError::Unknown => Self::ErrorUnknown,
        }
    }
}

/// How a Library's kind is named.
impl From<LibraryKind> for Text {
    fn from(kind: LibraryKind) -> Self {
        match kind {
            LibraryKind::Audiobooks => Self::LibraryKindAudiobooks,
            LibraryKind::Podcasts => Self::LibraryKindPodcasts,
        }
    }
}

/// How an episode filter is named.
impl From<EpisodeFilter> for Text {
    fn from(filter: EpisodeFilter) -> Self {
        match filter {
            EpisodeFilter::Unfinished => Self::EpisodesUnfinished,
            EpisodeFilter::Finished => Self::EpisodesFinished,
            EpisodeFilter::All => Self::EpisodesAll,
        }
    }
}

/// How a Group's transport state is named.
impl From<TransportState> for Text {
    fn from(state: TransportState) -> Self {
        match state {
            TransportState::Playing => Self::GroupPlaying,
            TransportState::Paused => Self::GroupPaused,
            TransportState::Stopped => Self::GroupStopped,
            TransportState::Transitioning => Self::GroupTransitioning,
        }
    }
}

/// One bundle per language, parsed once.
static BUNDLES: LazyLock<[FluentBundle<FluentResource>; 2]> =
    LazyLock::new(|| Language::ALL.map(bundle));

fn bundle(language: Language) -> FluentBundle<FluentResource> {
    let tag: LanguageIdentifier = language.tag().parse().expect("valid language tag");
    let resource = FluentResource::try_new(language.source().to_owned())
        .unwrap_or_else(|(_, errors)| panic!("{} messages: {errors:?}", language.tag()));
    let mut bundle = FluentBundle::new_concurrent(vec![tag]);
    // Unicode isolation marks only matter for right-to-left languages, which the App lacks.
    bundle.set_use_isolating(false);
    bundle
        .add_resource(resource)
        .unwrap_or_else(|errors| panic!("{} messages: {errors:?}", language.tag()));
    bundle
}

/// `text` in `language`. A missing or broken message shows its id, which the completeness test
/// rules out.
pub fn text(language: Language, text: Text) -> String {
    format(language, text.id(), None).unwrap_or_else(|_| text.id().to_owned())
}

/// `phrase` in `language` with `args`; a missing or broken message shows its id, as with
/// [`text`].
fn phrase(language: Language, phrase: Phrase, args: &FluentArgs) -> String {
    format(language, phrase.id(), Some(args)).unwrap_or_else(|_| phrase.id().to_owned())
}

/// A duration of `seconds` as a person reads it, in whole minutes rounded down: "1 h 12 min",
/// "45 min", "2 h", or "under 1 min" for less than a minute.
pub fn duration(language: Language, seconds: f64) -> String {
    // `as` rounds down, and turns a negative or NaN duration into 0.
    let minutes = (seconds / 60.0) as u64;
    let (hours, minutes) = (minutes / 60, minutes % 60);
    let mut args = FluentArgs::new();
    args.set("hours", hours);
    args.set("minutes", minutes);
    match (hours, minutes) {
        (0, 0) => text(language, Text::DurationUnderMinute),
        (0, _) => phrase(language, Phrase::DurationMinutes, &args),
        (_, 0) => phrase(language, Phrase::DurationHours, &args),
        _ => phrase(language, Phrase::DurationHoursMinutes, &args),
    }
}

/// What a person reads about their `progress` in an Item of `duration` seconds: "Finished",
/// "1 h 12 min left", "Not started" without Progress or at 0 seconds, and "Started" when the
/// duration is unknown.
pub fn progress(language: Language, progress: Option<&Progress>, duration: Option<f64>) -> String {
    match (progress, duration) {
        (Some(progress), _) if progress.finished => text(language, Text::ProgressFinished),
        (Some(progress), Some(duration)) if progress.current_time > 0.0 => {
            let mut args = FluentArgs::new();
            args.set(
                "duration",
                self::duration(language, duration - progress.current_time),
            );
            phrase(language, Phrase::ProgressLeft, &args)
        }
        (Some(progress), None) if progress.current_time > 0.0 => {
            text(language, Text::ProgressStarted)
        }
        _ => text(language, Text::ProgressNotStarted),
    }
}

/// The calendar date (in UTC) of `millis` since the Unix epoch: "March 31, 2026" or
/// "31. März 2026".
pub fn date(language: Language, millis: i64) -> String {
    let (year, month, day) = civil_date(millis.div_euclid(86_400_000));
    let mut args = FluentArgs::new();
    args.set("year", year);
    args.set("month", month);
    args.set("day", day);
    phrase(language, Phrase::Date, &args)
}

/// What an episode's row says under its title: "March 31, 2026 · 1 h 6 min left", without
/// the date when it has none.
pub fn episode_summary(language: Language, episode: &Episode) -> String {
    let progress = progress(language, episode.progress.as_ref(), episode.duration);
    match episode.published_at {
        Some(published_at) => format!("{} · {progress}", date(language, published_at)),
        None => progress,
    }
}

/// The facts the Item screen lists, each with its label: the person's Progress, the duration
/// if known, and an episode's publish date.
pub fn item_facts(language: Language, item: &Item) -> Vec<(Text, String)> {
    let mut facts = vec![(
        Text::ItemProgress,
        progress(language, item.progress.as_ref(), item.duration),
    )];
    if let Some(seconds) = item.duration {
        facts.push((Text::ItemDuration, duration(language, seconds)));
    }
    if let Some(published_at) = item.published_at {
        facts.push((Text::ItemPublished, date(language, published_at)));
    }
    facts
}

/// What a Group's row says about its playback: its state, then its track's title and creator
/// where known, such as "Playing · Moby Dick · Herman Melville".
pub fn group_playback(language: Language, group: &Group) -> String {
    let state = group.state.map_or(Text::GroupStateUnknown, Text::from);
    let mut parts = vec![text(language, state)];
    if let Some(track) = &group.now_playing {
        parts.extend(track.title.iter().chain(&track.creator).cloned());
    }
    parts.join(" · ")
}

/// An Item's chapters as the Item screen lists them: each title with its start time.
pub fn chapter_rows(item: &Item) -> Vec<(&str, String)> {
    item.chapters
        .iter()
        .map(|chapter| (chapter.title.as_str(), clock(chapter.start)))
        .collect()
}

/// A point `seconds` into an Item as a clock reading, the same in every language the App has:
/// "1:02:03", or "2:03" under an hour.
pub fn clock(seconds: f64) -> String {
    // `as` rounds down, and turns a negative or NaN time into 0.
    let seconds = seconds as u64;
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// The proleptic Gregorian (year, month, day) of `days` since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil_date(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    // Months counted from March, so the leap day comes last.
    let march_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * march_month + 2) / 5 + 1;
    let month = if march_month < 10 {
        march_month + 3
    } else {
        march_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month as u32, day as u32)
}

/// Message `id` in `language` with `args`, or what is wrong with it.
fn format(language: Language, id: &str, args: Option<&FluentArgs>) -> Result<String, String> {
    let [english, german] = &*BUNDLES;
    let bundle = match language {
        Language::English => english,
        Language::German => german,
    };
    let pattern = bundle
        .get_message(id)
        .and_then(|message| message.value())
        .ok_or("no message")?;
    let mut errors = Vec::new();
    let formatted = bundle.format_pattern(pattern, args, &mut errors);
    if errors.is_empty() {
        Ok(formatted.into_owned())
    } else {
        Err(format!("{errors:?}"))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use api::{ApiError, Episode, Item, ItemId, Progress};
    use fluent_bundle::FluentArgs;

    use super::{
        Language, Phrase, Text, clock, date, duration, episode_summary, format, item_facts,
        progress,
    };

    #[test]
    fn every_message_exists_in_every_language_without_errors() {
        for language in Language::ALL {
            for &text in Text::ALL {
                if let Err(error) = format(language, text.id(), None) {
                    panic!("{language:?} {}: {error}", text.id());
                }
            }
            // A message using a variable the App doesn't pass fails here.
            for &phrase in Phrase::ALL {
                let mut args = FluentArgs::new();
                for &variable in phrase.variables() {
                    args.set(variable, "x");
                }
                if let Err(error) = format(language, phrase.id(), Some(&args)) {
                    panic!("{language:?} {}: {error}", phrase.id());
                }
            }
        }
    }

    #[test]
    fn no_language_has_messages_the_app_does_not_use() {
        let used: BTreeSet<_> = Text::ALL
            .iter()
            .map(|text| text.id())
            .chain(Phrase::ALL.iter().map(|phrase| phrase.id()))
            .collect();
        for language in Language::ALL {
            let resource = fluent_syntax::parser::parse(language.source())
                .unwrap_or_else(|(_, errors)| panic!("{language:?}: {errors:?}"));
            let defined: BTreeSet<_> = resource
                .body
                .iter()
                .filter_map(|entry| match entry {
                    fluent_syntax::ast::Entry::Message(message) => Some(message.id.name),
                    _ => None,
                })
                .collect();
            assert_eq!(defined, used, "{language:?}");
        }
    }

    #[test]
    fn durations_are_whole_hours_and_minutes_rounded_down() {
        for (seconds, english, german) in [
            (0.0, "under 1 min", "unter 1 Min."),
            (59.9, "under 1 min", "unter 1 Min."),
            (-5.0, "under 1 min", "unter 1 Min."),
            (f64::NAN, "under 1 min", "unter 1 Min."),
            (60.0, "1 min", "1 Min."),
            (3599.0, "59 min", "59 Min."),
            (3600.0, "1 h", "1 Std."),
            (3659.0, "1 h", "1 Std."),
            (4320.0, "1 h 12 min", "1 Std. 12 Min."),
            (90_000.0, "25 h", "25 Std."),
            (93_784.0, "26 h 3 min", "26 Std. 3 Min."),
        ] {
            assert_eq!(duration(Language::English, seconds), english, "{seconds}");
            assert_eq!(duration(Language::German, seconds), german, "{seconds}");
        }
    }

    #[test]
    fn progress_reads_as_time_left_finished_or_not_started() {
        let at = |current_time, finished| Progress {
            current_time,
            finished,
        };
        for (progress_made, duration, english, german) in [
            (None, Some(4320.0), "Not started", "Nicht begonnen"),
            (
                Some(at(0.0, false)),
                Some(4320.0),
                "Not started",
                "Nicht begonnen",
            ),
            (
                Some(at(2880.0, false)),
                Some(7200.0),
                "1 h 12 min left",
                "noch 1 Std. 12 Min.",
            ),
            (
                Some(at(3000.0, false)),
                Some(4320.0),
                "22 min left",
                "noch 22 Min.",
            ),
            (
                Some(at(4300.0, false)),
                Some(4320.0),
                "under 1 min left",
                "noch unter 1 Min.",
            ),
            // Audiobookshelf may record a time past the end.
            (
                Some(at(5000.0, false)),
                Some(4320.0),
                "under 1 min left",
                "noch unter 1 Min.",
            ),
            (Some(at(0.0, true)), Some(4320.0), "Finished", "Beendet"),
            (Some(at(100.0, true)), Some(4320.0), "Finished", "Beendet"),
            // Without a duration nothing is "left".
            (Some(at(100.0, false)), None, "Started", "Begonnen"),
            (Some(at(0.0, false)), None, "Not started", "Nicht begonnen"),
            (Some(at(100.0, true)), None, "Finished", "Beendet"),
            (None, None, "Not started", "Nicht begonnen"),
        ] {
            assert_eq!(
                progress(Language::English, progress_made.as_ref(), duration),
                english,
                "{progress_made:?} {duration:?}"
            );
            assert_eq!(
                progress(Language::German, progress_made.as_ref(), duration),
                german,
                "{progress_made:?} {duration:?}"
            );
        }
    }

    #[test]
    fn an_episode_row_reads_date_and_progress() {
        let episode = |published_at, duration, progress| Episode {
            id: "ep".into(),
            title: "Episode".into(),
            duration,
            published_at,
            progress,
        };
        let started = Some(Progress {
            current_time: 300.0,
            finished: false,
        });
        for (episode, english, german) in [
            (
                episode(Some(1_774_915_200_000), Some(900.0), started.clone()),
                "March 31, 2026 · 10 min left",
                "31. März 2026 · noch 10 Min.",
            ),
            (episode(None, None, started), "Started", "Begonnen"),
        ] {
            assert_eq!(episode_summary(Language::English, &episode), english);
            assert_eq!(episode_summary(Language::German, &episode), german);
        }
    }

    #[test]
    fn an_items_facts_leave_out_what_is_unknown() {
        let item = Item {
            id: ItemId {
                library_item: "li".into(),
                episode: Some("ep".into()),
            },
            title: "Episode".into(),
            author: None,
            podcast: Some("Podcast".into()),
            description: None,
            duration: None,
            published_at: None,
            progress: None,
            chapters: Vec::new(),
            audio_files: Vec::new(),
        };
        assert_eq!(
            item_facts(Language::English, &item),
            [(Text::ItemProgress, "Not started".to_owned())]
        );
        let item = Item {
            duration: Some(3960.0),
            published_at: Some(1_774_915_200_000),
            ..item
        };
        assert_eq!(
            item_facts(Language::German, &item),
            [
                (Text::ItemProgress, "Nicht begonnen".to_owned()),
                (Text::ItemDuration, "1 Std. 6 Min.".to_owned()),
                (Text::ItemPublished, "31. März 2026".to_owned()),
            ]
        );
    }

    #[test]
    fn dates_follow_the_language() {
        for (millis, english, german) in [
            (0, "January 1, 1970", "1. Januar 1970"),
            (-1, "December 31, 1969", "31. Dezember 1969"),
            (1_709_164_800_000, "February 29, 2024", "29. Februar 2024"),
            (1_774_915_200_000, "March 31, 2026", "31. März 2026"),
            // The last millisecond of the day is still that day.
            (
                1_774_915_200_000 + 86_399_999,
                "March 31, 2026",
                "31. März 2026",
            ),
            (1_798_675_200_000, "December 31, 2026", "31. Dezember 2026"),
        ] {
            assert_eq!(date(Language::English, millis), english, "{millis}");
            assert_eq!(date(Language::German, millis), german, "{millis}");
        }
    }

    #[test]
    fn clock_readings_have_hours_only_from_an_hour_on() {
        for (seconds, expected) in [
            (0.0, "0:00"),
            (59.9, "0:59"),
            (61.0, "1:01"),
            (2400.0, "40:00"),
            (3600.0, "1:00:00"),
            (4200.0, "1:10:00"),
            (90_061.0, "25:01:01"),
        ] {
            assert_eq!(clock(seconds), expected, "{seconds}");
        }
    }

    /// Every `error-<code>` message, with the [`ApiError`] its code decodes to. `Text::from`
    /// matches every [`ApiError`] exhaustively, so a new code doesn't compile without a message.
    fn api_error_messages() -> Vec<(Text, ApiError)> {
        Text::ALL
            .iter()
            .filter_map(|&text| {
                let code = text.id().strip_prefix("error-")?.replace('-', "_");
                let error: ApiError =
                    serde_json::from_value(serde_json::json!({ "code": code })).ok()?;
                (error != ApiError::Unknown || code == "unknown").then_some((text, error))
            })
            .collect()
    }

    #[test]
    fn every_api_error_code_shows_its_own_message() {
        let messages = api_error_messages();
        for (text, error) in &messages {
            assert_eq!(Text::from(error), *text, "{error:?}");
        }
        // One message per code (ApiError has 10), each different in every language.
        assert_eq!(messages.len(), 10, "{messages:?}");
        for language in Language::ALL {
            let distinct: BTreeSet<_> = messages
                .iter()
                .map(|(text, _)| super::text(language, *text))
                .collect();
            assert_eq!(distinct.len(), messages.len(), "{language:?}");
        }
    }

    #[test]
    fn german_locales_choose_german_and_others_english() {
        for (locale, language) in [
            (Some("de-DE"), Language::German),
            (Some("de_AT"), Language::German),
            (Some("DE"), Language::German),
            (Some("en-US"), Language::English),
            (Some("fr-FR"), Language::English),
            (Some("dex"), Language::English),
            (None, Language::English),
        ] {
            assert_eq!(Language::for_locale(locale), language, "{locale:?}");
        }
    }
}
