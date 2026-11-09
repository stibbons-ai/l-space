//! The playback rules that need no I/O (PRD "Modules"): where a position in a whole Item
//! falls among its audio files and back ([`Files`]), and where the previous and next chapter
//! start ([`Chapters`]). No async, and nothing of Audiobookshelf, Sonos, the App API or
//! storage: callers convert to and from these plain times.
//!
//! Positions are [`Duration`]s from the start of the whole Item.

use std::time::Duration;

/// How far into a chapter a previous chapter restarts it rather than going to the chapter
/// before, allowing a second tap near the start to go back again.
pub const RESTART_GRACE: Duration = Duration::from_secs(3);

/// An Item's audio files in playing order, by their lengths: where in the whole Item each one
/// starts, and in which a whole-Item position falls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Files {
    /// Where each file ends in the whole Item; never empty.
    ends: Vec<Duration>,
}

/// A position within one of an Item's audio files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilePosition {
    /// The file, 0-based, in playing order.
    pub file: usize,
    /// How far into that file.
    pub within: Duration,
}

impl Files {
    /// The files of the given `lengths`, in playing order; none without a file.
    pub fn new(lengths: impl IntoIterator<Item = Duration>) -> Option<Self> {
        let mut end = Duration::ZERO;
        let ends: Vec<Duration> = lengths
            .into_iter()
            .map(|length| {
                end = end.saturating_add(length);
                end
            })
            .collect();
        (!ends.is_empty()).then_some(Self { ends })
    }

    /// How many files there are.
    pub fn count(&self) -> usize {
        self.ends.len()
    }

    /// The whole Item's length: all its files'.
    pub fn length(&self) -> Duration {
        *self.ends.last().expect("never empty")
    }

    /// Where file `file` (0-based) starts in the whole Item; none past the last file.
    fn start(&self, file: usize) -> Option<Duration> {
        match file {
            0 => Some(Duration::ZERO),
            file if file < self.ends.len() => Some(self.ends[file - 1]),
            _ => None,
        }
    }

    /// The file and the position within it for `position` in the whole Item, kept between
    /// the Item's start and its end. A position exactly where one file ends and the next
    /// begins is the next file's start; the Item's end is the end of its last file.
    pub fn locate(&self, position: Duration) -> FilePosition {
        let position = position.min(self.length());
        let last = self.ends.len() - 1;
        let file = self.ends.partition_point(|end| *end <= position).min(last);
        let start = self.start(file).expect("a file found above");
        FilePosition {
            file,
            within: position - start,
        }
    }

    /// The position in the whole Item for `at`: the file's start plus how far into it; none
    /// for a file past the last. Not kept within the Item: a file may play on past the length
    /// it was told to have, and where it is then is past the end.
    pub fn position(&self, at: FilePosition) -> Option<Duration> {
        let start = self.start(at.file)?;
        Some(start.saturating_add(at.within))
    }

    /// The length of one file, 0-based; none past the last file.
    pub fn file_length(&self, file: usize) -> Option<Duration> {
        Some(*self.ends.get(file)? - self.start(file)?)
    }
}

/// An Item's chapters by where they start, for moving to the previous or next one. An Item
/// without chapters counts as one chapter spanning it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chapters {
    /// Where each chapter starts, ascending, without duplicates; the first is always 0 and
    /// none lies at or past the end, except 0 when the Item has zero length.
    starts: Vec<Duration>,
}

impl Chapters {
    /// The chapters starting at `starts` (any order) in an Item `length` long. A start at or
    /// past the end is left out. The part before its first chapter counts as a chapter.
    pub fn new(starts: impl IntoIterator<Item = Duration>, length: Duration) -> Self {
        let mut starts: Vec<Duration> = starts
            .into_iter()
            .filter(|start| *start < length)
            .chain([Duration::ZERO])
            .collect();
        starts.sort_unstable();
        starts.dedup();
        Self { starts }
    }

    /// Where the previous chapter goes from `position`: the start of the chapter `position`
    /// is in when more than [`RESTART_GRACE`] into it, else the start of the chapter before;
    /// in the first chapter, its start.
    pub fn previous(&self, position: Duration) -> Duration {
        let current = self.current(position);
        let start = self.starts[current];
        if position.saturating_sub(start) > RESTART_GRACE {
            start
        } else {
            self.starts[current.saturating_sub(1)]
        }
    }

