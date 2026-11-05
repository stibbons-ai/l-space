//! What the App sees of a Library Server while browsing: Audiobookshelf's answers, shaped
//! into the App API's types for the session's user.

use std::cmp::Reverse;
use std::collections::HashMap;

use api::{
    AudioFile, Chapter, Episode, EpisodeFilter, Item, ItemId, Library, LibraryEntry, LibraryKind,
    Podcast, Progress,
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
    let progress_of = ProgressIndex::of(user);
    let mut episodes: Vec<_> = media
        .episodes
        .into_iter()
        .map(|episode| Episode {
            progress: progress_of.episode(&episode.id),
            duration: episode_duration(&episode),
            id: episode.id,
            title: episode.title,
            published_at: episode.published_at,
        })
        .filter(|episode| filter.passes(episode.progress.as_ref()))
        .collect();
    // Newest first; `None` sorts below every date.
    episodes.sort_by_key(|episode| Reverse(episode.published_at));
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
    let progress_of = ProgressIndex::of(user);
    match (library_item.media, &id.episode) {
        (abs::Media::Book(book), None) if !is_audiobook(&book) => Err(Missing::EbookAlone),
        (abs::Media::Book(book), None) => Ok(Item {
            title: book.metadata.title,
            author: non_empty(book.metadata.author_name),
            podcast: None,
            description: non_empty(book.metadata.description),
            duration: book_duration(&book.tracks, book.duration),
            published_at: None,
            progress: progress_of.audiobook(&library_item.id),
            chapters: book.chapters.entries.into_iter().map(chapter).collect(),
            audio_files: audio_files(book.tracks),
            id,
        }),
        (abs::Media::Book(_), Some(_)) => Err(Missing::EpisodeOfAudiobook),
        (abs::Media::Podcast(_), None) => Err(Missing::PodcastWithoutEpisode),
        (abs::Media::Podcast(podcast), Some(episode_id)) => {
            let episode = podcast
                .episodes
                .into_iter()
                .find(|episode| &episode.id == episode_id)
                .ok_or(Missing::NoSuchEpisode)?;
            Ok(Item {
                duration: episode_duration(&episode),
                progress: progress_of.episode(&episode.id),
                title: episode.title,
                author: None,
                podcast: Some(podcast.metadata.title),
                description: non_empty(episode.description),
                published_at: episode.published_at,
                chapters: Vec::new(),
                audio_files: audio_files(episode.audio_track),
                id,
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

/// A user's Progress by audiobook and by episode. An episode is found by its own id alone:
/// Audiobookshelf ids are unique, and it may record episode Progress without the podcast.
/// Should it hold several entries for one, the first counts.
struct ProgressIndex<'a> {
    audiobooks: HashMap<&'a str, &'a abs::MediaProgress>,
    episodes: HashMap<&'a str, &'a abs::MediaProgress>,
}

impl<'a> ProgressIndex<'a> {
    fn of(user: &'a abs::User) -> Self {
        let mut index = Self {
            audiobooks: HashMap::new(),
            episodes: HashMap::new(),
        };
        for entry in &user.media_progress.entries {
            if let Some(episode) = &entry.episode_id {
                index.episodes.entry(episode).or_insert(entry);
            } else if let Some(audiobook) = &entry.library_item_id {
                index.audiobooks.entry(audiobook).or_insert(entry);
            }
        }
        index
    }

    fn audiobook(&self, id: &str) -> Option<Progress> {
        self.audiobooks.get(id).map(|entry| progress(entry))
    }

    fn episode(&self, id: &str) -> Option<Progress> {
        self.episodes.get(id).map(|entry| progress(entry))
    }
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
/// Audiobookshelf 2.37.2; the fallback serves an answer without the episode's own), else
/// unknown.
fn episode_duration(episode: &abs::Episode) -> Option<f64> {
    episode.duration.or_else(|| {
        episode
            .audio_track
            .as_ref()
            .and_then(|track| track.duration)
    })
}

/// `text`, unless it is missing or blank.
fn non_empty(text: Option<String>) -> Option<String> {
    text.filter(|text| !text.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use api::{EpisodeFilter, Progress};

    use super::{Missing, item, library_entry, podcast};

    fn episode(id: &str, published_at: Option<i64>, duration: Option<f64>) -> abs::Episode {
        abs::Episode {
            id: id.into(),
            title: id.into(),
            description: None,
            published_at,
            duration,
            audio_track: None,
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

    fn decoded(item: serde_json::Value) -> abs::LibraryItem {
        serde_json::from_value(item).unwrap()
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
}
