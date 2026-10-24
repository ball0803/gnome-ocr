//! Working out the order and the structure of recognised text.
//!
//! The OCR engine reports where each line was found but not the order they
//! should be read in, nor which lines belong together. This works that out
//! from the geometry, using the approach the document analysis literature
//! settled on: group the lines into paragraphs, then cut the page
//! recursively at the widest run of whitespace to recover the reading order
//! and the structure at the same time.
//!
//! Vertical runs, which is how Japanese is often written, are kept apart and
//! read from the right.

/// A rectangle in the pixels of the frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn right(&self) -> f64 {
        self.x + self.width
    }

    pub fn bottom(&self) -> f64 {
        self.y + self.height
    }

    /// The smallest rectangle that holds all of them.
    pub fn union(rects: &[Rect]) -> Option<Rect> {
        let mut iter = rects.iter();
        let first = *iter.next()?;
        let mut left = first.x;
        let mut top = first.y;
        let mut right = first.right();
        let mut bottom = first.bottom();
        for rect in iter {
            left = left.min(rect.x);
            top = top.min(rect.y);
            right = right.max(rect.right());
            bottom = bottom.max(rect.bottom());
        }
        Some(Rect {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        })
    }
}

/// A word, and the spacing that follows it.
#[derive(Debug, Clone)]
pub struct Word {
    pub text: String,
    pub bounds: Rect,
    /// Whether a space follows this word. The engine knows this exactly, so
    /// the words of a line can be put back together without guessing from
    /// the gaps between them.
    pub has_space_after: bool,
}

/// A line of text.
#[derive(Debug, Clone)]
pub struct Line {
    pub text: String,
    pub bounds: Rect,
    pub words: Vec<Word>,
    /// True when the line runs downwards, which is how Japanese is often
    /// written.
    pub is_vertical: bool,
    /// True when the line reads from the right.
    pub is_right_to_left: bool,
    /// Whether the line is text at all, as opposed to a rule or a picture.
    pub is_text: bool,
}

/// A piece of the text, in reading order.
#[derive(Debug, Clone)]
pub enum Block {
    /// A line that introduces what follows it.
    Heading {
        text: String,
        level: u8,
        bounds: Rect,
    },
    /// A run of lines that belong together.
    Paragraph { text: String, bounds: Rect },
}

impl Block {
    pub fn text(&self) -> &str {
        match self {
            Block::Heading { text, .. } | Block::Paragraph { text, .. } => text,
        }
    }

    pub fn bounds(&self) -> Rect {
        match self {
            Block::Heading { bounds, .. } | Block::Paragraph { bounds, .. } => *bounds,
        }
    }
}

/// The text of a frame, in reading order and with its structure.
#[derive(Debug, Default)]
pub struct Layout {
    pub blocks: Vec<Block>,
}

impl Layout {
    /// The blocks grouped under the heading that introduces them.
    pub fn sections(&self) -> Vec<Section> {
        let mut sections: Vec<Section> = Vec::new();
        let mut current = Section::default();
        for block in &self.blocks {
            match block {
                Block::Heading { text, level, .. } => {
                    if current.has_content() {
                        sections.push(current);
                    }
                    current = Section {
                        heading: Some(Heading {
                            text: text.clone(),
                            level: *level,
                        }),
                        paragraphs: Vec::new(),
                    };
                }
                Block::Paragraph { text, .. } => current.paragraphs.push(text.clone()),
            }
        }
        if current.has_content() {
            sections.push(current);
        }
        sections
    }
}

/// A heading and the paragraphs under it.
#[derive(Debug, Default)]
pub struct Section {
    pub heading: Option<Heading>,
    pub paragraphs: Vec<String>,
}

impl Section {
    fn has_content(&self) -> bool {
        self.heading.is_some() || !self.paragraphs.is_empty()
    }
}

/// What introduces a section.
#[derive(Debug, Clone)]
pub struct Heading {
    pub text: String,
    /// 1 is the largest.
    pub level: u8,
}

/// How far the left edge of a line may drift and still be the same
/// paragraph, as a fraction of the height of the text.
const EDGE_TOLERANCE: f64 = 0.6;

/// How far apart two lines may be and still be the same paragraph, as a
/// fraction of the height of the text.
const PARAGRAPH_GAP: f64 = 0.75;