    /// Where the next chapter goes from `position`: the start of the chapter after the one
    /// `position` is in; none in the last chapter, so a stray tap cannot end the Item.
    pub fn next(&self, position: Duration) -> Option<Duration> {
        self.starts.get(self.current(position) + 1).copied()
    }

    /// The chapter whose start is at or immediately before `position`.
    /// Playback adapters must round seek targets up (Bridge `ItemFiles::locate`), not add tolerance here.
    fn current(&self, position: Duration) -> usize {
        self.starts.partition_point(|start| *start <= position) - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    /// "The Long Book": 40, 30 and 2 minutes.
    fn long_book() -> Files {
        Files::new([s(2400), s(1800), s(120)]).unwrap()
    }

    fn at(file: usize, within: Duration) -> FilePosition {
        FilePosition { file, within }
    }

    #[test]
    fn no_files_make_no_item() {
        assert_eq!(Files::new([]), None);
    }

    #[test]
    fn each_file_starts_where_the_ones_before_it_end() {
        let book = long_book();
        assert_eq!(book.count(), 3);
        assert_eq!(book.length(), s(4320));
        let starts: Vec<_> = (0..4).map(|file| book.start(file)).collect();
        assert_eq!(starts, [Some(s(0)), Some(s(2400)), Some(s(4200)), None]);
    }

    #[test]
    fn a_position_falls_in_the_file_that_holds_it() {
        let book = long_book();
        assert_eq!(book.locate(s(0)), at(0, s(0)));
        assert_eq!(book.locate(ms(2_399_999)), at(0, ms(2_399_999)));
        assert_eq!(book.locate(s(2405)), at(1, s(5)));
        assert_eq!(book.locate(s(4319)), at(2, s(119)));
    }

    #[test]
    fn a_position_on_a_boundary_is_the_later_files_start() {
        let book = long_book();
        assert_eq!(book.locate(s(2400)), at(1, s(0)));
        assert_eq!(book.locate(s(4200)), at(2, s(0)));
    }

    #[test]
    fn a_position_to_go_to_is_kept_within_the_item() {
        let book = long_book();
        // The end is the end of the last file, not a file after it.
        assert_eq!(book.locate(s(4320)), at(2, s(120)));
        assert_eq!(book.locate(s(99_999)), at(2, s(120)));
    }

    #[test]
    fn a_file_played_past_its_told_length_is_past_the_end_of_the_item() {
        let book = long_book();
        assert_eq!(book.position(at(2, s(500))), Some(s(4700)));
    }

    #[test]
    fn a_position_in_a_file_is_its_start_plus_how_far_in() {
        let book = long_book();
        assert_eq!(book.position(at(0, s(17))), Some(s(17)));
        assert_eq!(book.position(at(1, s(5))), Some(s(2405)));
        assert_eq!(book.position(at(2, s(0))), Some(s(4200)));
        assert_eq!(book.position(at(3, s(0))), None);
        // Round trip, across every boundary.
        for position in [0, 1, 2399, 2400, 2401, 4199, 4200, 4320] {
            let position = s(position);
            assert_eq!(book.position(book.locate(position)), Some(position));
        }
    }

    #[test]
    fn an_empty_file_is_passed_over() {
        let files = Files::new([s(10), s(0), s(5)]).unwrap();
        assert_eq!(files.locate(s(10)), at(2, s(0)));
        assert_eq!(files.locate(s(9)), at(0, s(9)));
    }

    #[test]
    fn a_single_file_is_the_whole_item() {
        let files = Files::new([s(600)]).unwrap();
        assert_eq!(files.locate(s(300)), at(0, s(300)));
        assert_eq!(files.locate(s(600)), at(0, s(600)));
    }

    /// "The Long Book"'s chapters, one per file.
    fn long_chapters() -> Chapters {
        Chapters::new([s(0), s(2400), s(4200)], s(4320))
    }

    #[test]
    fn previous_restarts_a_chapter_more_than_the_grace_into_it() {
        let chapters = long_chapters();
        assert_eq!(chapters.previous(s(2404)), s(2400));
        assert_eq!(chapters.previous(s(3000)), s(2400));
        assert_eq!(chapters.previous(s(4319)), s(4200));
    }

    #[test]
    fn previous_within_the_grace_goes_to_the_chapter_before() {
        let chapters = long_chapters();
        assert_eq!(chapters.previous(s(2400)), s(0));
        assert_eq!(chapters.previous(s(2403)), s(0));
        assert_eq!(chapters.previous(s(4202)), s(2400));
    }

    #[test]
    fn previous_in_the_first_chapter_goes_to_its_start() {
        let chapters = long_chapters();
        assert_eq!(chapters.previous(s(0)), s(0));
        assert_eq!(chapters.previous(s(2)), s(0));
        assert_eq!(chapters.previous(s(500)), s(0));
    }

    #[test]
    fn next_goes_to_the_start_of_the_chapter_after() {
        let chapters = long_chapters();
        assert_eq!(chapters.next(s(0)), Some(s(2400)));
        assert_eq!(chapters.next(s(2398)), Some(s(2400)));
        assert_eq!(chapters.next(s(2399)), Some(s(2400)));
        // On a boundary, the chapter starting there is the current one.
        assert_eq!(chapters.next(s(2400)), Some(s(4200)));
    }

    #[test]
    fn next_in_the_last_chapter_goes_nowhere() {
        let chapters = long_chapters();
        assert_eq!(chapters.next(s(4200)), None);
        assert_eq!(chapters.next(s(4320)), None);
    }

    #[test]
    fn an_item_without_chapters_is_one_chapter() {
        let chapters = Chapters::new([], s(600));
        assert_eq!(chapters.previous(s(300)), s(0));
        assert_eq!(chapters.previous(s(1)), s(0));
        assert_eq!(chapters.next(s(0)), None);
        assert_eq!(chapters.next(s(599)), None);
    }

    #[test]
    fn chapter_readings_before_a_start_stay_in_the_previous_chapter() {
        let chapters = Chapters::new([s(0), ms(123_400)], s(600));
        assert_eq!(chapters.next(s(0)), Some(ms(123_400)));
        assert_eq!(chapters.next(ms(122_399)), Some(ms(123_400)));
        assert_eq!(chapters.next(ms(122_400)), Some(ms(123_400)));
        assert_eq!(chapters.next(s(123)), Some(ms(123_400)));
        assert_eq!(chapters.next(ms(123_399)), Some(ms(123_400)));
        assert_eq!(chapters.previous(s(123)), s(0));
    }

    #[test]
    fn short_chapters_remain_reachable_in_both_directions() {
        let chapters = Chapters::new([s(0), s(100), ms(100_800), s(200)], s(600));
        assert_eq!(chapters.next(ms(99_200)), Some(s(100)));
        assert_eq!(chapters.next(ms(98_900)), Some(s(100)));
        assert_eq!(chapters.next(s(100)), Some(ms(100_800)));
        assert_eq!(chapters.previous(ms(100_800)), s(100));
        assert_eq!(chapters.previous(s(200)), ms(100_800));
    }

    #[test]
    fn file_lengths_are_available_only_for_existing_files() {
        let files = long_book();
        assert_eq!(files.file_length(0), Some(s(2400)));
        assert_eq!(files.file_length(1), Some(s(1800)));
        assert_eq!(files.file_length(2), Some(s(120)));
        assert_eq!(files.file_length(3), None);
        assert_eq!(files.file_length(usize::MAX), None);
    }

    #[test]
    fn the_part_before_the_first_chapter_is_a_chapter_of_its_own() {
        let chapters = Chapters::new([s(30), s(90)], s(600));
        assert_eq!(chapters.previous(s(10)), s(0));
        assert_eq!(chapters.next(s(10)), Some(s(30)));
        assert_eq!(chapters.previous(s(32)), s(0));
    }

    #[test]
    fn chapters_out_of_order_duplicated_or_past_the_end_are_set_straight() {
        let chapters = Chapters::new([s(90), s(0), s(30), s(30), s(600), s(700)], s(600));
        assert_eq!(chapters.next(s(0)), Some(s(30)));
        assert_eq!(chapters.next(s(30)), Some(s(90)));
        // Nothing starts at the end: the last chapter is the one from 90.
        assert_eq!(chapters.next(s(90)), None);
    }
}
