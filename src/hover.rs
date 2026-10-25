//! Finding what is under the pointer.
//!
//! The app draws the captured frame in the preview, and the recogniser says
//! where every line and every word of it was found. That is enough to say
//! which word the pointer is over, which is what the lookup needs.

use crate::layout::{Line, Word};
use crate::yomitan::Segment;

/// A point in the pixels of the captured frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// Whether the point is inside the box.
fn holds(box_x: f64, box_y: f64, box_width: f64, box_height: f64, point: Point) -> bool {
    point.x >= box_x
        && point.x <= box_x + box_width
        && point.y >= box_y
        && point.y <= box_y + box_height
}

/// The word of the line that the point is over.
///
/// The words of a line are where the recogniser found them, so the point is
/// compared with their boxes.
pub fn word_at(line: &Line, point: Point) -> Option<&Word> {
    line.words
        .iter()
        .find(|word| {
            holds(
                word.bounds.x,
                word.bounds.y,
                word.bounds.width,
                word.bounds.height,
                point,
            )
        })
}

/// The word of the text that the character offset falls in.
///
/// The offset counts characters, the way the text of a widget counts them.
pub fn word_at_offset(segments: &[Segment], offset: usize) -> Option<&Segment> {
    let mut seen = 0usize;
    for segment in segments {
        let length = segment.text.chars().count();
        if offset < seen + length {
            return Some(segment);
        }
        seen += length;
    }
    None
}

/// The line that the point is over.
///
/// The lines are in the order they were read, so the first one that holds
/// the point is the one that was read first, which is the one the reader
/// would be on.
pub fn line_at(lines: &[Line], point: Point) -> Option<&Line> {
    lines.iter().find(|line| {
        holds(
            line.bounds.x,
            line.bounds.y,
            line.bounds.width,
            line.bounds.height,
            point,
        )
    })
}

/// The word the pointer is over, in the frame.
///
/// The point is in the pixels of the preview, and the words are in the
/// pixels of the frame, so the point is put back into the frame's own
/// pixels first.
pub fn word_under(
    lines: &[Line],
    point: Point,
    frame_width: f64,
    frame_height: f64,
    drawn: (f64, f64, f64, f64),
) -> Option<&Word> {
    let (left, top, width, height) = drawn;
    if width <= 0.0 || height <= 0.0 || frame_width <= 0.0 || frame_height <= 0.0 {
        return None;
    }
    let in_frame = Point::new(
        (point.x - left) / width * frame_width,
        (point.y - top) / height * frame_height,
    );
    let line = line_at(lines, in_frame)?;
    word_at(line, in_frame)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Rect, Word};

    fn word(text: &str, x: f64, y: f64, width: f64, height: f64) -> Word {
        Word {
            text: text.to_owned(),
            bounds: Rect {
                x,
                y,
                width,
                height,
            },
            has_space_after: false,
        }
    }

    fn line(text: &str, x: f64, y: f64, width: f64, height: f64, words: Vec<Word>) -> Line {
        Line {
            text: text.to_owned(),
            bounds: Rect {
                x,
                y,
                width,
                height,
            },
            words,
            is_vertical: false,
            is_right_to_left: false,
            is_text: true,
            block_id: 0,
        }
    }

    /// The word under the pointer is the one whose box holds it.
    #[test]
    fn the_word_under_the_pointer_is_found() {
        let line = line(
            "人気",
            10.0,
            20.0,
            100.0,
            24.0,
            vec![word("人気", 10.0, 20.0, 40.0, 24.0)],
        );

        let found = word_at(&line, Point::new(20.0, 30.0)).expect("a word");
        assert_eq!(found.text, "人気");
    }

    /// The word a character falls in is the one that is found.
    #[test]
    fn the_word_under_a_character_is_found() {
        let segments = [
            Segment {
                text: "人気".to_owned(),
                reading: Some("にんき".to_owned()),
            },
            Segment {
                text: "の".to_owned(),
                reading: None,
            },
            Segment {
                text: "おかず".to_owned(),
                reading: None,
            },
        ];

        assert_eq!(word_at_offset(&segments, 0).map(|s| s.text.as_str()), Some("人気"));
        assert_eq!(word_at_offset(&segments, 1).map(|s| s.text.as_str()), Some("人気"));
        assert_eq!(word_at_offset(&segments, 2).map(|s| s.text.as_str()), Some("の"));
        assert_eq!(word_at_offset(&segments, 3).map(|s| s.text.as_str()), Some("おかず"));
        assert!(word_at_offset(&segments, 9).is_none());
    }

    /// A point outside every word is on no word.
    #[test]
    fn a_point_between_the_words_is_on_no_word() {
        let line = line(
            "人気 おかず",
            10.0,
            20.0,
            100.0,
            24.0,
            vec![
                word("人気", 10.0, 20.0, 40.0, 24.0),
                word("おかず", 60.0, 20.0, 40.0, 24.0),
            ],
        );

        assert!(word_at(&line, Point::new(52.0, 30.0)).is_none());
    }

    /// The line under the pointer is the one whose box holds it.
    #[test]
    fn the_line_under_the_pointer_is_found() {
        let lines = vec![
            line("first", 0.0, 0.0, 100.0, 20.0, Vec::new()),
            line("second", 0.0, 40.0, 100.0, 20.0, Vec::new()),
        ];

        let found = line_at(&lines, Point::new(50.0, 50.0)).expect("a line");
        assert_eq!(found.text, "second");
    }

    /// The point is in the pixels of the preview, and the words are in the
    /// pixels of the frame, so the point is put back into the frame first.
    #[test]
    fn a_point_in_the_preview_is_put_back_into_the_frame() {
        let lines = vec![line(
            "人気",
            100.0,
            200.0,
            400.0,
            48.0,
            vec![word("人気", 100.0, 200.0, 160.0, 48.0)],
        )];

        // The frame is 1920 by 1080 and the preview draws it at a quarter of
        // the size, at 50, 25. The word sits at 100, 200 in the frame, which
        // is 95, 81 in the preview.
        let found = word_under(
            &lines,
            Point::new(95.0, 81.0),
            1920.0,
            1080.0,
            (50.0, 25.0, 480.0, 270.0),
        )
        .expect("a word");
        assert_eq!(found.text, "人気");
    }

    /// A preview of nothing cannot be asked what is under the pointer.
    #[test]
    fn a_point_in_an_empty_preview_is_on_no_word() {
        let lines = vec![line("人気", 0.0, 0.0, 10.0, 10.0, Vec::new())];

        assert!(
            word_under(&lines, Point::new(1.0, 1.0), 1920.0, 1080.0, (0.0, 0.0, 0.0, 0.0))
                .is_none()
        );
    }
}