/// How wide a gap has to be before it separates two columns, as a fraction
/// of the height of the text.
const COLUMN_GAP: f64 = 2.0;

/// How far apart two blocks have to be before they are separate sections,
/// as a fraction of the height of the text.
const SECTION_GAP: f64 = 0.9;

/// How deep the cutting may go.
const MAX_DEPTH: usize = 8;

/// A line taller than this many times the usual text is a heading.
const HEADING_RATIO: f64 = 1.25;

/// The levels a heading can have, from the largest.
const HEADING_LEVELS: [(f64, u8); 3] = [(2.0, 1), (1.5, 2), (1.25, 3)];

/// A block of text waiting to be placed.
#[derive(Debug, Clone)]
struct Item {
    block: Block,
    bounds: Rect,
}

/// Works out the order and the structure of the recognised lines.
///
/// When `clump` is set, the lines of a paragraph are joined into one line.
/// Otherwise each keeps its own line.
pub fn analyse(lines: &[Line], clump: bool) -> Layout {
    let text_lines: Vec<&Line> = lines.iter().filter(|line| line.is_text).collect();
    if text_lines.is_empty() {
        return Layout::default();
    }

    let median_height = body_height(
        &text_lines
            .iter()
            .map(|line| line.bounds.height)
            .collect::<Vec<_>>(),
    );

    let (vertical, horizontal): (Vec<&Line>, Vec<&Line>) =
        text_lines.iter().partition(|line| line.is_vertical);

    let mut blocks = order_horizontal(&horizontal, median_height, clump);

    // Vertical runs are read from the right, and come after the horizontal
    // ones.
    blocks.extend(order_vertical(&vertical));

    Layout { blocks }
}

/// The height that most of the lines share, which is the size of ordinary
/// text.
///
/// The middle value would not do: a heading is taller than the text it
/// introduces, so one heading in a short frame would drag the middle up and
/// then the heading would look like ordinary text.
fn body_height(heights: &[f64]) -> f64 {
    if heights.is_empty() {
        return 1.0;
    }
    let mut sorted = heights.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    // The longest run of heights that are within a tenth of each other is
    // the group of ordinary text.
    let mut best_start = 0;
    let mut best_length = 0;
    let mut start = 0;
    for index in 1..=sorted.len() {
        let apart = index == sorted.len()
            || (sorted[index] - sorted[index - 1]) > sorted[index - 1].abs() * 0.1;
        if apart {
            if index - start > best_length {
                best_start = start;
                best_length = index - start;
            }
            start = index;
        }
    }
    let group = &sorted[best_start..best_start + best_length];
    group.iter().sum::<f64>() / group.len() as f64
}

/// Orders the lines that run across, and works out the headings.
fn order_horizontal(lines: &[&Line], median_height: f64, clump: bool) -> Vec<Block> {
    if lines.is_empty() {
        return Vec::new();
    }

    let mut headings: Vec<Item> = Vec::new();
    let mut body: Vec<&Line> = Vec::new();
    for line in lines {
        match heading_level(line.bounds.height, median_height) {
            Some(level) => headings.push(Item {
                block: Block::Heading {
                    text: line.text.trim().to_owned(),
                    level,
                    bounds: line.bounds,
                },
                bounds: line.bounds,
            }),
            None => body.push(line),
        }
    }

    let paragraphs = group_paragraphs(&body, median_height, clump);
    let mut items: Vec<Item> = paragraphs
        .into_iter()
        .map(|paragraph| Item {
            bounds: paragraph.bounds,
            block: Block::Paragraph {
                text: paragraph.text,
                bounds: paragraph.bounds,
            },
        })
        .collect();
    items.extend(headings);

    place_items(items, median_height)
}

/// Orders the lines that run downwards.
fn order_vertical(lines: &[&Line]) -> Vec<Block> {
    // Columns of vertical text are read from the right.
    let mut sorted: Vec<&&Line> = lines.iter().collect();
    sorted.sort_by(|a, b| {
        b.bounds
            .x
            .partial_cmp(&a.bounds.x)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                a.bounds
                    .y
                    .partial_cmp(&b.bounds.y)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });

    sorted
        .into_iter()
        .map(|line| Block::Paragraph {
            text: line.text.trim().to_owned(),
            bounds: line.bounds,
        })
        .collect()
}

