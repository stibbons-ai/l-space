//! Where a Handoff's audio files lie in its whole Item (`domain::Files`): what a Group plays
//! of a Handoff, read as a position in the whole Item, and where in which file a position in
//! the whole Item is, to send the Group there.

use std::time::Duration;

use domain::FilePosition;
use store::{HandoffFile, StreamId};

use crate::stream;

/// A Handoff's stream, and where its files lie in its whole Item.
#[derive(Debug, Clone)]
pub(super) struct ItemFiles {
    stream: StreamId,
    /// How many files the Handoff has.
    count: usize,
    /// Where they lie; none unless every file's length is known. Only a Handoff stored before
    /// Handoffs held their files' lengths lacks one: it has a single file, which starts at 0,
    /// its length what the Group tells.
    layout: Option<domain::Files>,
}

impl ItemFiles {
    /// The files `files` of the Handoff whose stream is `stream`.
    pub(super) fn of(stream: StreamId, files: &[HandoffFile]) -> Self {
        let lengths: Option<Vec<Duration>> = files.iter().map(|file| file.duration).collect();
        Self {
            stream,
            count: files.len(),
            layout: lengths.and_then(domain::Files::new),
        }
    }

    /// The file (0-based) of the Handoff that `playback` plays, if it plays one: its URL is
    /// one of the Handoff's stream's.
    pub(super) fn playing(&self, playback: &sonos::Playback) -> Option<usize> {
        let named = stream::stream_in_url(playback.uri.as_deref()?)?;
        (named.stream == self.stream && named.file < self.count).then_some(named.file)
    }

    /// Whether `playback` plays one of the Handoff's files.
    pub(super) fn plays(&self, playback: &sonos::Playback) -> bool {
        self.playing(playback).is_some()
    }

    /// `playback` read in the whole Item when it plays one of the Handoff's files: its
    /// position is the file's start plus the position in the file, its length the Item's, from
    /// its files' lengths (as the Group tells it for a Handoff of one file of unknown length).
    /// Anything else as it is.
    pub(super) fn whole(&self, mut playback: sonos::Playback) -> sonos::Playback {
        let Some(file) = self.playing(&playback) else {
            return playback;
        };
        match &self.layout {
            Some(layout) => {
                playback.position = playback
                    .position
                    .and_then(|within| layout.position(FilePosition { file, within }));
                playback.duration = Some(layout.length());
            }
            // The first file starts at 0: the Group's figures are the Item's.
            None if file == 0 => {}
            None => playback.position = None,
        }
        playback
    }

    /// Where a Group that plays `playback` (as read, not [`ItemFiles::whole`]) goes for
    /// `target` in the whole Item: the file to go to, none when it is the one it plays, and
    /// how far into that file.
    pub(super) fn seek(
        &self,
        playback: &sonos::Playback,
        target: Duration,
    ) -> (Option<usize>, Duration) {
        let to = self.locate(target);
        let track = (self.playing(playback) != Some(to.file)).then_some(to.file);
        (track, to.within)
    }

    /// Where `position` in the whole Item lies among the files (kept within the Item); in the
    /// first file when their lengths aren't known.
    pub(super) fn locate(&self, position: Duration) -> FilePosition {
        match &self.layout {
            Some(layout) => layout.locate(position),
            None => FilePosition {
                file: 0,
                within: position,
            },
        }
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
    fn a_seek_selects_the_file_only_when_it_changes() {
        let token = StreamToken::new("ab12".into());
        let book = [file("a", Some(s(2400))), file("b", Some(s(1800)))];
        let files = ItemFiles::of(token.hash(), &book);
        let in_second = playing(&token, 1, 5);
        assert_eq!(files.seek(&in_second, s(2390)), (Some(0), s(2390)));
        assert_eq!(files.seek(&in_second, s(2420)), (None, s(20)));
        assert_eq!(files.seek(&in_second, s(2400)), (None, s(0)));
    }

    #[test]
    fn a_single_file_of_unknown_length_is_the_whole_item_as_the_group_tells_it() {
        let token = StreamToken::new("ab12".into());
        let files = ItemFiles::of(token.hash(), &[file("a", None)]);
        assert_eq!(files.whole(playing(&token, 0, 90)), playing(&token, 0, 90));
        assert_eq!(files.seek(&playing(&token, 0, 90), s(120)), (None, s(120)));
    }
}
