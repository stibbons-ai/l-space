//! What the App sees of a Library Server while browsing: Audiobookshelf's answers, shaped
//! into the App API's types for the session's user.

use std::cmp::Reverse;

use api::{
    AudioFile, Chapter, Episode, EpisodeFilter, HOME_ENTRIES, HomeEntry, Item, ItemId, Library,
    LibraryEntry, LibraryKind, Podcast, Progress,
};

pub fn library(library: abs::Library) -> Library {
    Library {
        id: library.id,
        name: library.name,
        kind: match library.media_type {
            abs::MediaType::Book => LibraryKind::Audiobooks,
            abs::MediaType::Podcast => LibraryKind::Podcasts,
        },
    }
}

/// How a Library lists `item`; nothing for a book without audio (an ebook alone), which
/// is no Item.
pub fn library_entry(item: abs::LibraryItem) -> Option<LibraryEntry> {
    let (title, author) = match item.media {
        abs::Media::Book(book) if !is_audiobook(&book) => return None,
        abs::Media::Book(book) => (book.metadata.title, book.metadata.author_name),
        abs::Media::Podcast(podcast) => (podcast.metadata.title, podcast.metadata.author),
    };
    Some(LibraryEntry {
        id: item.id,
        title,
        author: non_empty(author),
    })
}

/// The podcast `item` with the episodes `filter` picks by `user`'s Progress, newest first
/// (episodes without a publish date last); not found if `item` is an audiobook.
pub fn podcast(
    user: &abs::User,
    item: abs::LibraryItem,
    filter: EpisodeFilter,
) -> Result<Podcast, Missing> {
    let abs::Media::Podcast(media) = item.media else {
        return Err(Missing::NotAPodcast);
    };
    let mut episodes: Vec<_> = media
        .episodes
        .into_iter()
        .map(|episode| Episode {
            progress: progress_in(user, &item.id, Some(&episode.id)),
            duration: episode_duration(&episode),
            id: episode.id,
            title: episode.title,
            published_at: episode.published_at,
        })
        .filter(|episode| filter.passes(episode.progress.as_ref()))
        .collect();
    episodes.sort_by_key(Episode::order);
    Ok(Podcast {
        id: item.id,
        title: media.metadata.title,
        author: non_empty(media.metadata.author),
        filter,
        episodes,
    })
}

/// The Item `id` in `library_item` (an audiobook, or the podcast holding the episode), with
/// `user`'s Progress; or why `library_item` holds no such Item.
pub fn item(user: &abs::User, library_item: abs::LibraryItem, id: ItemId) -> Result<Item, Missing> {
    let progress = item_progress(user, &id);
    Ok(match select(library_item.media, &id)? {
        Selected::Audiobook(book) => Item {
            title: book.metadata.title,
            author: non_empty(book.metadata.author_name),
            podcast: None,
            description: non_empty(book.metadata.description),
            duration: book_duration(&book.tracks, book.duration),
            published_at: None,
            progress,
            chapters: book.chapters.entries.into_iter().map(chapter).collect(),
            audio_files: audio_files(book.tracks),
            id,
        },
        Selected::Episode { podcast, episode } => Item {
            duration: episode_duration(&episode),
            progress,
            title: episode.title,
            author: None,
            podcast: Some(podcast.title),
            description: non_empty(episode.description),
            published_at: episode.published_at,
            chapters: Vec::new(),
            audio_files: audio_files(episode.audio_track),
            id,
        },
    })
}

/// `user`'s Progress in Item `id`; none if they have none.
pub fn item_progress(user: &abs::User, id: &ItemId) -> Option<Progress> {
    progress_in(user, &id.library_item, id.episode.as_deref())
}

