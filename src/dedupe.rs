//! Deciding whether recognised text is new.
//!
//! The same words stay on screen, so recognising them again is not new text.
//! A detection counts as new only when its text has not been seen in a place
//! that looks like the same place.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The thresholds that decide whether two detections are in the same place.
///
/// They can be changed while the app runs.
#[derive(Debug, Clone, Copy)]
pub struct Settings {
    /// How much two boxes have to overlap to count as the same place.
    ///
    /// This is the usual threshold for "the same object" in detection work.
    pub overlap: f64,
    /// How alike two boxes have to be in size to count as the same place
    /// even when they do not overlap.
    pub dimension: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            overlap: 0.5,
            dimension: 0.9,
        }
    }
}

/// An axis-aligned rectangle in the pixels of the captured frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Box {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Box {
    fn area(&self) -> f64 {
        self.width.max(0.0) * self.height.max(0.0)
    }

    fn right(&self) -> f64 {
        self.x + self.width
    }

    fn bottom(&self) -> f64 {
        self.y + self.height
    }
}

/// Intersection over Union, also known as the Jaccard index.
///
/// `|A and B| / |A or B|`, which is 1 for identical boxes and 0 for boxes
/// that do not touch. For axis-aligned boxes the intersection is itself a
/// rectangle.
pub fn overlap(a: &Box, b: &Box) -> f64 {
    let width = (a.right().min(b.right()) - a.x.max(b.x)).max(0.0);
    let height = (a.bottom().min(b.bottom()) - a.y.max(b.y)).max(0.0);
    let intersection = width * height;
    let union = a.area() + b.area() - intersection;
    if union <= 0.0 {
        return 0.0;
    }
    intersection / union
}

/// How alike two boxes are in size, ignoring where they are.
///
/// Each dimension is compared by how much it differs from the larger of the
/// two, so the measure does not change with the size of the frame. The two
/// differences are averaged, giving 1 for identical dimensions and 0 when
/// one of them does not match at all.
pub fn dimension_similarity(a: &Box, b: &Box) -> f64 {
    let width_difference = relative_difference(a.width, b.width);
    let height_difference = relative_difference(a.height, b.height);
    (1.0 - (width_difference + height_difference) / 2.0).clamp(0.0, 1.0)
}

fn relative_difference(a: f64, b: f64) -> f64 {
    let larger = a.abs().max(b.abs());
    if larger <= 0.0 {
        return 0.0;
    }
    (a - b).abs() / larger
}

/// Something the recogniser found.
#[derive(Debug, Clone)]
pub struct Detection {
    pub text: String,
    pub bounds: Box,
}

/// What has been seen so far, so repeats can be told apart.
#[derive(Debug)]
pub struct History {
    entries: HashMap<String, Vec<Box>>,
    settings: Arc<Mutex<Settings>>,
}

impl History {
    pub fn new(settings: Arc<Mutex<Settings>>) -> Self {
        Self {
            entries: HashMap::new(),
            settings,
        }
    }

    fn thresholds(&self) -> Settings {
        self.settings.lock().map(|settings| *settings).unwrap_or_default()
    }

