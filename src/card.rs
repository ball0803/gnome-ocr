//! The card that shows what a word means.
//!
//! The card is the visible face of the dictionary: the user points at a
//! word, the app asks the dictionary about it, and the card says what the
//! word means. Its looks come from Yomitan, the browser extension that
//! holds the dictionaries, so that the card reads the way the user
//! expects from it.
//!
//! Yomitan's popup is a card with a bold headword, the reading of the
//! word in a quieter colour beside it, the meanings as a list, the tags
//! as small rounded pills and the name of the dictionary underneath. That
//! structure is kept here, but the colours are the ones of the GTK
//! theme, so that the card is right in a dark GNOME as well. The
//! rounding of the pills and the sizes of the words follow Yomitan's own
//! stylesheet.
//!
//! The card is a popover, so that it can be put beside the pointer
//! without taking any room in the window. It takes no focus and no
//! keyboard, so that putting it up does not interrupt what the user is
//! doing, and it stays up until it is told to go away.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::gdk;
use gtk::prelude::*;

use crate::yomitan;

/// The looks of the card.
///
/// The colours are the ones of the theme, so that the card is right in a
/// dark GNOME as well as in a light one. The rounding of the tags is
/// the one Yomitan uses.
const CSS: &str = "
.card-content {
    padding-top: 10px;
    padding-bottom: 10px;
    padding-left: 12px;
    padding-right: 12px;
}

.card-word {
    font-weight: bold;
    font-size: 1.15em;
}

.card-reading {
    color: @theme_unfocused_fg_color;
    font-size: 0.85em;
}

.card-meaning {
    font-size: 0.95em;
}

.card-tag {
    background-color: @accent_bg_color;
    color: @accent_fg_color;
    border-radius: 0.25em;
    padding-top: 0.05em;
    padding-bottom: 0.05em;
    padding-left: 0.4em;
    padding-right: 0.4em;
    font-size: 0.8em;
}

.card-dictionary {
    color: @theme_unfocused_fg_color;
    font-size: 0.8em;
}
";

/// The kind of tag that says what part of speech the word is.
///
/// The dictionary says which kind each of its tags is, so that the word
/// classes are told from the marks that say how common a word is.
const WORD_CLASS: &str = "partOfSpeech";

/// The word classes the dictionaries use, and the codes for them.
///
/// A dictionary says what kind of word it is with a short code: `n` for a
/// noun, `v1` for a verb of the first class, `adj-no` for an adjective
/// that has the `na` form, and so on.
///
/// This is a list and not a pattern on purpose. The dictionaries also
/// send frequency ranks and popularity marks — `ichi`, `news1k`, `⭐`,
/// `★`, `1` — which look like codes but say nothing about the word, and
/// no shape tells them apart from a word class.
const WORD_CLASSES: &[&str] = &[
    // The nouns, and the words that behave like them.
    "n",
    "n-adv",
    "n-pref",
    "ctr",
    "exp",
    "num",
    "pron",
    "unk",
    // The verbs.
    "v1",
    "v1-s",
    "v2",
    "v3",
    "v4",
    "v5",
    "v5r",
    "v5k",
    "v5u",
    "v5s",
    "vi",
    "vt",
    "vk",
    "vz",
    "vs",
    "vs-i",
    "vs-s",
    "aux",
    "aux-v",
    "aux-adj",
    // The adjectives.
    "adj-i",
    "adj-f",
    "adj-na",
    "adj-no",
    "adj-ku",
    "adj-ix",
    "adj-shiku",
    "adj-kari",
    // The rest of the word classes.
    "adv",
    "adv-to",
    "conj",
    "cop",
    "cop-da",
    "int",
    "interj",
    "pref",
    "suf",
    "punct",
];

/// The card that shows what a word means.
pub struct Card {
    /// The popover the card is drawn in.
    popover: gtk::Popover,
    /// The box the words are put in, which is emptied and filled again
    /// every time the card is shown.
    content: gtk::Box,
    /// The window the popover is attached to. A widget can only be given
    /// a parent once, so the window is remembered to keep it from being
    /// moved about for nothing.
    window: RefCell<Option<gtk::Root>>,
    /// Whether the card is up. The popover says so as well, but it says
    /// so only once it has been drawn, and this says so at once.
    showing: Rc<Cell<bool>>,
}

