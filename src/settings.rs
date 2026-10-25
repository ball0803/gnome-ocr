//! The settings the user can change while the app is running.
//!
//! They are held behind a handle that the app and the pipeline share, so a
//! change takes effect on the next frame without anything being restarted.

use std::sync::{Arc, Mutex};

/// A handle to the settings.
pub type Shared = Arc<Mutex<Settings>>;

/// The settings themselves.
#[derive(Debug, Clone, Copy)]
pub struct Settings {
    /// How much two boxes have to overlap to count as the same place.
    ///
    /// This is the usual threshold for "the same object" in detection work.
    pub overlap: f64,
    /// How alike two boxes have to be in size to count as the same place
    /// even when they do not overlap.
    pub dimension: f64,
    /// Whether lines that belong to the same paragraph are joined into one
    /// line, rather than each keeping its own line.
    pub clump: bool,
    /// Whether the runs of short, aligned entries are gathered into one list,
    /// rather than each entry being kept on its own.
    pub lists: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            overlap: 0.5,
            dimension: 0.9,
            clump: true,
            lists: true,
        }
    }
}

/// A handle to a fresh set of settings.
pub fn shared() -> Shared {
    Arc::new(Mutex::new(Settings::default()))
}

/// The settings, or the defaults if they cannot be read.
pub fn read(settings: &Shared) -> Settings {
    settings.lock().map(|settings| *settings).unwrap_or_default()
}