/// Home's "continue listening": the Items in `in_progress` that are in one of `libraries`
/// (those the user can see), with `user`'s Progress past the start and not Finished; the one
/// whose Progress changed last first, at most [`HOME_ENTRIES`]. An ebook alone is no Item and
/// is left out, as is a book Audiobookshelf lists only for its ebook progress (at 0 seconds).
/// Also how many entries named no Item in a way the Bridge knows (a podcast without its
/// episode, a book with one), left out too.
pub fn continue_listening(
    user: &abs::User,
    libraries: &[abs::Library],
    in_progress: Vec<abs::InProgress>,
) -> (Vec<HomeEntry>, usize) {
    let mut unexpected = 0;
    let entries = in_progress
        .into_iter()
        .filter(|entry| {
            libraries
                .iter()
                .any(|library| library.id == entry.library_id)
        })
        .filter_map(|entry| {
            let last_update = entry.progress_last_update;
            match in_progress_entry(user, entry) {
                Ok(entry) => Some((last_update, entry?)),
                Err(UnexpectedShape) => {
                    unexpected += 1;
                    None
                }
            }
        })
        .filter(|(_, entry)| {
            entry
                .progress
                .as_ref()
                .is_some_and(|progress| progress.current_time > 0.0)
        });
    let listed = newest_unfinished(entries);
    (listed, unexpected)
}

/// Home's "latest episodes": `episodes` with `user`'s Progress, unless it is Finished; newest
/// publish date first (the date the Library Server added one without it), at most
/// [`HOME_ENTRIES`]. The Library Server picks `episodes` as each Library's newest unfinished by
/// publish date, so an episode without one shows only when its Library has fewer than
/// [`HOME_ENTRIES`] unfinished episodes with one.
pub fn latest_episodes(user: &abs::User, episodes: Vec<abs::RecentEpisode>) -> Vec<HomeEntry> {
    let entries = episodes.into_iter().map(|recent| {
        let newness = recent.episode.published_at.or(recent.added_at);
        let entry = episode_entry(
            user,
            recent.library_item_id,
            recent.podcast.metadata,
            recent.episode,
        );
        (newness, entry)
    });
    newest_unfinished(entries)
}

/// The `entries` whose Progress isn't Finished, the latest time first (those without one
/// last, otherwise in the order given), at most [`HOME_ENTRIES`].
fn newest_unfinished(entries: impl Iterator<Item = (Option<i64>, HomeEntry)>) -> Vec<HomeEntry> {
    let mut entries: Vec<_> = entries
        .filter(|(_, entry)| EpisodeFilter::Unfinished.passes(entry.progress.as_ref()))
        .collect();
    entries.sort_by_key(|(time, _)| Reverse(*time));
    entries.truncate(HOME_ENTRIES);
    entries.into_iter().map(|(_, entry)| entry).collect()
}

/// An Item in progress that names no Item in a way the Bridge knows.
struct UnexpectedShape;

/// How Home lists the audiobook or episode `entry`, with `user`'s Progress; nothing for an
/// ebook alone.
fn in_progress_entry(
    user: &abs::User,
    entry: abs::InProgress,
) -> Result<Option<HomeEntry>, UnexpectedShape> {
    let library_item = entry.library_item.id;
    match (entry.library_item.media, entry.recent_episode) {
        (abs::Media::Book(book), None) if is_audiobook(&book) => {
            let id = ItemId {
                library_item,
                episode: None,
            };
            Ok(Some(HomeEntry {
                progress: item_progress(user, &id),
                id,
                title: book.metadata.title,
                author: non_empty(book.metadata.author_name),
                podcast: None,
                duration: book_duration(&book.tracks, book.duration),
                published_at: None,
            }))
        }
        (abs::Media::Book(_), None) => Ok(None),
        (abs::Media::Podcast(podcast), Some(episode)) => Ok(Some(episode_entry(
            user,
            library_item,
            podcast.metadata,
            episode,
        ))),
        (abs::Media::Book(_), Some(_)) | (abs::Media::Podcast(_), None) => Err(UnexpectedShape),
    }
}

/// How Home lists `episode` of the podcast `podcast` (the library item `library_item`), with
/// `user`'s Progress.
fn episode_entry(
    user: &abs::User,
    library_item: String,
    podcast: abs::PodcastMetadata,
    episode: abs::Episode,
) -> HomeEntry {
    let duration = episode_duration(&episode);
    let id = ItemId {
        library_item,
        episode: Some(episode.id),
    };
    HomeEntry {
        progress: item_progress(user, &id),
        id,
        title: episode.title,
        author: None,
        podcast: non_empty(Some(podcast.title)),
        duration,
        published_at: episode.published_at,
    }
}