impl Card {
    /// A card, with nothing in it yet.
    pub fn new() -> Self {
        let popover = gtk::Popover::new();
        // The card is a hint and not a place to work in, so it takes no
        // focus and no keyboard, and it closes only when it is told to.
        popover.set_autohide(false);
        popover.set_has_arrow(false);
        popover.set_can_focus(false);
        popover.set_focusable(false);
        popover.set_focus_on_click(false);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.add_css_class("card-content");

        let provider = gtk::CssProvider::new();
        provider.connect_parsing_error(|_, _section, error| {
            tracing::warn!("the stylesheet of the card does not fit: {error}");
        });
        provider.load_from_string(CSS);
        // The stylesheet is given to the whole display, but it says
        // nothing about anything but the card, which has classes of its
        // own.
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

        popover.set_child(Some(&content));

        let showing = Rc::new(Cell::new(false));
        let flag = Rc::clone(&showing);
        popover.connect_closed(move |_| flag.set(false));

        Self {
            popover,
            content,
            window: RefCell::new(None),
            showing,
        }
    }

    /// Shows the word and what the dictionary says about it, beside the
    /// widget the pointer is over.
    ///
    /// The card is put where the pointer is, so that it is clear which
    /// word it is about. The entries are grouped by the word they are
    /// for, so that the card reads as one block per word.
    pub fn show(
        &self,
        parent: &impl IsA<gtk::Widget>,
        word: &str,
        reading: Option<&str>,
        entries: &[yomitan::Entry],
    ) {
        let words = blocks(word, reading, entries);
        self.fill(&words);

        // The card is made of widgets, so the same words are given to the
        // screen reader as one block of text.
        let text = card_text(&words);
        self.content.update_property(&[gtk::accessible::Property::Description(&text)]);

        let Some(window) = parent.root() else {
            tracing::warn!("the card has no window to sit in");
            return;
        };
        // The popover can only be given a parent once, so it is only
        // moved when the word is looked at in another window.
        let mut attached = self.window.borrow_mut();
        if attached.as_ref() != Some(&window) {
            self.popover.set_parent(&window);
            *attached = Some(window);
        }
        drop(attached);

        if let Some(place) = pointing_to(parent) {
            self.popover.set_pointing_to(Some(&place));
        }
        self.popover.popup();
        self.showing.set(true);
        tracing::debug!("the card shows {word}");
    }

    /// Takes the card away.
    ///
    /// The caller hides the card when the pointer moves away from the
    /// word, so that the card is only there while the word is being
    /// looked at.
    pub fn hide(&self) {
        if !self.is_showing() {
            return;
        }
        self.popover.popdown();
        self.showing.set(false);
    }

    /// Whether the card is showing.
    pub fn is_showing(&self) -> bool {
        self.showing.get()
    }

    /// Puts the words on the card, taking away what was on it before.
    fn fill(&self, words: &[WordBlock]) {
        while let Some(child) = self.content.first_child() {
            self.content.remove(&child);
        }
        for (number, block) in words.iter().enumerate() {
            // The words are told apart by a line, the way the entries of
            // a dictionary are.
            if number > 0 {
                self.content
                    .append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            }
            self.content.append(&headword(block));
            if !block.meanings.is_empty() {
                self.content.append(&meanings(block));
            }
            if !block.tags.is_empty() {
                self.content.append(&tags(block));
            }
            if !block.dictionaries.is_empty() {
                self.content.append(&dictionary(block));
            }
        }
    }
}

/// What the card shows about one word.
#[derive(Debug, PartialEq, Eq)]
struct WordBlock {
    /// The word itself.
    word: String,
    /// How it is read, when there is a reading to show.
    reading: Option<String>,
    /// What it means, one meaning after another.
    meanings: Vec<String>,
    /// The tags that say what kind of word it is.
    tags: Vec<String>,
    /// The dictionaries the meanings came from.
    dictionaries: Vec<String>,
}

/// The words the card shows, one block for each of them.
///
/// The entries are grouped by the word they are for, so that the card
/// reads as one block per word. The word that was looked at comes first
/// and carries the reading that was given with it, because that is the
/// word the user is asking about.
fn blocks(word: &str, reading: Option<&str>, entries: &[yomitan::Entry]) -> Vec<WordBlock> {
    let mut words: Vec<WordBlock> = Vec::new();
    for entry in entries {
        let number = match words.iter().position(|known| known.word == entry.term) {
            Some(number) => number,
            None => {
                words.push(WordBlock {
                    word: entry.term.clone(),
                    reading: entry.reading.clone(),
                    meanings: Vec::new(),
                    tags: Vec::new(),
                    dictionaries: Vec::new(),
                });
                words.len() - 1
            }
        };
        let known = &mut words[number];
        known.meanings.extend(entry.definitions.iter().cloned());
        for tag in entry.tags.iter().filter(|tag| is_word_class(tag)) {
            if !known.tags.contains(&tag.name) {
                known.tags.push(tag.name.clone());
            }
        }
        if let Some(dictionary) = &entry.dictionary
            && !known.dictionaries.contains(dictionary)
        {
            known.dictionaries.push(dictionary.clone());
        }
    }

    match words.iter().position(|known| known.word == word) {
        Some(number) => {
            let mut first = words.remove(number);
            if let Some(reading) = reading {
                first.reading = Some(reading.to_owned());
            }
            words.insert(0, first);
        }
        // The dictionary had nothing for the word, but it is still the
        // word the user is asking about, so it heads the card.
        None => words.insert(
            0,
            WordBlock {
                word: word.to_owned(),
                reading: reading.map(str::to_owned),
                meanings: Vec::new(),
                tags: Vec::new(),
                dictionaries: Vec::new(),
            },
        ),
    }
    words
}

