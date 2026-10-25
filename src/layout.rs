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
//!
//! # How the thresholds are worked out
//!
//! No threshold here is an absolute size, which is the point of the document
//! spectrum: the tolerances are multipliers of the spacings that the page
//! itself shows. There are two kinds, and they answer different questions.
//!
//! The questions about the text itself are answered against the size of the
//! text. The size of the ordinary text comes from the height that most of
//! the lines share, and the tolerances are proportions of it: how far the
//! left edges of a column may differ, how much space there may be between
//! two lines of a paragraph, and how much the spacing of the entries of a
//! list may change. They are proportions rather than sizes because the same
//! page is read at different sizes, and because the same text is set with
//! different amounts of space between the lines.
//!
//! The question about where one block ends and the next begins is answered
//! against the page itself, from the gaps between the lines, split into a
//! lower and an upper group by Otsu's method, the same thresholding an
//! image histogram uses. That is the one that has to come from the page,
//! because the gutters of a page are its own.
//!
//! The measure of the text, which says how wide a line of running text
//! reaches, is the width of the widest line on the page. An entry of a list
//! is short compared with it.
//!
//! The only number that is not a ratio is how many entries make a list,
//! which is a definition rather than a tolerance: the list detection work
//! says a list is three or more records.

/// The ideographic space, which is how a gap is marked in Japanese.
const IDEOGRAPHIC_SPACE: &str = "\u{3000}";

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
    /// The block the engine's layout model put this line in. The lines of
    /// one block belong to the same paragraph.
    pub block_id: i64,
}


/// The measure of the text: the width of the widest line on the page.
///
/// A line of running text reaches the measure, because it is wrapped to it.
/// An entry of a list does not, so it is short compared with the measure.
fn text_measure(lines: &[&Line]) -> f64 {
    lines
        .iter()
        .map(|line| line.bounds.width)
        .fold(0.0, f64::max)
}

/// How far the left edges of two lines may differ and still count as the
/// same alignment, as a fraction of the height of the text.
///
/// The lines of a column all begin at the same place, to within the width of
/// the characters that begin them and the slack of the recogniser. A line
/// set further along belongs to another column, while a deeper indent is a
/// different level.
const ITEM_ALIGNMENT: f64 = 0.6;

/// The gap between two lines of one paragraph, as a fraction of the height
/// of the text.
///
/// Running text is set with less space between its lines than the lines are
/// tall, and a new paragraph is set with more.
const PARAGRAPH_GAP: f64 = 0.75;

/// How much the spacing between the entries of a list may change, as a
/// fraction of the height of the text.
///
/// The entries of a list are evenly spaced, so the space between them is
/// nearly the same each time.
const ITEM_GAP: f64 = 0.8;

/// How many items a run needs before it counts as a list.
const MIN_LIST_ITEMS: usize = 3;


/// Splits the values into a lower and an upper group, the way Otsu's method
/// splits an image histogram: the threshold that makes the two groups as
/// different from each other as possible.
///
/// This is how the gap between two lines is decided to be a boundary or not,
/// without a threshold chosen by hand.
fn otsu_threshold(values: &[f64]) -> Option<f64> {
    if values.len() < 2 {
        return None;
    }
    let lowest = values.iter().copied().fold(f64::INFINITY, f64::min);
    let highest = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let span = highest - lowest;
    if span <= f64::EPSILON {
        return None;
    }

    const BINS: usize = 64;
    let mut histogram = [0usize; BINS];
    for value in values {
        let bin = (((value - lowest) / span) * (BINS - 1) as f64).round() as usize;
        histogram[bin] += 1;
    }

    let total = values.len();
    let mut total_sum = 0.0;
    for (bin, count) in histogram.iter().enumerate() {
        total_sum += *count as f64 * (bin as f64 + 0.5);
    }

    let mut best_variance = 0.0f64;
    let mut best_bin = 0usize;
    let mut lower_sum = 0.0;
    let mut lower_count = 0usize;
    for (bin, count) in histogram.iter().enumerate() {
        lower_count += count;
        if lower_count == 0 {
            continue;
        }
        let upper_count = total - lower_count;
        if upper_count == 0 {
            break;
        }
        lower_sum += *count as f64 * (bin as f64 + 0.5);
        let lower_mean = lower_sum / lower_count as f64;
        let upper_mean = (total_sum - lower_sum) / upper_count as f64;
        let variance = lower_count as f64 * upper_count as f64 * (lower_mean - upper_mean).powi(2);
        if variance > best_variance {
            best_variance = variance;
            best_bin = bin;
        }
    }

    Some(lowest + (best_bin as f64 + 0.5) * span / (BINS - 1) as f64)
}