    /// Whether this detection is something the user has not seen before.
    ///
    /// It is not new when its text has been seen before in a box that either
    /// overlaps the old one or is about the same size.
    pub fn is_new(&mut self, detection: &Detection) -> bool {
        let Some(previous) = self.entries.get(&detection.text) else {
            self.entries
                .insert(detection.text.clone(), vec![detection.bounds]);
            return true;
        };

        let thresholds = self.thresholds();
        let same_place = previous.iter().any(|previous_box| {
            overlap(previous_box, &detection.bounds) >= thresholds.overlap
                || dimension_similarity(previous_box, &detection.bounds) >= thresholds.dimension
        });

        if !same_place {
            self.entries
                .entry(detection.text.clone())
                .or_default()
                .push(detection.bounds);
        }

        !same_place
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_at(x: f64, y: f64, width: f64, height: f64) -> Box {
        Box {
            x,
            y,
            width,
            height,
        }
    }

    fn detection(text: &str, bounds: Box) -> Detection {
        Detection {
            text: text.to_owned(),
            bounds,
        }
    }

    #[test]
    fn identical_boxes_fully_overlap() {
        let a = box_at(0.0, 0.0, 100.0, 50.0);
        assert!((overlap(&a, &a) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn boxes_that_do_not_touch_do_not_overlap() {
        let a = box_at(0.0, 0.0, 100.0, 50.0);
        let b = box_at(500.0, 500.0, 100.0, 50.0);
        assert_eq!(overlap(&a, &b), 0.0);
    }

    #[test]
    fn half_overlapping_boxes_overlap_by_a_third() {
        let a = box_at(0.0, 0.0, 100.0, 100.0);
        let b = box_at(50.0, 0.0, 100.0, 100.0);
        // 50x100 of intersection over 150x100 of union.
        assert!((overlap(&a, &b) - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn the_same_size_scores_a_full_similarity() {
        let a = box_at(0.0, 0.0, 100.0, 50.0);
        let b = box_at(900.0, 900.0, 100.0, 50.0);
        assert!((dimension_similarity(&a, &b) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_change_in_one_dimension_costs_half_of_it() {
        let a = box_at(0.0, 0.0, 100.0, 50.0);
        let b = box_at(0.0, 0.0, 110.0, 50.0);
        // The difference is measured against the larger of the two, so ten
        // wider is ten over a hundred and ten, which is a twentieth of the
        // score.
        let expected = 1.0 - (10.0 / 110.0) / 2.0;
        assert!((dimension_similarity(&a, &b) - expected).abs() < 1e-9);
    }

    #[test]
    fn the_similarity_does_not_depend_on_which_box_is_first() {
        let a = box_at(0.0, 0.0, 100.0, 50.0);
        let b = box_at(0.0, 0.0, 110.0, 50.0);
        assert_eq!(dimension_similarity(&a, &b), dimension_similarity(&b, &a));
    }

    #[test]
    fn a_box_of_a_completely_different_size_scores_low() {
        let a = box_at(0.0, 0.0, 100.0, 50.0);
        let b = box_at(0.0, 0.0, 400.0, 200.0);
        assert!(dimension_similarity(&a, &b) < 0.5);
    }

    #[test]
    fn the_same_text_in_the_same_place_is_not_new() {
        let mut history = History::new(Arc::new(Mutex::new(Settings::default())));
        let first = detection("hello", box_at(10.0, 10.0, 80.0, 20.0));
        assert!(history.is_new(&first));

        let repeat = detection("hello", box_at(11.0, 11.0, 80.0, 20.0));
        assert!(!history.is_new(&repeat));
    }

    #[test]
    fn the_same_text_somewhere_else_of_the_same_size_is_not_new() {
        let mut history = History::new(Arc::new(Mutex::new(Settings::default())));
        assert!(history.is_new(&detection("hello", box_at(10.0, 10.0, 80.0, 20.0))));

        let elsewhere = detection("hello", box_at(600.0, 400.0, 80.0, 20.0));
        assert!(!history.is_new(&elsewhere));
    }

    #[test]
    fn the_same_text_in_a_different_size_is_new() {
        let mut history = History::new(Arc::new(Mutex::new(Settings::default())));
        assert!(history.is_new(&detection("hello", box_at(10.0, 10.0, 80.0, 20.0))));

        let bigger = detection("hello", box_at(600.0, 400.0, 400.0, 120.0));
        assert!(history.is_new(&bigger));
    }

    /// The thresholds can be changed while the app is running, and the
    /// change takes effect on the next detection.
    #[test]
    fn the_thresholds_can_be_changed_while_the_app_runs() {
        let settings = Arc::new(Mutex::new(Settings::default()));
        let mut history = History::new(Arc::clone(&settings));
        assert!(history.is_new(&detection("hello", box_at(10.0, 10.0, 80.0, 20.0))));

        // About the same size but not quite, and somewhere else, so by
        // default it counts as new.
        let similar = detection("hello", box_at(600.0, 400.0, 100.0, 24.0));
        assert!(history.is_new(&similar));

        // Lower the bar and the same detection is no longer new.
        if let Ok(mut settings) = settings.lock() {
            settings.dimension = 0.5;
        }
        let again = detection("hello", box_at(600.0, 400.0, 100.0, 24.0));
        assert!(!history.is_new(&again));
    }

    #[test]
    fn different_text_is_always_new() {
        let mut history = History::new(Arc::new(Mutex::new(Settings::default())));
        assert!(history.is_new(&detection("hello", box_at(10.0, 10.0, 80.0, 20.0))));

        let other = detection("goodbye", box_at(10.0, 10.0, 80.0, 20.0));
        assert!(history.is_new(&other));
    }
}