/// Whether a tag says what kind of word it is, and is worth showing.
///
/// The dictionaries send all sorts of tags with a word. The word classes
/// — `n` for a noun, `v1` for a verb of the first class, `adj-no` for an
/// adjective that has the `na` form — say what kind of word it is, and
/// are worth showing. The rest are noise: the frequency ranks of the
/// dictionary (`ichi`, `news1k`), the marks that say how common a word is
/// (`⭐`, `★`, `1`), and the empty tags that come with everything.
///
/// The dictionary says which kind each tag is, so that is asked first. A
/// tag that is one of the known word classes is kept as well, so that a
/// word class is not lost when the dictionary does not say what kind a
/// tag is.
fn is_word_class(tag: &yomitan::Tag) -> bool {
    let name = tag.name.trim();
    if name.is_empty() {
        return false;
    }
    tag.category == WORD_CLASS || WORD_CLASSES.contains(&name)
}

/// The card as text, one line for each thing on it.
///
/// The card is made of widgets, so that the tags can be pills and the
/// reading can be dimmer than the word. This is the same content as
/// text, which is what a screen reader is given and what the tests look
/// at.
fn card_text(words: &[WordBlock]) -> String {
    let mut text = String::new();
    for block in words {
        text.push_str(&block.word);
        if let Some(reading) = &block.reading {
            // The reading is written the way the Japanese writes it,
            // beside the word and a little apart from it.
            text.push_str("  ");
            text.push_str(reading);
        }
        text.push('\n');
        for meaning in &block.meanings {
            text.push_str(meaning);
            text.push('\n');
        }
        for tag in &block.tags {
            text.push_str(tag);
            text.push('\n');
        }
        for dictionary in &block.dictionaries {
            text.push_str(dictionary);
            text.push('\n');
        }
    }
    text
}

/// The word and how it is read.
fn headword(block: &WordBlock) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let word = gtk::Label::new(Some(&block.word));
    word.set_xalign(0.0);
    word.add_css_class("card-word");
    row.append(&word);

    if let Some(reading) = &block.reading {
        let label = gtk::Label::new(Some(reading));
        label.set_xalign(0.0);
        label.add_css_class("card-reading");
        // The reading sits a little apart from the word, the way the
        // Japanese writes it.
        label.set_margin_start(8);
        row.append(&label);
    }
    row
}

/// The meanings of the word, one to a line.
///
/// Each meaning is a label of its own, so that one of them can be picked
/// and copied on its own.
fn meanings(block: &WordBlock) -> gtk::Box {
    let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
    for meaning in &block.meanings {
        let label = gtk::Label::new(Some(meaning));
        label.set_xalign(0.0);
        label.set_yalign(0.0);
        label.set_wrap(true);
        label.set_max_width_chars(40);
        label.set_selectable(true);
        label.add_css_class("card-meaning");
        list.append(&label);
    }
    list
}

/// The tags of the word, as the small pills they are in Yomitan.
fn tags(block: &WordBlock) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    for tag in &block.tags {
        let label = gtk::Label::new(Some(tag));
        label.add_css_class("card-tag");
        row.append(&label);
    }
    row
}

/// Which dictionary the meanings came from.
fn dictionary(block: &WordBlock) -> gtk::Label {
    let label = gtk::Label::new(Some(&block.dictionaries.join(", ")));
    label.set_xalign(0.0);
    label.add_css_class("card-dictionary");
    label
}

/// The place the pointer is at, in the coordinates of the window.
///
/// The card points at a rectangle of one pixel at the pointer, which is
/// what tells it where to sit.
fn pointing_to(parent: &impl IsA<gtk::Widget>) -> Option<gdk::Rectangle> {
    let surface = parent.root()?.native()?.surface()?;
    let pointer = gdk::Display::default()?.default_seat()?.pointer()?;
    let (x, y, _) = surface.device_position(&pointer)?;
    Some(gdk::Rectangle::new(pixel(x)?, pixel(y)?, 1, 1))
}