/// Finds the runs of blocks that are a list.
///
/// The entries of a menu are separate blocks that share a left edge and sit
/// an even distance apart. The evenness of the spacing is what makes them
/// one list, and it is measured against the size of the text, so it holds
/// whatever the page is scaled to.
fn detect_lists(lines: &[&Line], median_height: f64) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for band in align_bands(lines, median_height) {
        detect_runs(&band, lines, median_height, &mut groups);
    }
    groups
}

/// Puts the lines into bands that share a left edge.
///
/// A column of text, and the entries of a menu, all line up on the left. The
/// entries of two columns sit at the same height as one another, so the bands
/// are what keep the two apart.
fn align_bands(lines: &[&Line], median_height: f64) -> Vec<Vec<usize>> {
    let mut by_edge: Vec<usize> = (0..lines.len()).collect();
    by_edge.sort_by(|a, b| {
        lines[*a]
            .bounds
            .x
            .partial_cmp(&lines[*b].bounds.x)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut bands: Vec<Vec<usize>> = Vec::new();
    let mut band: Vec<usize> = Vec::new();

    for index in by_edge {
        let edge = lines[index].bounds.x;
        let same_band = match band.first() {
            Some(first) => {
                (edge - lines[*first].bounds.x).abs() <= ITEM_ALIGNMENT * median_height
            }
            None => true,
        };
        if same_band {
            band.push(index);
        } else {
            bands.push(std::mem::take(&mut band));
            band.push(index);
        }
    }
    if !band.is_empty() {
        bands.push(band);
    }

    bands
}

/// Finds the evenly spaced runs within one band of aligned lines.
fn detect_runs(
    band: &[usize],
    lines: &[&Line],
    median_height: f64,
    groups: &mut Vec<Vec<usize>>,
) {
    let mut ordered: Vec<usize> = band.to_vec();
    ordered.sort_by(|a, b| {
        lines[*a]
            .bounds
            .y
            .partial_cmp(&lines[*b].bounds.y)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Whether a line is an entry of its own. The engine's layout model knows
    // which lines are separate items and which are the wrapped lines of one
    // paragraph, so it is asked first. Without it, a line that is short
    // compared with the measure of the text is taken to be an entry.
    let engine_grouped = lines.iter().any(|line| line.block_id != 0);
    let measure = text_measure(lines);

    let mut run: Vec<usize> = Vec::new();
    for (position, index) in ordered.iter().enumerate() {
        let line = lines[*index];
        let is_entry = if engine_grouped {
            match position.checked_sub(1).map(|previous| ordered[previous]) {
                Some(previous) => line.block_id != lines[previous].block_id,
                None => true,
            }
        } else {
            line.bounds.width * 2.0 <= measure
        };
        if !is_entry {
            push_group(&mut run, groups);
            continue;
        }
        // The entries of a list are evenly spaced: the space between them
        // may change, but not by more than the text is tall.
        if run.len() >= 2 {
            let previous = lines[run[run.len() - 1]];
            let before = lines[run[run.len() - 2]];
            let now = line.bounds.y - previous.bounds.bottom();
            let was = previous.bounds.y - before.bounds.bottom();
            if (now - was).abs() > ITEM_GAP * median_height {
                push_group(&mut run, groups);
            }
        }
        run.push(*index);
    }
    push_group(&mut run, groups);
}

/// The vertical gaps between the lines of a run, from the top down.
fn gaps_between(run: &[usize], lines: &[&Line]) -> Vec<f64> {
    let mut ordered: Vec<usize> = run.to_vec();
    ordered.sort_by(|a, b| {
        lines[*a]
            .bounds
            .y
            .partial_cmp(&lines[*b].bounds.y)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    ordered
        .windows(2)
        .map(|pair| {
            let upper = lines[pair[0]].bounds.bottom();
            let lower = lines[pair[1]].bounds.y;
            (lower - upper).max(0.0)
        })
        .collect()
}

/// Keeps a run of lines if it is long enough to be a list.
fn push_group(run: &mut Vec<usize>, groups: &mut Vec<Vec<usize>>) {
    if run.len() >= MIN_LIST_ITEMS {
        groups.push(run.clone());
    }
    run.clear();
}

/// Builds a list out of a run of lines.
fn make_list(lines: &[&Line]) -> (Vec<String>, Rect) {
    let items = lines.iter().map(|line| line_text(line)).collect::<Vec<_>>();
    let bounds = Rect::union(&lines.iter().map(|line| line.bounds).collect::<Vec<_>>())
        .unwrap_or(Rect {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
        });
    (items, bounds)
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
    /// A run of short, aligned items that belong together, such as the
    /// entries of a menu.
    List { items: Vec<String>, bounds: Rect },
}

/// A block of text that had not been seen before.
#[derive(Debug, Clone)]
pub struct Fresh {
    /// The block itself.
    pub block: Block,
    /// Whether the page has a large gap between this block and the block
    /// before it, so that it starts a new paragraph.
    ///
    /// This is taken from the page and not worked out from the blocks that
    /// happen to be new, because a block that was already shown still stands
    /// between them.
    pub new_paragraph: bool,
}

impl Block {
    /// The text of the block. The items of a list are shown together, since
    /// they are one unit of the page.
    ///
    /// The entries of a list are apart on the page, so the space that goes
    /// between them is one of ours, and it is the ideographic space, which
    /// is how a gap is marked in Japanese. The spaces that were on the page
    /// are left as they were.
    pub fn text(&self) -> String {
        match self {
            Block::Heading { text, .. } | Block::Paragraph { text, .. } => text.clone(),
            Block::List { items, .. } => items.join(IDEOGRAPHIC_SPACE),
        }
    }

    pub fn bounds(&self) -> Rect {
        match self {
            Block::Heading { bounds, .. }
            | Block::Paragraph { bounds, .. }
            | Block::List { bounds, .. } => *bounds,
        }
    }
}

/// The text of a frame, in reading order and with its structure.
#[derive(Debug, Default)]
pub struct Layout {
    pub blocks: Vec<Block>,
    /// The gap that sets one block apart from the next, worked out from the
    /// page. A gap larger than this is a break between paragraphs.
    pub block_gap: f64,
}

impl Layout {
    /// Whether the block at the given place starts a new paragraph.
    ///
    /// That is the case when the page has a large gap between it and the
    /// block before it.
    pub fn starts_paragraph(&self, index: usize) -> bool {
        if index == 0 || index >= self.blocks.len() {
            return false;
        }
        let previous = self.blocks[index - 1].bounds();
        let current = self.blocks[index].bounds();
        current.y - previous.bottom() > self.block_gap
    }

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
                Block::List { items, .. } => {
                    for item in items {
                        current.paragraphs.push(item.clone());
                    }
                }
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

/// Something with a place on the page.
trait Placed {
    fn bounds(&self) -> Rect;
}

impl Placed for &Line {
    fn bounds(&self) -> Rect {
        self.bounds
    }
}

impl Placed for Item {
    fn bounds(&self) -> Rect {
        self.bounds
    }
}

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
///
/// When `lists` is set, the runs of short, aligned entries are gathered into
/// one list. Otherwise each entry is kept on its own.
pub fn analyse(lines: &[Line], clump: bool, lists: bool) -> Layout {
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

    let (mut blocks, block_gap) = order_horizontal(&horizontal, median_height, clump, lists);

    // Vertical runs are read from the right, and come after the horizontal
    // ones.
    blocks.extend(order_vertical(&vertical));

    Layout { blocks, block_gap }
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
fn order_horizontal(
    lines: &[&Line],
    median_height: f64,
    clump: bool,
    lists: bool,
) -> (Vec<Block>, f64) {
    if lines.is_empty() {
        return (Vec::new(), median_height);
    }

    // The lines of a page arrive in no order at all, and the entries of a
    // menu sit at the same height as the entries of the column beside it.
    // The lists are found from where the lines are, so they are put in place
    // first.
    let mut sorted: Vec<&Line> = lines.to_vec();
    sorted.sort_by(|a, b| {
        a.bounds
            .y
            .partial_cmp(&b.bounds.y)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                a.bounds
                    .x
                    .partial_cmp(&b.bounds.x)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });

    // Where one block ends and the next begins is a property of the page, so
    // the gaps of every band are looked at together and split into a lower
    // and an upper group. Where one block ends and the next begins is what
    // the gaps are compared against.
    let bands = align_bands(&sorted, median_height);
    let page_gaps: Vec<f64> = bands
        .iter()
        .flat_map(|band| gaps_between(band, &sorted))
        .collect();
    let threshold = otsu_threshold(&page_gaps);

    // The runs of short, aligned lines are a list, such as the entries of a
    // menu, and are kept together as one block.
    let groups = if lists {
        detect_lists(&sorted, median_height)
    } else {
        Vec::new()
    };
    let mut listed = vec![false; sorted.len()];
    for group in &groups {
        for index in group {
            listed[*index] = true;
        }
    }

    // An entry of a list is not a heading, even when it is taller than the
    // text around it. The entries of a menu often carry an icon and are
    // taller than the body text of the page.
    let mut headings: Vec<Item> = Vec::new();
    let mut body: Vec<&Line> = Vec::new();
    for (index, line) in sorted.iter().enumerate() {
        if listed[index] {
            continue;
        }
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

    // The paragraphs. The engine's grouping comes first, because it knows
    // things the geometry does not, such as a paragraph whose first line is
    // indented. When the engine has grouped nothing, the lines are grouped
    // within their band, so that two columns stay apart.
    let paragraphs = if body.iter().any(|line| line.block_id != 0) {
        group_by_block(&body, clump)
    } else {
        let mut paragraphs = Vec::new();
        for band in align_bands(&body, median_height) {
            let lines: Vec<&Line> = band.iter().map(|index| body[*index]).collect();
            paragraphs.extend(group_by_geometry(&lines, median_height, clump));
        }
        paragraphs
    };

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

    for group in &groups {
        let lines: Vec<&Line> = group.iter().map(|index| sorted[*index]).collect();
        let (entries, bounds) = make_list(&lines);
        items.push(Item {
            bounds,
            block: Block::List {
                items: entries,
                bounds,
            },
        });
    }

    for heading in headings {
        items.push(heading);
    }

    let placed = place_items(items, median_height);
    (placed, threshold.unwrap_or(median_height))
}

/// Joins the blocks into text.
///
/// Where the page has a large gap between two blocks, the break is kept and
/// the next block is set out with an ideographic space, which is how a
/// paragraph is marked in Japanese.
pub fn join_blocks(blocks: &[Fresh]) -> String {
    let mut text = String::new();
    for (index, entry) in blocks.iter().enumerate() {
        if index > 0 {
            text.push('\n');
            if entry.new_paragraph {
                text.push_str(IDEOGRAPHIC_SPACE);
            }
        }
        text.push_str(&entry.block.text());
    }
    text
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
///
/// The engine's layout model already says which lines belong together, and it
/// knows things that cannot be told from the geometry alone, such as a
/// paragraph whose first line is indented. It is used when it has grouped
/// anything at all; otherwise the lines are grouped by their left edge and
/// the gap between them.
/// Groups the lines by the block the engine put them in.
fn group_by_block(lines: &[&Line], clump: bool) -> Vec<Paragraph> {
    let mut order: Vec<i64> = Vec::new();
    for line in lines {
        if !order.contains(&line.block_id) {
            order.push(line.block_id);
        }
    }

    order
        .into_iter()
        .map(|block| {
            let group: Vec<&Line> = lines
                .iter()
                .copied()
                .filter(|line| line.block_id == block)
                .collect();
            make_paragraph(&group, clump)
        })
        .collect()
}

/// Groups the lines by their left edge and the gap between them.
fn group_by_geometry(
    lines: &[&Line],
    median_height: f64,
    clump: bool,
) -> Vec<Paragraph> {
    let mut sorted: Vec<&Line> = lines.to_vec();
    sorted.sort_by(|a, b| {
        a.bounds
            .y
            .partial_cmp(&b.bounds.y)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let gaps: Vec<f64> = sorted
        .windows(2)
        .map(|pair| {
            let upper = pair[0].bounds.bottom();
            let lower = pair[1].bounds.y;
            (lower - upper).max(0.0)
        })
        .collect();

    let mut paragraphs: Vec<Paragraph> = Vec::new();
    let mut current: Vec<&Line> = Vec::new();
    let mut current_left: Option<f64> = None;

    for (position, line) in sorted.iter().enumerate() {
        let continues = match (current_left, position) {
            (Some(left), position) if position > 0 => {
                // Two lines are one paragraph when they line up on the left
                // and the space between them is smaller than the text is
                // tall: running text is set with less space between its
                // lines than the lines are tall.
                let same_edge = (line.bounds.x - left).abs() <= ITEM_ALIGNMENT * median_height;
                same_edge && gaps[position - 1] <= PARAGRAPH_GAP * median_height
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
    }

    if !current.is_empty() {
        paragraphs.push(make_paragraph(&current, clump));
    }
    paragraphs
}

/// Joins a group of lines into one paragraph.
fn make_paragraph(lines: &[&Line], clump: bool) -> Paragraph {
    // Whether the text is set with spaces between its words. The engine
    // knows this from the words it found.
    let spaced = lines.iter().any(|line| words_are_spaced(line));

    let mut text = String::new();
    for line in lines {
        if !text.is_empty() {
            if clump {
                // The lines of a paragraph are put back together the way the
                // page had them: with a space where the text has spaces
                // between its words, and with nothing where it does not, as
                // in Japanese.
                if spaced {
                    text.push(' ');
                }
            } else {
                text.push('\n');
            }
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

/// Whether the words of the line are set apart by spaces.
///
/// The engine says whether a space follows each word it found, so this is
/// known rather than guessed from the gaps between the words.
fn words_are_spaced(line: &Line) -> bool {
    line.words.iter().any(|word| word.has_space_after)
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
fn cut_into_columns<T: Placed + Clone>(items: &[T], median_height: f64) -> Vec<Vec<T>> {
    let mut columns = Vec::new();
    cut(items, median_height, &mut columns, 0);
    columns
}

/// One step of the cutting.
fn cut<T: Placed + Clone>(
    items: &[T],
    median_height: f64,
    columns: &mut Vec<Vec<T>>,
    depth: usize,
) {
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
fn split<T: Placed + Clone>(
    items: &[T],
    position: f64,
    axis: Axis,
    median_height: f64,
    columns: &mut Vec<Vec<T>>,
    depth: usize,
) {
    let (first, second): (Vec<T>, Vec<T>) = items
        .iter()
        .cloned()
        .partition(|item| {
            let bounds = item.bounds();
            let middle = match axis {
                Axis::X => bounds.x + bounds.width / 2.0,
                Axis::Y => bounds.y + bounds.height / 2.0,
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
fn widest_gap<T: Placed>(items: &[T], axis: Axis) -> Option<(f64, f64)> {
    let mut spans: Vec<(f64, f64)> = items
        .iter()
        .map(|item| {
            let bounds = item.bounds();
            match axis {
                Axis::X => (bounds.x, bounds.right()),
                Axis::Y => (bounds.y, bounds.bottom()),
            }
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

    /// A line of words that are set apart by spaces, as English is.
    fn spaced_line(words: &[&str], x: f64, y: f64, width: f64, height: f64) -> Line {
        let mut line = line(&words.join(" "), x, y, width, height);
        line.words = words
            .iter()
            .enumerate()
            .map(|(index, text)| Word {
                text: (*text).to_owned(),
                bounds: Rect {
                    x,
                    y,
                    width: width / words.len() as f64,
                    height,
                },
                has_space_after: index + 1 < words.len(),
            })
            .collect();
        line
    }

    /// A line of words that are set together, as Japanese is.
    fn tight_line(words: &[&str], x: f64, y: f64, width: f64, height: f64) -> Line {
        let mut line = spaced_line(words, x, y, width, height);
        for word in &mut line.words {
            word.has_space_after = false;
        }
        line
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
            block_id: 0,
        }
    }

    fn texts(blocks: &[Block]) -> Vec<String> {
        blocks.iter().map(Block::text).collect()
    }

    #[test]
    fn a_single_column_keeps_its_order() {
        let lines = vec![
            line("first", 100.0, 100.0, 200.0, 20.0),
            line("second", 100.0, 130.0, 200.0, 20.0),
            line("third", 100.0, 160.0, 200.0, 20.0),
        ];
        let layout = analyse(&lines, true, true);
        assert_eq!(texts(&layout.blocks), vec!["first second third"]);
    }

    /// The engine's layout model says which lines belong together, and it
    /// knows things the geometry alone does not, such as a paragraph whose
    /// first line is indented.
    #[test]
    fn the_lines_of_one_block_are_one_paragraph() {
        let mut first = line("A paragraph that starts", 96.0, 100.0, 300.0, 20.0);
        first.block_id = 1;
        let mut second = line("with an indented first line", 60.0, 130.0, 300.0, 20.0);
        second.block_id = 1;
        let mut third = line("and a third line", 60.0, 160.0, 300.0, 20.0);
        third.block_id = 1;

        let layout = analyse(&[first, second, third], true, true);
        assert_eq!(texts(&layout.blocks), vec!["A paragraph that starts with an indented first line and a third line"]);
    }

    /// Lines the engine put in different blocks are different paragraphs.
    #[test]
    fn lines_of_different_blocks_are_different_paragraphs() {
        let mut first = line("one", 60.0, 100.0, 200.0, 20.0);
        first.block_id = 1;
        let mut second = line("two", 60.0, 300.0, 200.0, 20.0);
        second.block_id = 2;

        let layout = analyse(&[first, second], true, true);
        assert_eq!(texts(&layout.blocks), vec!["one", "two"]);
    }

    /// A screen is mostly separate labels, with the occasional sentence
    /// wrapped over two lines. The engine tells the two apart.
    #[test]
    fn a_wrapped_sentence_is_joined_among_separate_labels() {
        let mut heading = line("Heading", 60.0, 60.0, 200.0, 24.0);
        heading.block_id = 0;
        let mut first = line("The sentence that was", 60.0, 120.0, 300.0, 20.0);
        first.block_id = 1;
        let mut second = line("wrapped over two lines", 60.0, 145.0, 300.0, 20.0);
        second.block_id = 1;
        let mut other = line("A separate label", 60.0, 200.0, 200.0, 20.0);
        other.block_id = 2;

        let layout = analyse(&[heading, first, second, other], true, true);
        assert_eq!(
            texts(&layout.blocks),
            vec![
                "Heading",
                "The sentence that was wrapped over two lines",
                "A separate label"
            ]
        );
    }


    /// The lines of a real page, as the recogniser found them.
    fn real_page() -> Vec<Line> {
vec![
        line("cookpad", 53.0, 17.0, 80.0, 18.0),
        line("«", 233.0, 21.0, 16.0, 12.0),
        line("Q", 24.0, 75.0, 20.0, 16.0),
        line("さがす", 61.0, 75.0, 48.0, 18.0),
        line("■ プレミアムサービス", 27.0, 127.0, 188.0, 20.0),
        line("かぼちゃ", 304.9, 142.0, 58.1, 16.3),
        line("「クリームチー", 572.0, 142.0, 101.0, 17.0),
        line("里芋", 855.0, 139.8, 32.1, 16.2),
        line("☆ パートナープログラム", 24.0, 179.0, 208.0, 19.0),
        line("8 レシピチャレンジ", 26.0, 229.0, 171.0, 24.0),
        line("小松菜", 304.0, 242.8, 45.1, 19.2),
        line("国または地域", 60.0, 286.0, 103.0, 17.0),
        line("人気のおかず", 297.0, 304.0, 103.0, 17.0),
        line("きろく", 62.0, 338.0, 46.0, 14.0),
        line("レシピの保存、レシピの作成をおこ", 24.0, 378.0, 214.0, 14.0),
        line("なうにはログインしてください", 23.0, 398.0, 193.0, 13.0),
        line("ポトフ", 304.0, 409.9, 46.0, 15.1),
        line("ログイン", 1143.0, 14.5, 58.1, 17.5),
        line("+ レシピを書く", 1252.0, 13.0, 110.2, 19.0),
        line("ほうれん草", 855.0, 243.6, 77.1, 18.4),
        line("ブロッコリー", 1131.0, 242.3, 89.1, 20.7),
        line("おでん", 580.0, 409.7, 46.1, 15.3),
        line("グラタン", 854.0, 406.5, 63.1, 19.5),
        line("鍋", 1131.0, 408.0, 16.0, 16.0),
        line("クラムチャウダー", 305.0, 513.0, 122.0, 16.0),
        line("ンバーグ", 597.0, 510.9, 62.0, 16.1),
        line("すきやき", 856.0, 513.0, 59.0, 13.2),
        line("カテゴリ", 296.0, 569.0, 66.0, 18.0),
        line("ロン", 317.0, 798.0, 21.0, 9.0),
        line("野菜", 362.0, 633.0, 26.0, 14.0),
        line("09", 595.0, 635.0, 18.0, 15.0),
        line("お肉", 637.0, 633.0, 27.0, 14.0),
        line("魚介", 913.0, 634.0, 27.0, 15.0),
        line("サラダ", 362.0, 713.8, 41.1, 15.2),
        line("たまご", 1188.0, 635.0, 39.0, 13.0),
        line("スープ", 636.0, 713.0, 41.0, 13.0),
        line("ごはんもの", 915.0, 712.9, 65.0, 14.1),
        line("麵", 1186.0, 714.0, 15.0, 13.0),
        line("お弁当", 361.0, 793.9, 41.0, 14.1),
        line("パン", 636.0, 793.0, 27.0, 12.0),
        line("人気のキーワード", 297.0, 905.0, 139.0, 19.0),
        line("クックパッドのプレミアムサービス", 35.0, 955.0, 253.0, 18.0),
        line("人気順検索や殿堂入りレシピなど、失敗知らずの人気レシピが見放題!", 35.0, 976.0, 495.0, 20.0),
        line("カテゴリをもっと見る", 771.0, 861.0, 135.0, 14.0),
        line("お菓子", 912.0, 793.0, 42.0, 16.0),
        line("おもてなし", 1188.0, 795.0, 67.0, 13.0),
        line("20:00 更新", 1325.0, 305.0, 57.0, 12.0),
        line("20:00 更新", 1325.0, 907.9, 58.0, 12.1),
        line("プレミアムサービスをはじめる", 1142.0, 967.0, 214.0, 16.0),
]
    }

    /// The menu of a real page, taken from an actual screen capture. The
    /// entries are short, share a left edge, and are indented in two levels.
    #[test]
    fn a_menu_of_a_real_page_is_found() {
        let lines = real_page();

        let layout = analyse(&lines, true, true);

        // The entries of the menu come out as one list.
        let lists: Vec<&Vec<String>> = layout
            .blocks
            .iter()
            .filter_map(|block| match block {
                crate::layout::Block::List { items, .. } => Some(items),
                _ => None,
            })
            .collect();
        assert!(!lists.is_empty(), "the menu should have been found");

        let items = &lists[0];
        assert!(
            items.iter().any(|item| item.contains("プレミアムサービス")),
            "the menu should hold the entries, got {items:?}"
        );
    }

    /// The text of a real page keeps the breaks the page has, and a break is
    /// marked with an ideographic space.
    #[test]
    fn the_text_of_a_real_page_keeps_its_breaks() {
        let lines = real_page();
        let layout = analyse(&lines, true, true);
        let fresh = (0..layout.blocks.len())
            .map(|index| crate::layout::Fresh {
                block: layout.blocks[index].clone(),
                new_paragraph: layout.starts_paragraph(index),
            })
            .collect::<Vec<_>>();
        let text = crate::layout::join_blocks(&fresh);

        assert!(
            text.contains("プレミアムサービス"),
            "the text should hold what the page said, got {text:?}"
        );
        assert!(
            text.contains('\u{3000}'),
            "the break should be marked with an ideographic space, got {text:?}"
        );

        // The two lines of one wrapped sentence are one paragraph.
        let paragraph = layout
            .blocks
            .iter()
            .find(|block| block.text().contains("レシピの保存"))
            .expect("the wrapped sentence should be a paragraph");
        assert!(
            paragraph.text().contains("なうにはログインしてください"),
            "both lines of the wrapped sentence should be in one paragraph, got {:?}",
            paragraph.text()
        );

        // The lists of the page are found: the menu, and the five columns of
        // categories under it.
        let lists: Vec<Vec<String>> = layout
            .blocks
            .iter()
            .filter_map(|block| match block {
                crate::layout::Block::List { items, .. } => Some(items.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(lists.len(), 6, "the page should hold six lists, got {lists:?}");
        assert!(
            lists[0].iter().any(|item| item.contains("プレミアムサービス")),
            "the first list should be the menu, got {lists:?}"
        );
        assert!(
            lists.iter().any(|items| {
                items.len() == 3 && items[0] == "人気のおかず" && items[2] == "クラムチャウダー"
            }),
            "the column of dishes should be a list, got {lists:?}"
        );
    }

    /// With the gathering turned off, the entries of the menu are kept on
    /// their own.
    #[test]
    fn the_entries_of_a_menu_can_be_kept_apart() {
        let mut first = line("Q", 24.0, 75.0, 20.0, 16.0);
        first.block_id = 1;
        let mut second = line("A section", 27.0, 127.0, 188.0, 20.0);
        second.block_id = 2;
        let mut third = line("Another section", 24.0, 179.0, 208.0, 19.0);
        third.block_id = 3;
        let mut fourth = line("And another", 26.0, 229.0, 171.0, 24.0);
        fourth.block_id = 4;

        let lines = vec![first, second, third, fourth];
        let gathered = analyse(&lines, true, true);
        let apart = analyse(&lines, true, false);

        let lists = |layout: &crate::layout::Layout| {
            layout
                .blocks
                .iter()
                .filter(|block| matches!(block, crate::layout::Block::List { .. }))
                .count()
        };
        assert_eq!(lists(&gathered), 1, "the menu should be gathered");
        assert_eq!(lists(&apart), 0, "the menu should not be gathered");
    }

    /// A large gap between two blocks leaves an ideographic space in the
    /// text, so that the break on the page is kept.
    #[test]
    fn a_large_gap_leaves_a_blank_line_in_the_text() {
        let mut first = line("above", 60.0, 100.0, 200.0, 20.0);
        first.block_id = 1;
        let mut second = line("below", 60.0, 300.0, 200.0, 20.0);
        second.block_id = 2;

        let layout = analyse(&[first, second], true, true);
        let fresh = (0..layout.blocks.len())
            .map(|index| crate::layout::Fresh {
                block: layout.blocks[index].clone(),
                new_paragraph: layout.starts_paragraph(index),
            })
            .collect::<Vec<_>>();
        let text = crate::layout::join_blocks(&fresh);
        assert!(
            text.contains("\n\u{3000}"),
            "a large gap should leave an ideographic space, got {text:?}"
        );
        assert!(
            text.as_bytes().contains(&0xE3u8),
            "the space must be the ideographic one, not a plain one: {text:?}"
        );
    }

    /// A block that was already shown still stands between two new ones, so
    /// the break between them is the one the page has there, and not the
    /// larger gap that spans across the block in between.
    #[test]
    fn a_block_that_was_already_shown_does_not_make_a_break() {
        let blocks = vec![
            Block::Paragraph {
                text: "above".to_owned(),
                bounds: Rect { x: 0.0, y: 100.0, width: 100.0, height: 20.0 },
            },
            Block::Paragraph {
                text: "between".to_owned(),
                bounds: Rect { x: 0.0, y: 200.0, width: 100.0, height: 200.0 },
            },
            Block::Paragraph {
                text: "below".to_owned(),
                bounds: Rect { x: 0.0, y: 420.0, width: 100.0, height: 20.0 },
            },
        ];
        let layout = Layout { blocks, block_gap: 50.0 };

        // The block in between was shown already, so only the other two are
        // new.
        let fresh = vec![
            Fresh {
                block: layout.blocks[0].clone(),
                new_paragraph: layout.starts_paragraph(0),
            },
            Fresh {
                block: layout.blocks[2].clone(),
                new_paragraph: layout.starts_paragraph(2),
            },
        ];

        let text = join_blocks(&fresh);
        assert!(
            !text.contains(IDEOGRAPHIC_SPACE),
            "the gap spans across a block that was shown already, got {text:?}"
        );
        assert_eq!(text, "above\nbelow");
    }

    /// With the joining turned off, each line keeps its own line.
    #[test]
    fn the_lines_of_a_paragraph_can_be_kept_apart() {
        let lines = vec![
            line("first", 100.0, 100.0, 200.0, 20.0),
            line("second", 100.0, 130.0, 200.0, 20.0),
        ];
        let layout = analyse(&lines, false, true);
        assert_eq!(texts(&layout.blocks), vec!["first\nsecond"]);
    }

    #[test]
    fn lines_far_apart_become_separate_paragraphs() {
        let lines = vec![
            line("first", 100.0, 100.0, 200.0, 20.0),
            line("second", 100.0, 400.0, 200.0, 20.0),
        ];
        let layout = analyse(&lines, true, true);
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
        let layout = analyse(&lines, true, true);
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
        let layout = analyse(&lines, true, true);
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
        let layout = analyse(&lines, true, true);
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

        let layout = analyse(&[left, right], true, true);
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

        let layout = analyse(&[text_line], true, true);
        assert_eq!(layout.blocks[0].text(), "Hello World");
    }

    #[test]
    fn lines_that_are_not_text_are_left_out() {
        let mut rule = line("|", 10.0, 10.0, 400.0, 2.0);
        rule.is_text = false;
        let lines = vec![rule, line("real text", 10.0, 40.0, 200.0, 20.0)];

        let layout = analyse(&lines, true, true);
        assert_eq!(texts(&layout.blocks), vec!["real text"]);
    }

    /// The entries of a list are apart on the page, so the space that goes
    /// between them is one of ours, and it is the ideographic space.
    #[test]
    fn the_entries_of_a_list_are_separated_by_an_ideographic_space() {
        let mut first = line("Q", 24.0, 75.0, 20.0, 16.0);
        first.block_id = 1;
        let mut second = line("A section", 27.0, 127.0, 188.0, 20.0);
        second.block_id = 2;
        let mut third = line("Another section", 24.0, 179.0, 208.0, 19.0);
        third.block_id = 3;
        let mut fourth = line("And another", 26.0, 229.0, 171.0, 24.0);
        fourth.block_id = 4;

        let layout = analyse(&[first, second, third, fourth], true, true);
        let text = layout
            .blocks
            .iter()
            .map(|block| block.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("Q\u{3000}A section\u{3000}Another section\u{3000}And another"),
            "the entries should be separated by an ideographic space, got {text:?}"
        );
    }

    /// The lines of a Japanese paragraph are put back together with nothing
    /// between them, because Japanese is written without spaces between its
    /// words.
    #[test]
    fn the_lines_of_a_japanese_paragraph_are_joined_without_a_space() {
        let first = tight_line(&["レシピの保存、レシピの作成をおこ"], 60.0, 100.0, 200.0, 20.0);
        let second = tight_line(&["なうにはログインしてください"], 60.0, 130.0, 200.0, 20.0);

        let layout = analyse(&[first, second], true, true);
        let text = layout
            .blocks
            .iter()
            .map(|block| block.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("レシピの保存、レシピの作成をおこなうにはログインしてください"),
            "the sentence should be put back together, got {text:?}"
        );
    }

    /// Where the text has spaces between its words, the lines of a paragraph
    /// are put back together with a space between them.
    #[test]
    fn the_lines_of_a_spaced_paragraph_are_joined_with_a_space() {
        let first = spaced_line(&["popular", "search"], 60.0, 100.0, 200.0, 20.0);
        let second = spaced_line(&["and", "more"], 60.0, 130.0, 200.0, 20.0);

        let layout = analyse(&[first, second], true, true);
        let text = layout
            .blocks
            .iter()
            .map(|block| block.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("popular search and more"),
            "the words should be put back together, got {text:?}"
        );
    }

    #[test]
    fn nothing_gives_an_empty_layout() {
        let layout = analyse(&[], true, true);
        assert!(layout.blocks.is_empty());
        assert!(layout.blocks.is_empty());
    }
}
