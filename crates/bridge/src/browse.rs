//! What the App sees of a Library Server while browsing: Audiobookshelf's answers, shaped
//! into the App API's types for the session's user.

use std::cmp::Reverse;
use std::collections::HashMap;

use api::{
    ApiError, AudioFile, Chapter, Episode, EpisodeFilter, Item, ItemId, Library, LibraryEntry,
    LibraryKind, Podcast, Progress,
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

pub fn library_entry(item: abs::LibraryItem) -> LibraryEntry {
    let (title, author) = match item.media {
        abs::Media::Book(book) => (book.metadata.title, book.metadata.author_name),
        abs::Media::Podcast(podcast) => (podcast.metadata.title, podcast.metadata.author),
    };
    LibraryEntry {
        id: item.id,
        title,
        author: non_empty(author),
    }
}

/// The podcast `item` with the episodes `filter` picks by `user`'s Progress, newest first
/// (episodes without a publish date last); not found if `item` is an audiobook.
pub fn podcast(
    user: &abs::User,
    item: abs::LibraryItem,
    filter: EpisodeFilter,
) -> Result<Podcast, ApiError> {
    let abs::Media::Podcast(media) = item.media else {
        return Err(ApiError::NotFound);
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
        episodes,
    })
}

/// The Item `id` in `library_item` (an audiobook, or the podcast holding the episode), with
/// `user`'s Progress; not found if `library_item` has no such Item.
pub fn item(
    user: &abs::User,
    library_item: abs::LibraryItem,
    id: ItemId,
) -> Result<Item, ApiError> {
    let progress_of = ProgressIndex::of(user);
    match (library_item.media, &id.episode) {
        (abs::Media::Book(book), None) => Ok(Item {
            title: book.metadata.title,
            author: non_empty(book.metadata.author_name),
            podcast: None,
            description: non_empty(book.metadata.description),
            duration: book.duration,
            published_at: None,
            progress: progress_of.audiobook(&library_item.id),
            chapters: book.chapters.into_iter().map(chapter).collect(),
            audio_files: book.tracks.into_iter().map(audio_file).collect(),
            id,
        }),
        (abs::Media::Podcast(podcast), Some(episode_id)) => {
            let episode = podcast
                .episodes
                .into_iter()
                .find(|episode| &episode.id == episode_id)
                .ok_or(ApiError::NotFound)?;
            Ok(Item {
                duration: episode_duration(&episode),
                progress: progress_of.episode(&episode.id),
                title: episode.title,
                author: None,
                podcast: Some(podcast.metadata.title),
                description: non_empty(episode.description),
                published_at: episode.published_at,
                chapters: Vec::new(),
                audio_files: episode.audio_track.into_iter().map(audio_file).collect(),
                id,
            })
        }
        _ => Err(ApiError::NotFound),
    }
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
        for entry in &user.media_progress {
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

fn audio_file(track: abs::Track) -> AudioFile {
    AudioFile {
        id: track.ino,
        start_offset: track.start_offset,
        duration: track.duration,
    }
}

/// An episode's duration in seconds: its own, else its audio file's, else unknown.
fn episode_duration(episode: &abs::Episode) -> Option<f64> {
    episode
        .duration
        .or(episode.audio_track.as_ref().map(|track| track.duration))
}

/// `text`, unless it is missing or blank.
fn non_empty(text: Option<String>) -> Option<String> {
    text.filter(|text| !text.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use api::{EpisodeFilter, Progress};

    use super::podcast;

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
            media_progress,
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
            start_offset: 0.0,
            duration: 60.0,
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
}