/// The pixel a place on the screen is at.
///
/// The screen is measured in fractions of a pixel, and a rectangle wants
/// whole ones.
fn pixel(place: f64) -> Option<i32> {
    let rounded = place.round();
    if rounded < f64::from(i32::MIN) || rounded > f64::from(i32::MAX) {
        return None;
    }
    Some(rounded as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tag, as the dictionary would give it.
    fn tag(name: &str, category: &str) -> yomitan::Tag {
        yomitan::Tag {
            name: name.to_owned(),
            category: category.to_owned(),
        }
    }

    /// An entry, as the dictionary would give it.
    fn entry(
        term: &str,
        reading: Option<&str>,
        definitions: &[&str],
        tags: &[yomitan::Tag],
        dictionary: Option<&str>,
    ) -> yomitan::Entry {
        yomitan::Entry {
            term: term.to_owned(),
            reading: reading.map(str::to_owned),
            definitions: definitions.iter().map(|text| (*text).to_owned()).collect(),
            tags: tags.to_vec(),
            dictionary: dictionary.map(str::to_owned),
        }
    }

    #[test]
    fn the_word_classes_are_kept() {
        for name in ["n", "adj-no", "v1"] {
            assert!(
                is_word_class(&tag(name, WORD_CLASS)),
                "{name} says what kind of word it is"
            );
            assert!(
                is_word_class(&tag(name, "")),
                "{name} is a word class even when the dictionary does not say"
            );
        }
    }

    #[test]
    fn the_marks_and_the_ranks_are_dropped() {
        for (name, category) in [
            ("", ""),
            ("⭐", "popular"),
            ("★", "popular"),
            ("1", "frequency"),
            ("2", "frequency"),
            ("ichi", "frequent"),
            ("news1k", "frequency"),
        ] {
            assert!(
                !is_word_class(&tag(name, category)),
                "{name} says nothing about the word"
            );
        }
    }

    #[test]
    fn the_entries_are_grouped_by_their_word() {
        let entries = vec![
            entry(
                "人気",
                Some("にんき"),
                &["popularity"],
                &[tag("n", WORD_CLASS)],
                Some("JMdict"),
            ),
            entry(
                "犬",
                Some("いぬ"),
                &["a dog"],
                &[tag("n", WORD_CLASS)],
                Some("JMdict"),
            ),
            entry(
                "人気",
                Some("にんき"),
                &["a popular thing"],
                &[tag("v5", WORD_CLASS)],
                Some("JMdict"),
            ),
        ];

        let words = blocks("人気", Some("にんき"), &entries);

        assert_eq!(words.len(), 2);
        assert_eq!(words[0].word, "人気");
        assert_eq!(words[0].reading, Some("にんき".to_owned()));
        assert_eq!(words[0].meanings, ["popularity", "a popular thing"]);
        assert_eq!(words[0].tags, ["n", "v5"]);
        assert_eq!(words[0].dictionaries, ["JMdict"]);
        assert_eq!(words[1].word, "犬");
        assert_eq!(words[1].meanings, ["a dog"]);
    }

    #[test]
    fn the_word_that_was_looked_at_comes_first() {
        let entries = vec![
            entry(
                "犬",
                Some("いぬ"),
                &["a dog"],
                &[tag("n", WORD_CLASS)],
                Some("JMdict"),
            ),
            entry(
                "人気",
                Some("にんき"),
                &["popularity"],
                &[tag("n", WORD_CLASS)],
                Some("JMdict"),
            ),
        ];

        let words = blocks("人気", Some("にんき"), &entries);

        assert_eq!(words.len(), 2);
        assert_eq!(words[0].word, "人気");
        assert_eq!(words[1].word, "犬");
    }

    #[test]
    fn a_word_the_dictionary_knows_nothing_about_is_still_shown() {
        let words = blocks("コーク", None, &[]);

        assert_eq!(words.len(), 1);
        assert_eq!(words[0].word, "コーク");
        assert!(words[0].meanings.is_empty());
    }

    #[test]
    fn the_card_says_the_word_the_meanings_and_the_tags() {
        let entries = vec![entry(
            "人気",
            Some("にんき"),
            &["popularity"],
            &[tag("n", WORD_CLASS), tag("ichi", "frequent")],
            Some("JMdict"),
        )];

        let text = card_text(&blocks("人気", Some("にんき"), &entries));
        let lines: Vec<&str> = text.lines().collect();

        assert!(lines.contains(&"人気  にんき"));
        assert!(lines.contains(&"popularity"));
        assert!(lines.contains(&"n"));
        assert!(lines.contains(&"JMdict"));
        assert!(!lines.contains(&"ichi"));
    }
}