/// `user`'s Progress in the audiobook `library_item`, or in the episode `episode` (found by its
/// id alone: Audiobookshelf ids are unique, and it may record episode Progress without the
/// podcast). Should it hold several entries for one, the first counts.
fn progress_in(user: &abs::User, library_item: &str, episode: Option<&str>) -> Option<Progress> {
    user.media_progress
        .entries
        .iter()
        .find(|entry| match episode {
            Some(episode) => entry.episode_id.as_deref() == Some(episode),
            None => {
                entry.episode_id.is_none() && entry.library_item_id.as_deref() == Some(library_item)
            }
        })
        .map(progress)
}

/// The audio file of Item `id` in `library_item`, if it has exactly one; or why not.
pub fn single_audio_file(
    library_item: abs::LibraryItem,
    id: &ItemId,
) -> Result<abs::Track, NoSingleFile> {
    let mut tracks = match select(library_item.media, id).map_err(NoSingleFile::Missing)? {
        Selected::Audiobook(book) => book.tracks,
        Selected::Episode { episode, .. } => episode.audio_track.into_iter().collect(),
    };
    match tracks.len() {
        1 => Ok(tracks.remove(0)),
        files => Err(NoSingleFile::Files(files)),
    }
}

/// Why an Item has no single audio file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoSingleFile {
    /// The library item holds no such Item.
    Missing(Missing),
    /// It has this many audio files.
    Files(usize),
}

/// What an Item is in its library item.
enum Selected {
    Audiobook(abs::Book),
    Episode {
        podcast: abs::PodcastMetadata,
        episode: abs::Episode,
    },
}

/// The Item `id` in `media` (an audiobook, or the podcast holding the episode); or why
/// `media` holds no such Item.
fn select(media: abs::Media, id: &ItemId) -> Result<Selected, Missing> {
    match (media, &id.episode) {
        (abs::Media::Book(book), None) if !is_audiobook(&book) => Err(Missing::EbookAlone),
        (abs::Media::Book(book), None) => Ok(Selected::Audiobook(book)),
        (abs::Media::Book(_), Some(_)) => Err(Missing::EpisodeOfAudiobook),
        (abs::Media::Podcast(_), None) => Err(Missing::PodcastWithoutEpisode),
        (abs::Media::Podcast(podcast), Some(episode_id)) => {
            let episode = podcast
                .episodes
                .into_iter()
                .find(|episode| &episode.id == episode_id)
                .ok_or(Missing::NoSuchEpisode)?;
            Ok(Selected::Episode {
                podcast: podcast.metadata,
                episode,
            })
        }
    }
}

/// Why a library item that exists holds nothing the App asked for: the App answers
/// `not_found` either way, the reason goes to the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    /// A podcast was asked for, and it is an audiobook.
    NotAPodcast,
    /// An audiobook was asked for, and it is a book without audio.
    EbookAlone,
    /// An episode was asked for in an audiobook.
    EpisodeOfAudiobook,
    /// An Item was asked for in a podcast without naming the episode.
    PodcastWithoutEpisode,
    /// The podcast has no episode with that id.
    NoSuchEpisode,
}

/// An audiobook's duration: the sum of its audio files' if each one is known, else unknown.
/// Audiobookshelf's own figure counts an unprobed file as 0 and still includes excluded ones,
/// so it serves only for a book without listed files.
fn book_duration(tracks: &[abs::Track], duration: Option<f64>) -> Option<f64> {
    if tracks.is_empty() {
        return duration;
    }
    tracks.iter().map(|track| track.duration).sum()
}

fn progress(progress: &abs::MediaProgress) -> Progress {
    Progress {
        current_time: progress.current_time,
        finished: progress.is_finished,
    }
}

fn chapter(chapter: abs::Chapter) -> Chapter {
    Chapter {
        title: chapter.title,
        start: chapter.start,
        end: chapter.end,
    }
}

