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

/// A recognized Handoff's state and whole-Item times, without the Group's stream URL.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct ItemPlayback {
    pub state: sonos::TransportState,
    pub position: Option<Duration>,
    pub duration: Option<Duration>,
}

/// File selection and in-file time sent to Sonos for a whole-Item seek.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct FileSeek {
    pub track: Option<usize>,
    pub within: Duration,
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

    /// The files of a Handoff read from storage.
    pub(super) fn stored(found: &store::GroupHandoff) -> Self {
        let files = Self::of(found.stream, &found.active.handoff.files);
        if files.count > 1 && files.layout.is_none() {
            let handoff = &found.active.handoff;
            tracing::warn!(
                group = handoff.group,
                user_id = handoff.started_by.user_id,
                library_item = handoff.library_item,
                episode = handoff.episode,
                "a stored multi-file Handoff has incomplete file lengths"
            );
        }
        files
    }

    /// The Handoff's stream.
    pub(super) fn stream(&self) -> &StreamId {
        &self.stream
    }

    /// The file (0-based) of the Handoff that `playback` plays, if it plays one: its URL is
    /// one of the Handoff's stream's.
    pub(super) fn playing(&self, playback: &sonos::Playback) -> Option<usize> {
        if self.count > 1 && self.layout.is_none() {
            return None;
        }
        let named = stream::stream_in_url(playback.uri.as_deref()?, self.legacy_name.as_deref())?;
        (named.stream == self.stream && named.file < self.count).then_some(named.file)
    }

    /// A recognized reading in whole-Item coordinates. A single file of unknown length
    /// uses the Group's length; another stream is not an Item reading.
    pub(super) fn whole_if_plays(&self, playback: &sonos::Playback) -> Option<ItemPlayback> {
        let file = self.playing(playback)?;
        Some(ItemPlayback {
            state: playback.state,
            position: match &self.layout {
                Some(layout) => playback
                    .position
                    .and_then(|within| layout.position(FilePosition { file, within })),
                None => playback.position,
            },
            duration: self
                .layout
                .as_ref()
                .map(domain::Files::length)
                .or(playback.duration),
        })
    }

    /// Where a recognized Handoff goes for `target` in the whole Item. Always select a file
    /// in a multi-file Item: the Group may have advanced since it was read.
    pub(super) fn seek(&self, target: Duration) -> FileSeek {
        let to = self.locate(target);
        FileSeek {
            track: (self.count > 1).then_some(to.file),
            within: to.within,
        }
    }

    /// Where `position` in the whole Item lies among the files. Round within the file up to
    /// Sonos's whole-second resolution, advancing to the next file when rounding reaches a
    /// non-last file's end, and capping at the last file's length.
    /// Rounding up keeps a chapter target at or past its exact `Chapters::current` boundary.
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
        if length.is_some_and(|length| rounded >= length) && at.file + 1 < self.count {
            return FilePosition {
                file: at.file + 1,
                within: Duration::ZERO,
            };
        }
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
        let whole = files.whole_if_plays(&playing(&token, 1, 5)).unwrap();
        assert_eq!(
            (whole.position, whole.duration),
            (Some(s(2405)), Some(s(4320)))
        );
        // Another stream's, or a file the Handoff doesn't have: not the Handoff's.
        let other = StreamToken::new("cd34".into());
        assert_eq!(files.whole_if_plays(&playing(&other, 1, 5)), None);
        assert_eq!(files.whole_if_plays(&playing(&token, 3, 5)), None);
    }

    #[test]
    fn a_multi_file_seek_always_selects_its_file() {
        let token = StreamToken::new("ab12".into());
        let book = [file("a", Some(s(2400))), file("b", Some(s(1800)))];
        let files = ItemFiles::of(token.hash(), &book);
        assert_eq!(
            files.seek(s(2390)),
            FileSeek {
                track: Some(0),
                within: s(2390)
            }
        );
        assert_eq!(
            files.seek(s(2420)),
            FileSeek {
                track: Some(1),
                within: s(20)
            }
        );
        assert_eq!(
            files.seek(s(2400)),
            FileSeek {
                track: Some(1),
                within: s(0)
            }
        );
    }

    #[test]
    fn a_single_file_of_unknown_length_is_the_whole_item_as_the_group_tells_it() {
        let token = StreamToken::new("ab12".into());
        let files = ItemFiles::of(token.hash(), &[file("a", None)]);
        let read = files.whole_if_plays(&playing(&token, 0, 90)).unwrap();
        assert_eq!((read.position, read.duration), (Some(s(90)), Some(s(1))));
        assert_eq!(
            files.seek(s(120)),
            FileSeek {
                track: None,
                within: s(120)
            }
        );
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
                .whole_if_plays(&playing(&token, to.file, to.within.as_secs()))
                .unwrap()
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
    fn rounding_to_a_file_end_selects_the_next_file() {
        let token = StreamToken::new("ab12".into());
        for (length, last, target) in [(2_400_000, 1800, 2_399_400), (40_050, 30, 40_030)] {
            let files = ItemFiles::of(
                token.hash(),
                &[
                    file("a", Some(Duration::from_millis(length))),
                    file("b", Some(s(last))),
                ],
            );
            assert_eq!(
                files.locate(Duration::from_millis(target)),
                FilePosition {
                    file: 1,
                    within: Duration::ZERO,
                }
            );
            assert_eq!(
                files.locate(s(99_999)),
                FilePosition {
                    file: 1,
                    within: s(last),
                }
            );
        }
    }

    #[test]
    fn a_fractional_last_file_caps_rounding_at_its_length() {
        let token = StreamToken::new("ab12".into());
        let length = Duration::from_millis(120_400);
        let files = ItemFiles::of(
            token.hash(),
            &[file("a", Some(s(40))), file("b", Some(length))],
        );
        for target in [s(40) + length, s(999)] {
            assert_eq!(
                files.locate(target),
                FilePosition {
                    file: 1,
                    within: length
                }
            );
        }
    }

    #[test]
    fn incomplete_stored_multi_file_layouts_are_not_playing() {
        let token = StreamToken::new("ab12".into());
        let files = ItemFiles::of(token.hash(), &[file("a", Some(s(10))), file("b", None)]);
        assert_eq!(files.playing(&playing(&token, 0, 5)), None);
        assert_eq!(files.whole_if_plays(&playing(&token, 1, 5)), None);
        assert_eq!(files.playing(&playing(&token, 2, 5)), None);
    }
}
