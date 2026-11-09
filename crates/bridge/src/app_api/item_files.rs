//! A Handoff's audio files: which of an Item's files a Handoff plays ([`playable_files`]),
//! and where they lie in the whole Item (`domain::Files`): what a Group plays of a Handoff,
//! read as a position in the whole Item, and where in which file a position in the whole
//! Item is, to send the Group there.

use std::time::Duration;

use api::{ApiError, ItemId};
use domain::FilePosition;
use store::{HandoffFile, StreamId};

use super::known_length;
use crate::stream;

/// The files a Handoff of Item `id` (from `library_server`) plays, from the Item's audio
/// files `tracks` in playing order: each named, in a format a Group plays
/// ([`sonos::media_type`]), with its length when the Library Server knows it.
/// [`ApiError::ItemNotPlayable`] (logged) for an Item without an audio file, with one in
/// another format or unnamed, or with more than one and any of unknown length: the Bridge
/// can't place a position in the whole Item without them.
pub(super) fn playable_files(
    library_server: &str,
    id: &ItemId,
    tracks: Vec<abs::Track>,
) -> Result<Vec<HandoffFile>, ApiError> {
    let not_playable = |reason: &str| {
        tracing::info!(
            library_server,
            library_item = id.library_item,
            episode = id.episode,
            reason,
            "an Item can't be played on a Group"
        );
        ApiError::ItemNotPlayable
    };
    if tracks.is_empty() {
        return Err(not_playable("it has no audio file"));
    }
    let several = tracks.len() > 1;
    tracks
        .into_iter()
        .map(|track| {
            let name = track
                .metadata
                .map(|metadata| metadata.filename)
                .filter(|name| sonos::media_type(name).is_some())
                .ok_or_else(|| {
                    not_playable("an audio file in a format Sonos doesn't play, or unnamed")
                })?;
            let duration = known_length(track.duration)
                .and_then(|length| Duration::try_from_secs_f64(length).ok());
            if several && duration.is_none() {
                return Err(not_playable("one of its audio files has no known length"));
            }
            Ok(HandoffFile {
                ino: track.ino,
                name,
                duration,
            })
        })
        .collect()
}

/// A Handoff's stream, and where its files lie in its whole Item.
#[derive(Debug, Clone)]
pub(super) struct ItemFiles {
    stream: StreamId,
    count: usize,
    /// The single file's percent-encoded legacy path segment.
    legacy_name: Option<String>,
    /// Where they lie; none unless every file's length is known, which only a Handoff of a
    /// single file lacks ([`playable_files`]; also one stored before Handoffs held their
    /// files' lengths): that file starts at 0, its length what the Group tells.
    layout: Option<domain::Files>,
}

impl ItemFiles {
    /// The files `files` of the Handoff whose stream is `stream`.
    pub(super) fn of(stream: StreamId, files: &[HandoffFile]) -> Self {
        let lengths: Option<Vec<Duration>> = files.iter().map(|file| file.duration).collect();
        Self {
            stream,
            count: files.len(),
            legacy_name: (files.len() == 1).then(|| stream::path_segment(&files[0].name)),
            layout: lengths.and_then(domain::Files::new),
        }
    }

    /// The Handoff's stream.
    pub(super) fn stream(&self) -> &StreamId {
        &self.stream
    }

    /// The file (0-based) of the Handoff that `playback` plays, if it plays one: its URL is
    /// one of the Handoff's stream's.
    pub(super) fn playing(&self, playback: &sonos::Playback) -> Option<usize> {
        let named = stream::stream_in_url(playback.uri.as_deref()?, self.legacy_name.as_deref())?;
        (named.stream == self.stream && named.file < self.count).then_some(named.file)
    }

    /// Whether `playback` plays one of the Handoff's files.
    pub(super) fn plays(&self, playback: &sonos::Playback) -> bool {
        self.playing(playback).is_some()
    }

    /// `playback` read in the whole Item when it plays one of the Handoff's files: its
    /// position is the file's start plus the position in the file, its length the Item's, from
    /// its files' lengths (as the Group tells them for a Handoff of one file of unknown
    /// length). Anything else as it is.
    pub(super) fn whole(&self, mut playback: sonos::Playback) -> sonos::Playback {
        let (Some(file), Some(layout)) = (self.playing(&playback), &self.layout) else {
            return playback;
        };
        playback.position = playback
            .position
            .and_then(|within| layout.position(FilePosition { file, within }));
        playback.duration = Some(layout.length());
        playback
    }

    /// Where the Group goes for `target` in the whole Item. Always select a file in a
    /// multi-file Item: the Group may have advanced since `playback` was read.
    pub(super) fn seek(
        &self,
        playback: &sonos::Playback,
        target: Duration,
    ) -> (Option<usize>, Duration) {
        let to = self.locate(target);
        let track = (self.count > 1 || self.playing(playback) != Some(to.file)).then_some(to.file);
        (track, to.within)
    }

