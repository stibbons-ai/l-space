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

/// The podcast `item` with the episodes `filter` picks by `user`'s Progress, newest first;
/// not found if `item` is an audiobook.
pub fn podcast(
    user: &abs::User,
    item: abs::LibraryItem,
    filter: EpisodeFilter,
) -> Result<Podcast, ApiError> {
    let abs::Media::Podcast(media) = item.media else {
        return Err(ApiError::NotFound);
    };
    // The first entry per episode, as `progress_entry` finds it.
    let mut progress_by_episode = HashMap::new();
    for entry in &user.media_progress {
        if entry.library_item_id == item.id
            && let Some(episode) = &entry.episode_id
        {
            progress_by_episode.entry(episode.as_str()).or_insert(entry);
        }
    }
    let mut episodes: Vec<_> = media
        .episodes
        .into_iter()
        .map(|episode| Episode {
            progress: progress_by_episode
                .get(episode.id.as_str())
                .map(|entry| progress(entry)),
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

/// `user`'s Progress entry for the audiobook `library_item_id`, or for its episode
/// `episode_id`: the first, should Audiobookshelf hold several.
fn progress_entry<'a>(
    user: &'a abs::User,
    library_item_id: &str,
    episode_id: Option<&str>,
) -> Option<&'a abs::MediaProgress> {
    user.media_progress.iter().find(|entry| {
        entry.library_item_id == library_item_id && entry.episode_id.as_deref() == episode_id
    })
}

/// The Item `id` in `library_item` (an audiobook, or the podcast holding the episode), with
/// `user`'s Progress; not found if `library_item` has no such Item.
pub fn item(
    user: &abs::User,
    library_item: abs::LibraryItem,
    id: ItemId,
) -> Result<Item, ApiError> {
    let progress = progress_entry(user, &library_item.id, id.episode.as_deref()).map(progress);
    match (library_item.media, &id.episode) {
        (abs::Media::Book(book), None) => Ok(Item {
            title: book.metadata.title,
            author: non_empty(book.metadata.author_name),
            podcast: None,
            description: non_empty(book.metadata.description),
            duration: book.duration,
            published_at: None,
            progress,
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
                title: episode.title,
                author: None,
                podcast: Some(podcast.metadata.title),
                description: non_empty(episode.description),
                published_at: episode.published_at,
                progress,
                chapters: Vec::new(),
                audio_files: episode.audio_track.into_iter().map(audio_file).collect(),
                id,
            })
        }
        _ => Err(ApiError::NotFound),
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

/// An episode's duration in seconds: its own, else its audio file's, else unknown (0).
fn episode_duration(episode: &abs::Episode) -> f64 {
    episode
        .duration
        .or(episode.audio_track.as_ref().map(|track| track.duration))
        .unwrap_or_default()
}

/// `text`, unless it is missing or blank.
fn non_empty(text: Option<String>) -> Option<String> {
    text.filter(|text| !text.trim().is_empty())
}