/// An Item's audio files in playing order. A file's start offset is known only while every
/// file before it has a known duration: Audiobookshelf adds them up, counting an unknown one
/// as 0.
fn audio_files(tracks: impl IntoIterator<Item = abs::Track>) -> Vec<AudioFile> {
    let mut offsets_known = true;
    tracks
        .into_iter()
        .map(|track| {
            let start_offset = track.start_offset.filter(|_| offsets_known);
            offsets_known &= track.duration.is_some();
            AudioFile {
                id: track.ino,
                start_offset,
                duration: track.duration,
            }
        })
        .collect()
}

/// Whether a library item listed as a book is an audiobook: not an ebook alone, which is no
/// Item. Only an explicit count of 0 audio files says so.
fn is_audiobook(book: &abs::Book) -> bool {
    book.num_tracks != Some(0)
}

/// An episode's duration in seconds: its own, else its audio file's (the same value on
/// Audiobookshelf 2.37.2; the fallback serves an answer without the episode's own, such as
/// an Item in progress, which has only `audioFile`), else unknown.
fn episode_duration(episode: &abs::Episode) -> Option<f64> {
    episode.duration.or_else(|| {
        episode
            .audio_track
            .iter()
            .chain(&episode.audio_file)
            .find_map(|track| track.duration)
    })
}

