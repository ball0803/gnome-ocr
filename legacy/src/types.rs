//! Shared types for OCR results, config, and IPC.
//!
//! These mirror the JSON shape produced by qocrd's stdout and consumed by the overlay renderer.

use serde::{Deserialize, Serialize};

/// A point on screen in absolute Wayland coordinates (origin top-left of the primary output).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// Axis-aligned bounding box (rotation = 0). All units in pixels.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub struct Aabb {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Single character / symbol inside a word.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub text: String,
    pub box_: Aabb,
    pub aabb: Aabb,
}

/// Single word inside a line. Has optional whitespace box for word spacing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Word {
    pub text: String,
    pub symbols: Vec<Symbol>,
    pub box_: Aabb,
    pub aabb: Aabb,
    #[serde(rename = "has_space_after")]
    pub has_space_after: bool,
    #[serde(default, rename = "whitespace_box")]
    pub whitespace_box: Aabb,
}

/// Single OCR line (text + per-symbol bboxes). Tells us vertical/horizontal orientation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Line {
    pub text: String,
    pub words: Vec<Word>,
    pub box_: Aabb,
    pub aabb: Aabb,
    #[serde(rename = "is_vertical")]
    pub is_vertical: bool,
}

/// Region of the screen that was OCR'd. Coordinates are in screen-space (x,y) and
/// output-space (X,Y = region origin in that monitor's coords).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Region {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub x_in_monitor: i32,
    pub y_in_monitor: i32,
}

/// One scan result, output by qocrd on stdout (null-byte-delimited).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrResult {
    pub lines: Vec<Line>,
    pub monitor: String,
    pub region: Region,
}

/// `{"unchanged":true}` reply from qocrd when the captured frame matches the previous one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnchangedResult {
    pub unchanged: bool,
}

/// IPC commands the overlay accepts from `gnome-ocr-cli`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// Begin region-select drag, then OCR the picked region.
    Scan {
        #[serde(default = "default_japanese")]
        japanese: bool,
        #[serde(default)]
        fullscreen: bool,
    },
    /// Re-OCR all currently-displayed regions.
    Rescan,
    /// Hide text overlays but keep regions visible.
    ClearOverlay,
    /// Clear all overlays + regions.
    ClearAll,
    /// Briefly flash the currently-tracked region.
    ShowRegion,
    /// Trigger a Yomitan lookup at absolute screen coords.
    TriggerPopup {
        x: i32,
        y: i32,
        monitor: String,
    },
    /// Toggle hover-lookup mode.
    HoverOn,
    HoverOff,
    /// Toggle a boolean config value (dot-notation path).
    ToggleConfig { key: String },
    /// Read a config value (dot-notation path).
    GetConfig { key: String },
}

fn default_japanese() -> bool {
    true
}