    /// Where `position` in the whole Item lies among the files. Round within the file up to
    /// Sonos's whole-second resolution, capped at its length, rather than landing before a
    /// chapter or saved Progress.
    pub(super) fn locate(&self, position: Duration) -> FilePosition {
        let mut at = match &self.layout {
            Some(layout) => layout.locate(position),
            None => FilePosition {
                file: 0,
                within: position,
            },
        };
        let rounded = Duration::from_secs(
            at.within
                .as_secs()
                .saturating_add(u64::from(at.within.subsec_nanos() > 0)),
        );
        let length = self
            .layout
            .as_ref()
            .and_then(|layout| layout.file_length(at.file));
        at.within = length.map_or(rounded, |length| rounded.min(length));
        at
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::StreamToken;

    fn s(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    fn file(name: &str, duration: Option<Duration>) -> HandoffFile {
        HandoffFile {
            ino: name.into(),
            name: format!("{name}.mp3"),
            duration,
        }
    }

    fn playing(token: &StreamToken, file: usize, position: u64) -> sonos::Playback {
        sonos::Playback {
            state: sonos::TransportState::Playing,
            uri: Some(format!("http://127.0.0.1:9{}", token.path(file, "x.mp3"))),
            position: Some(s(position)),
            duration: Some(s(1)),
        }
    }

    #[test]
    fn a_groups_place_in_a_file_is_read_in_the_whole_item() {
        let token = StreamToken::new("ab12".into());
        let book = [
            file("a", Some(s(2400))),
            file("b", Some(s(1800))),
            file("c", Some(s(120))),
        ];
        let files = ItemFiles::of(token.hash(), &book);
        let whole = files.whole(playing(&token, 1, 5));
        assert_eq!(
            (whole.position, whole.duration),
            (Some(s(2405)), Some(s(4320)))
        );
        assert_eq!(files.playing(&whole), Some(1));
        // Another stream's, or a file the Handoff doesn't have: not the Handoff's.
        let other = StreamToken::new("cd34".into());
        assert!(!files.plays(&playing(&other, 1, 5)));
        assert_eq!(files.whole(playing(&other, 1, 5)), playing(&other, 1, 5));
        assert!(!files.plays(&playing(&token, 3, 5)));
    }

    #[test]
    fn a_multi_file_seek_always_selects_its_file() {
        let token = StreamToken::new("ab12".into());
        let book = [file("a", Some(s(2400))), file("b", Some(s(1800)))];
        let files = ItemFiles::of(token.hash(), &book);
        let in_second = playing(&token, 1, 5);
        assert_eq!(files.seek(&in_second, s(2390)), (Some(0), s(2390)));
        assert_eq!(files.seek(&in_second, s(2420)), (Some(1), s(20)));
        assert_eq!(files.seek(&in_second, s(2400)), (Some(1), s(0)));
    }

    #[test]
    fn a_single_file_of_unknown_length_is_the_whole_item_as_the_group_tells_it() {
        let token = StreamToken::new("ab12".into());
        let files = ItemFiles::of(token.hash(), &[file("a", None)]);
        assert_eq!(files.whole(playing(&token, 0, 90)), playing(&token, 0, 90));
        assert_eq!(files.seek(&playing(&token, 0, 90), s(120)), (None, s(120)));
    }

    #[test]
    fn fractional_chapters_advance_and_restart_from_paused_readbacks() {
        let token = StreamToken::new("ab12".into());
        let start = Duration::from_millis(2_400_052);
        let middle = Duration::from_millis(3_000_400);
        let files = ItemFiles::of(
            token.hash(),
            &[file("a", Some(start)), file("b", Some(s(1800)))],
        );
        let chapters = domain::Chapters::new([Duration::ZERO, start, middle], s(4200));
        let readback = |target| {
            let to = files.locate(target);
            files
                .whole(playing(&token, to.file, to.within.as_secs()))
                .position
                .unwrap()
        };
        let second = readback(chapters.next(Duration::ZERO).unwrap());
        assert!(second >= start);
        let third = readback(chapters.next(second).unwrap());
        assert!(third >= middle);
        assert_eq!(chapters.next(third), None);
        assert_eq!(chapters.previous(third), start);
        assert_eq!(chapters.previous(readback(start)), Duration::ZERO);
        assert_eq!(chapters.previous(third + s(4)), middle);
        let resumed = readback(Duration::from_millis(3_000_900));
        assert!(resumed >= Duration::from_millis(3_000_900));
        assert!(resumed < Duration::from_millis(3_001_900));
    }

    #[test]
    fn unknown_stored_lengths_do_not_hide_later_files() {
        let token = StreamToken::new("ab12".into());
        let files = ItemFiles::of(token.hash(), &[file("a", Some(s(10))), file("b", None)]);
        assert_eq!(files.playing(&playing(&token, 1, 5)), Some(1));
        assert_eq!(files.playing(&playing(&token, 2, 5)), None);
    }
}