/// Puts the items in reading order.
///
/// The items are cut into columns at the widest runs of whitespace, and
/// anything that runs across the whole width is taken out first so that it
/// cannot break the cut, then put back where it belongs.
fn place_items(items: Vec<Item>, median_height: f64) -> Vec<Block> {
    if items.is_empty() {
        return Vec::new();
    }

    let widest = items
        .iter()
        .map(|item| item.bounds.width)
        .fold(0.0f64, |widest, width| widest.max(width));
    let across = widest * CROSS_LAYOUT_SHARE;

    let (spanning, rest): (Vec<Item>, Vec<Item>) = items
        .into_iter()
        .partition(|item| item.bounds.width >= across && widest > 0.0);

    let mut columns = cut_into_columns(&rest, median_height);

    for item in spanning {
        let best = columns
            .iter()
            .enumerate()
            .max_by(|(_, left), (_, right)| {
                x_overlap(left, item.bounds)
                    .partial_cmp(&x_overlap(right, item.bounds))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(index, _)| index);
        match best {
            Some(index) => columns[index].push(item),
            None => columns.push(vec![item]),
        }
    }

    let mut blocks = Vec::new();
    for column in columns {
        let mut column = column;
        column.sort_by(|a, b| {
            a.bounds
                .y
                .partial_cmp(&b.bounds.y)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        blocks.extend(column.into_iter().map(|item| item.block));
    }
    blocks
}

/// How much of the width an item has to cover to count as running across
/// the whole width.
const CROSS_LAYOUT_SHARE: f64 = 0.8;

/// How much two things overlap along the horizontal axis.
fn x_overlap(column: &[Item], bounds: Rect) -> f64 {
    let Some(union) = Rect::union(&column.iter().map(|item| item.bounds).collect::<Vec<_>>())
    else {
        return 0.0;
    };
    let left = union.x.max(bounds.x);
    let right = union.right().min(bounds.right());
    (right - left).max(0.0)
}

/// A run of lines that belong together.
#[derive(Debug, Clone)]
struct Paragraph {
    text: String,
    bounds: Rect,
}

/// Groups the lines into paragraphs.
fn group_paragraphs(lines: &[&Line], median_height: f64, clump: bool) -> Vec<Paragraph> {
    let mut paragraphs: Vec<Paragraph> = Vec::new();
    let mut current: Vec<&Line> = Vec::new();
    let mut current_left: Option<f64> = None;
    let mut previous_bottom: Option<f64> = None;

    for line in lines {
        let continues = match (current_left, previous_bottom) {
            (Some(left), Some(bottom)) => {
                let same_edge = (line.bounds.x - left).abs() <= EDGE_TOLERANCE * median_height;
                let gap = line.bounds.y - bottom;
                same_edge && gap <= PARAGRAPH_GAP * median_height && gap > -median_height
            }
            _ => false,
        };

        if continues {
            current.push(line);
        } else {
            if !current.is_empty() {
                paragraphs.push(make_paragraph(&current, clump));
            }
            current = vec![line];
            current_left = Some(line.bounds.x);
        }
        previous_bottom = Some(line.bounds.bottom());
    }

    if !current.is_empty() {
        paragraphs.push(make_paragraph(&current, clump));
    }
    paragraphs
}

/// Joins a group of lines into one paragraph.
fn make_paragraph(lines: &[&Line], clump: bool) -> Paragraph {
    let mut text = String::new();
    for line in lines {
        if !text.is_empty() {
            text.push(if clump { ' ' } else { '\n' });
        }
        text.push_str(&line_text(line));
    }
    let bounds = Rect::union(&lines.iter().map(|line| line.bounds).collect::<Vec<_>>()).unwrap_or(Rect {
        x: 0.0,
        y: 0.0,
        width: 0.0,
        height: 0.0,
    });
    Paragraph { text, bounds }
}

/// The text of a line, put back together from its words.
///
/// A line that reads from the right is put back together in that order.
fn line_text(line: &Line) -> String {
    if line.words.is_empty() {
        return line.text.trim().to_owned();
    }
    let mut words: Vec<&Word> = line.words.iter().collect();
    if line.is_right_to_left {
        words.sort_by(|a, b| {
            b.bounds
                .x
                .partial_cmp(&a.bounds.x)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    let mut text = String::new();
    for word in words {
        text.push_str(word.text.trim());
        if word.has_space_after {
            text.push(' ');
        }
    }
    text.trim_end().to_owned()
}

/// How big a heading is, if the line is one.
fn heading_level(height: f64, median_height: f64) -> Option<u8> {
    if median_height <= 0.0 {
        return None;
    }
    let ratio = height / median_height;
    if ratio < HEADING_RATIO {
        return None;
    }
    for (limit, level) in HEADING_LEVELS {
        if ratio >= limit {
            return Some(level);
        }
    }
    None
}

/// Cuts the items up recursively to find the reading order.
///
/// Each leaf is one column, and the columns come out in reading order.
fn cut_into_columns(items: &[Item], median_height: f64) -> Vec<Vec<Item>> {
    let mut columns = Vec::new();
    cut(items, median_height, &mut columns, 0);
    columns
}

/// One step of the cutting.
fn cut(items: &[Item], median_height: f64, columns: &mut Vec<Vec<Item>>, depth: usize) {
    if items.is_empty() {
        return;
    }
    if depth >= MAX_DEPTH {
        columns.push(items.to_vec());
        return;
    }

    let column_gap = widest_gap(items, Axis::X).filter(|gap| gap.0 >= COLUMN_GAP * median_height);
    let section_gap =
        widest_gap(items, Axis::Y).filter(|gap| gap.0 >= SECTION_GAP * median_height);

    // A gap between columns is worth more than a gap between sections, so
    // the columns are looked for first.
    match (column_gap, section_gap) {
        (Some((_, position)), Some((_, other))) => {
            if position <= other {
                split(items, position, Axis::X, median_height, columns, depth);
            } else {
                split(items, other, Axis::Y, median_height, columns, depth);
            }
        }
        (Some((_, position)), None) => split(items, position, Axis::X, median_height, columns, depth),
        (None, Some((_, position))) => split(items, position, Axis::Y, median_height, columns, depth),
        (None, None) => columns.push(items.to_vec()),
    }
}

/// Splits the items in two along an axis.
fn split(
    items: &[Item],
    position: f64,
    axis: Axis,
    median_height: f64,
    columns: &mut Vec<Vec<Item>>,
    depth: usize,
) {
    let (first, second): (Vec<Item>, Vec<Item>) = items
        .iter()
        .cloned()
        .partition(|item| {
            let middle = match axis {
                Axis::X => item.bounds.x + item.bounds.width / 2.0,
                Axis::Y => item.bounds.y + item.bounds.height / 2.0,
            };
            middle < position
        });
    cut(&first, median_height, columns, depth + 1);
    cut(&second, median_height, columns, depth + 1);
}

/// The axis a gap is measured along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    X,
    Y,
}

/// The widest gap between the blocks along an axis, and where it is.
fn widest_gap(items: &[Item], axis: Axis) -> Option<(f64, f64)> {
    let mut spans: Vec<(f64, f64)> = items
        .iter()
        .map(|item| match axis {
            Axis::X => (item.bounds.x, item.bounds.right()),
            Axis::Y => (item.bounds.y, item.bounds.bottom()),
        })
        .collect();
    if spans.is_empty() {
        return None;
    }
    spans.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut best = 0.0;
    let mut best_position = 0.0;
    let mut furthest = spans[0].1;
    for &(start, end) in &spans[1..] {
        if start > furthest {
            let gap = start - furthest;
            if gap > best {
                best = gap;
                best_position = furthest + gap / 2.0;
            }
        }
        furthest = furthest.max(end);
    }

    if best > 0.0 {
        Some((best, best_position))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(text: &str, x: f64, y: f64, width: f64, height: f64) -> Word {
        Word {
            text: text.to_owned(),
            bounds: Rect {
                x,
                y,
                width,
                height,
            },
            has_space_after: true,
        }
    }

    fn line(text: &str, x: f64, y: f64, width: f64, height: f64) -> Line {
        Line {
            text: text.to_owned(),
            bounds: Rect {
                x,
                y,
                width,
                height,
            },
            words: vec![word(text, x, y, width, height)],
            is_vertical: false,
            is_right_to_left: false,
            is_text: true,
        }
    }

    fn texts(blocks: &[Block]) -> Vec<&str> {
        blocks.iter().map(Block::text).collect()
    }

    #[test]
    fn a_single_column_keeps_its_order() {
        let lines = vec![
            line("first", 100.0, 100.0, 200.0, 20.0),
            line("second", 100.0, 130.0, 200.0, 20.0),
            line("third", 100.0, 160.0, 200.0, 20.0),
        ];
        let layout = analyse(&lines, true);
        assert_eq!(texts(&layout.blocks), vec!["first second third"]);
    }

    /// With the joining turned off, each line keeps its own line.
    #[test]
    fn the_lines_of_a_paragraph_can_be_kept_apart() {
        let lines = vec![
            line("first", 100.0, 100.0, 200.0, 20.0),
            line("second", 100.0, 130.0, 200.0, 20.0),
        ];
        let layout = analyse(&lines, false);
        assert_eq!(texts(&layout.blocks), vec!["first\nsecond"]);
    }

    #[test]
    fn lines_far_apart_become_separate_paragraphs() {
        let lines = vec![
            line("first", 100.0, 100.0, 200.0, 20.0),
            line("second", 100.0, 400.0, 200.0, 20.0),
        ];
        let layout = analyse(&lines, true);
        assert_eq!(texts(&layout.blocks), vec!["first", "second"]);
    }

    #[test]
    fn two_columns_are_read_one_after_the_other() {
        let lines = vec![
            line("left one", 50.0, 100.0, 150.0, 20.0),
            line("left two", 50.0, 130.0, 150.0, 20.0),
            line("right one", 700.0, 100.0, 150.0, 20.0),
            line("right two", 700.0, 130.0, 150.0, 20.0),
        ];
        let layout = analyse(&lines, true);
        assert_eq!(
            texts(&layout.blocks),
            vec!["left one left two", "right one right two"]
        );
    }

    #[test]
    fn a_gap_makes_two_paragraphs() {
        let lines = vec![
            line("one", 100.0, 100.0, 200.0, 20.0),
            line("two", 100.0, 128.0, 200.0, 20.0),
            line("three", 100.0, 300.0, 200.0, 20.0),
        ];
        let layout = analyse(&lines, true);
        assert_eq!(texts(&layout.blocks).len(), 2);
        assert!(layout.blocks[0].text().contains("one"));
        assert!(layout.blocks[1].text().contains("three"));
    }

    #[test]
    fn a_taller_line_becomes_a_heading_and_starts_a_section() {
        let lines = vec![
            line("A Heading", 100.0, 100.0, 400.0, 50.0),
            line("body text", 100.0, 170.0, 200.0, 20.0),
        ];
        let layout = analyse(&lines, true);
        assert!(matches!(layout.blocks[0], Block::Heading { level: 1, .. }));
        let sections = layout.sections();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading.as_ref().unwrap().text, "A Heading");
        assert_eq!(sections[0].paragraphs, vec!["body text"]);
    }

    #[test]
    fn vertical_text_is_read_from_the_right() {
        let mut left = line("left column", 100.0, 100.0, 30.0, 200.0);
        left.is_vertical = true;
        let mut right = line("right column", 500.0, 100.0, 30.0, 200.0);
        right.is_vertical = true;

        let layout = analyse(&[left, right], true);
        assert_eq!(texts(&layout.blocks), vec!["right column", "left column"]);
    }

    #[test]
    fn words_are_put_back_together_using_the_spacing_the_engine_reports() {
        let mut first = word("Hello", 10.0, 10.0, 50.0, 20.0);
        first.has_space_after = true;
        let mut second = word("World", 70.0, 10.0, 50.0, 20.0);
        second.has_space_after = false;

        let mut text_line = line("", 10.0, 10.0, 110.0, 20.0);
        text_line.words = vec![first, second];

        let layout = analyse(&[text_line], true);
        assert_eq!(layout.blocks[0].text(), "Hello World");
    }

    #[test]
    fn lines_that_are_not_text_are_left_out() {
        let mut rule = line("|", 10.0, 10.0, 400.0, 2.0);
        rule.is_text = false;
        let lines = vec![rule, line("real text", 10.0, 40.0, 200.0, 20.0)];

        let layout = analyse(&lines, true);
        assert_eq!(texts(&layout.blocks), vec!["real text"]);
    }

    #[test]
    fn nothing_gives_an_empty_layout() {
        let layout = analyse(&[], true);
        assert!(layout.blocks.is_empty());
        assert!(layout.blocks.is_empty());
    }
}