/// `text`, unless it is missing or blank.
fn non_empty(text: Option<String>) -> Option<String> {
    text.filter(|text| !text.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use api::{EpisodeFilter, Progress};

    use super::{Missing, continue_listening, item, latest_episodes, library_entry, podcast};

    fn episode(id: &str, published_at: Option<i64>, duration: Option<f64>) -> abs::Episode {
        abs::Episode {
            id: id.into(),
            title: id.into(),
            description: None,
            published_at,
            duration,
            audio_track: None,
            audio_file: None,
        }
    }

    fn the_podcast(episodes: Vec<abs::Episode>) -> abs::LibraryItem {
        abs::LibraryItem {
            id: "li-1".into(),
            media: abs::Media::Podcast(abs::Podcast {
                metadata: abs::PodcastMetadata {
                    title: "Podcast".into(),
                    author: None,
                },
                episodes,
            }),
        }
    }

    fn user(media_progress: Vec<abs::MediaProgress>) -> abs::User {
        abs::User {
            id: "u1".into(),
            username: "alice".into(),
            media_progress: abs::Known {
                entries: media_progress,
                dropped: 0,
            },
        }
    }

    #[test]
    fn episodes_are_newest_first_and_undated_ones_last() {
        let item = the_podcast(vec![
            episode("undated", None, None),
            episode("old", Some(1), None),
            episode("new", Some(3), None),
            episode("middle", Some(2), None),
        ]);
        let listed = podcast(&user(Vec::new()), item, EpisodeFilter::All).unwrap();
        let ids: Vec<_> = listed
            .episodes
            .iter()
            .map(|episode| episode.id.as_str())
            .collect();
        assert_eq!(ids, ["new", "middle", "old", "undated"]);
    }

    #[test]
    fn an_episodes_duration_is_its_own_else_its_audio_files_else_unknown() {
        let mut from_file = episode("file", Some(2), None);
        from_file.audio_track = Some(abs::Track {
            ino: "1".into(),
            start_offset: Some(0.0),
            duration: Some(60.0),
            metadata: None,
        });
        let mut own = episode("own", Some(3), Some(30.0));
        own.audio_track = from_file.audio_track.clone();
        let item = the_podcast(vec![own, from_file, episode("unknown", Some(1), None)]);
        let listed = podcast(&user(Vec::new()), item, EpisodeFilter::All).unwrap();
        let durations: Vec<_> = listed
            .episodes
            .iter()
            .map(|episode| episode.duration)
            .collect();
        assert_eq!(durations, [Some(30.0), Some(60.0), None]);
    }

    /// Audiobookshelf records episode Progress without the podcast when given only the
    /// episode; it still counts, and the first of two entries wins.
    #[test]
    fn episode_progress_is_found_by_the_episode_alone() {
        let entry = |library_item_id: Option<&str>, current_time| abs::MediaProgress {
            library_item_id: library_item_id.map(Into::into),
            episode_id: Some("ep".into()),
            current_time,
            is_finished: false,
        };
        let listed = podcast(
            &user(vec![entry(None, 120.0), entry(Some("li-1"), 5.0)]),
            the_podcast(vec![episode("ep", Some(1), None)]),
            EpisodeFilter::All,
        )
        .unwrap();
        assert_eq!(
            listed.episodes[0].progress,
            Some(Progress {
                current_time: 120.0,
                finished: false
            })
        );
    }

    fn decoded<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
        serde_json::from_value(value).unwrap()
    }

    fn audiobook_id() -> api::ItemId {
        api::ItemId {
            library_item: "li-2".into(),
            episode: None,
        }
    }

    /// Audiobookshelf sends 0 for a duration it doesn't know: from its answer to the App,
    /// that is no duration.
    #[test]
    fn a_zero_duration_is_unknown() {
        let listed = podcast(
            &user(Vec::new()),
            decoded(serde_json::json!({
                "id": "li-1",
                "mediaType": "podcast",
                "media": {"metadata": {"title": "Podcast"}, "episodes": [
                    {"id": "zero-with-file", "title": "a", "publishedAt": 3, "duration": 0,
                     "audioTrack": {"ino": "1", "startOffset": 0, "duration": 60}},
                    {"id": "zero-with-zero-file", "title": "b", "publishedAt": 2, "duration": 0,
                     "audioTrack": {"ino": "2", "startOffset": 0, "duration": 0}},
                    {"id": "zero-alone", "title": "c", "publishedAt": 1, "duration": 0},
                ]},
            })),
            EpisodeFilter::All,
        )
        .unwrap();
        let durations: Vec<_> = listed
            .episodes
            .iter()
            .map(|episode| episode.duration)
            .collect();
        assert_eq!(durations, [Some(60.0), None, None]);

        let book = decoded(serde_json::json!({
            "id": "li-2",
            "mediaType": "book",
            "media": {"metadata": {"title": "Book"}, "duration": 0, "numTracks": 1,
                      "tracks": [{"ino": "1", "startOffset": 0, "duration": 0}]},
        }));
        let shown = item(&user(Vec::new()), book, audiobook_id()).unwrap();
        assert_eq!(shown.duration, None);
        assert_eq!(shown.audio_files[0].duration, None);
    }

    /// Audiobookshelf adds up the files' durations for the offsets, counting an unknown one
    /// as 0: from there on, the offsets are unknown.
    #[test]
    fn offsets_after_a_file_of_unknown_duration_are_unknown() {
        let book = decoded(serde_json::json!({
            "id": "li-2",
            "mediaType": "book",
            "media": {"metadata": {"title": "Book"}, "numTracks": 4, "tracks": [
                {"ino": "1", "startOffset": 0, "duration": 100},
                {"ino": "2", "startOffset": 100, "duration": null},
                {"ino": "3", "startOffset": 100, "duration": 50},
                {"ino": "4", "startOffset": 150, "duration": 30},
            ]},
        }));
        let shown = item(&user(Vec::new()), book, audiobook_id()).unwrap();
        let offsets: Vec<_> = shown
            .audio_files
            .iter()
            .map(|file| file.start_offset)
            .collect();
        assert_eq!(offsets, [Some(0.0), Some(100.0), None, None]);
    }

    /// An audiobook's duration is the sum of its audio files' when each is known: the
    /// Library Server's own figure counts an unprobed file as 0.
    #[test]
    fn an_audiobooks_duration_needs_every_files_duration() {
        let book = |middle: serde_json::Value| {
            decoded(serde_json::json!({
                "id": "li-2",
                "mediaType": "book",
                "media": {"metadata": {"title": "Book"}, "numTracks": 3, "duration": 150,
                          "tracks": [
                              {"ino": "1", "startOffset": 0, "duration": 100},
                              {"ino": "2", "startOffset": 100, "duration": middle},
                              {"ino": "3", "startOffset": 100, "duration": 50},
                          ]},
            }))
        };
        let duration = |book| {
            item(&user(Vec::new()), book, audiobook_id())
                .unwrap()
                .duration
        };
        assert_eq!(duration(book(serde_json::Value::Null)), None);
        assert_eq!(duration(book(serde_json::json!(20))), Some(170.0));
    }

    /// Only an explicit count of no audio files makes a book an ebook alone, which is no Item.
    #[test]
    fn only_a_book_with_no_audio_files_is_left_out() {
        let book = |num_tracks: Option<u64>| {
            let mut media = serde_json::json!({"metadata": {"title": "Book"}});
            if let Some(num_tracks) = num_tracks {
                media["numTracks"] = num_tracks.into();
            }
            decoded(serde_json::json!({"id": "li-2", "mediaType": "book", "media": media}))
        };
        assert!(library_entry(book(Some(0))).is_none());
        assert_eq!(
            item(&user(Vec::new()), book(Some(0)), audiobook_id()),
            Err(Missing::EbookAlone)
        );
        for num_tracks in [None, Some(2)] {
            assert!(library_entry(book(num_tracks)).is_some(), "{num_tracks:?}");
            assert!(
                item(&user(Vec::new()), book(num_tracks), audiobook_id()).is_ok(),
                "{num_tracks:?}"
            );
        }
    }

    #[test]
    fn the_applied_filter_comes_with_the_episodes() {
        for filter in EpisodeFilter::CHOICES {
            let listed = podcast(&user(Vec::new()), the_podcast(Vec::new()), filter).unwrap();
            assert_eq!(listed.filter, filter);
        }
    }

    fn progress_entry(
        library_item: &str,
        episode: Option<&str>,
        current_time: f64,
        is_finished: bool,
    ) -> abs::MediaProgress {
        abs::MediaProgress {
            library_item_id: Some(library_item.into()),
            episode_id: episode.map(Into::into),
            current_time,
            is_finished,
        }
    }

    fn library(id: &str) -> abs::Library {
        abs::Library {
            id: id.into(),
            name: id.into(),
            media_type: abs::MediaType::Book,
        }
    }

    fn ids(entries: &[api::HomeEntry]) -> Vec<(&str, Option<&str>)> {
        entries
            .iter()
            .map(|entry| (entry.id.library_item.as_str(), entry.id.episode.as_deref()))
            .collect()
    }

    /// Audiobookshelf already leaves out Finished Items; one it lists while the user's own
    /// Progress says Finished is left out too, and so is one at 0 seconds (a book read only
    /// as an ebook) or without the user's Progress. Entries that name no Item as the Bridge
    /// knows them are counted.
    #[test]
    fn continue_listening_lists_unfinished_items_the_user_can_see_last_listened_first() {
        let book = |id: &str, library: &str, num_tracks: u64, last: Option<i64>| {
            serde_json::json!({"id": id, "libraryId": library, "mediaType": "book",
                "media": {"metadata": {"title": id, "authorName": "Author"},
                          "numTracks": num_tracks, "duration": 600},
                "progressLastUpdate": last})
        };
        let episode = |id: &str, last: i64| {
            serde_json::json!({"id": "li-p", "libraryId": "lib-2", "mediaType": "podcast",
                "media": {"metadata": {"title": "Podcast"}},
                "recentEpisode": {"id": id, "title": id, "publishedAt": 7,
                                  "audioFile": {"ino": "1", "duration": 900}},
                "progressLastUpdate": last})
        };
        let mut book_with_episode = book("li-odd-book", "lib-1", 1, Some(90));
        book_with_episode["recentEpisode"] = episode("ep-odd", 90)["recentEpisode"].clone();
        let mut podcast_without_episode = episode("ep-none", 95);
        podcast_without_episode["recentEpisode"] = serde_json::Value::Null;
        let in_progress: Vec<abs::InProgress> = decoded(serde_json::json!([
            book("li-a", "lib-1", 1, Some(10)),
            episode("ep-1", 30),
            episode("ep-2", 40),
            book("li-ebook", "lib-1", 0, Some(50)),
            book("li-hidden", "lib-hidden", 1, Some(60)),
            book("li-undated", "lib-1", 1, None),
            book("li-ebook-read", "lib-1", 1, Some(70)),
            book("li-no-progress", "lib-1", 1, Some(80)),
            book_with_episode,
            podcast_without_episode,
        ]));
        let user = user(vec![
            progress_entry("li-a", None, 100.0, false),
            progress_entry("li-p", Some("ep-1"), 50.0, false),
            progress_entry("li-p", Some("ep-2"), 900.0, true),
            progress_entry("li-undated", None, 20.0, false),
            progress_entry("li-ebook-read", None, 0.0, false),
        ]);

        let (listed, unexpected) =
            continue_listening(&user, &[library("lib-1"), library("lib-2")], in_progress);

        assert_eq!(
            ids(&listed),
            [("li-p", Some("ep-1")), ("li-a", None), ("li-undated", None)]
        );
        assert_eq!(unexpected, 2);
        let (episode, book) = (&listed[0], &listed[1]);
        assert_eq!(
            (episode.podcast.as_deref(), episode.author.as_deref()),
            (Some("Podcast"), None)
        );
        assert_eq!(
            (episode.duration, episode.published_at),
            (Some(900.0), Some(7))
        );
        assert_eq!(
            episode.progress,
            Some(Progress {
                current_time: 50.0,
                finished: false
            })
        );
        assert_eq!(
            (
                book.author.as_deref(),
                book.podcast.as_deref(),
                book.duration
            ),
            (Some("Author"), None, Some(600.0))
        );
        assert_eq!(
            listed[2].progress,
            Some(Progress {
                current_time: 20.0,
                finished: false
            })
        );
    }

    #[test]
    fn latest_episodes_are_unfinished_and_newest_first_by_publish_or_added_date() {
        let recent = |id: &str, published_at: Option<i64>, added_at: Option<i64>| {
            serde_json::json!({"libraryItemId": "li-p", "id": id, "title": id,
                "publishedAt": published_at, "addedAt": added_at,
                "podcast": {"metadata": {"title": "Podcast"}}})
        };
        let episodes: Vec<abs::RecentEpisode> = decoded(serde_json::json!([
            recent("old", Some(1), Some(9)),
            recent("undated", None, Some(2)),
            recent("new", Some(3), Some(9)),
            recent("finished", Some(4), Some(9)),
            recent("no-date", None, None),
            {"libraryItemId": "li-q", "id": "untitled", "title": "untitled", "publishedAt": 0,
             "podcast": {"metadata": {"title": null}}},
        ]));
        let user = user(vec![
            progress_entry("li-p", Some("finished"), 0.0, true),
            progress_entry("li-p", Some("old"), 60.0, false),
        ]);

        let listed = latest_episodes(&user, episodes);

        assert_eq!(
            ids(&listed),
            [
                ("li-p", Some("new")),
                ("li-p", Some("undated")),
                ("li-p", Some("old")),
                ("li-q", Some("untitled")),
                ("li-p", Some("no-date")),
            ]
        );
        assert_eq!(listed[1].published_at, None);
        assert_eq!(listed[2].podcast.as_deref(), Some("Podcast"));
        // A podcast without a title names none, rather than an empty one.
        assert_eq!(listed[3].podcast, None);
        assert_eq!(
            listed[2].progress,
            Some(Progress {
                current_time: 60.0,
                finished: false
            })
        );
    }

    #[test]
    fn latest_episodes_are_at_most_twenty_the_newest() {
        let episodes: Vec<abs::RecentEpisode> = decoded(
            (0..25)
                .map(|n| {
                    serde_json::json!({"libraryItemId": "li-p", "id": format!("ep-{n}"),
                        "title": "Episode", "publishedAt": n,
                        "podcast": {"metadata": {"title": "Podcast"}}})
                })
                .collect(),
        );
        let listed = latest_episodes(&user(Vec::new()), episodes);
        assert_eq!(listed.len(), api::HOME_ENTRIES);
        assert_eq!(listed[0].id.episode.as_deref(), Some("ep-24"));
        assert_eq!(listed[19].id.episode.as_deref(), Some("ep-5"));
    }
}
