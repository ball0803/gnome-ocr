//! The settings the user can change while the app is running.
//!
//! They are held behind a handle that the app and the pipeline share, so a
//! change takes effect on the next frame without anything being restarted.

use std::sync::{Arc, Mutex};

use gtk::gdk;

/// A handle to the settings.
pub type Shared = Arc<Mutex<Settings>>;

/// The key that has to be held down while the pointer moves, for the word
/// under it to be looked up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modifier {
    /// Any word under the pointer is looked up.
    None,
    /// The shift key.
    Shift,
    /// The control key.
    Control,
    /// The alt key.
    Alt,
}

impl Modifier {
    /// The key, as the window sees it.
    pub fn mask(self) -> gdk::ModifierType {
        match self {
            Modifier::None => gdk::ModifierType::empty(),
            Modifier::Shift => gdk::ModifierType::SHIFT_MASK,
            Modifier::Control => gdk::ModifierType::CONTROL_MASK,
            Modifier::Alt => gdk::ModifierType::ALT_MASK,
        }
    }

    /// The name of the key, as it is shown to the user.
    pub fn name(self) -> &'static str {
        match self {
            Modifier::None => "None",
            Modifier::Shift => "Shift",
            Modifier::Control => "Control",
            Modifier::Alt => "Alt",
        }
    }

    /// The modifier with that name, if there is one.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "None" => Some(Modifier::None),
            "Shift" => Some(Modifier::Shift),
            "Control" => Some(Modifier::Control),
            "Alt" => Some(Modifier::Alt),
            _ => None,
        }
    }
}

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
    /// The key that has to be held down while the pointer moves, for the
    /// word under it to be looked up.
    pub modifier: Modifier,
    /// How long the pointer has to rest over a word, in milliseconds, before
    /// it is looked up.
    pub scan_delay: u32,
    /// Whether the readings of the words are shown with the text, as
    /// 人気（にんき）のおかず.
    ///
    /// The readings come from Yomitan, which has to be running for them to
    /// be had. The text that is copied is the plain text either way.
    pub readings: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            overlap: 0.5,
            dimension: 0.9,
            clump: true,
            lists: true,
            readings: false,
            modifier: Modifier::Shift,
            scan_delay: 20,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The key is found by its name, so that the setting and the control
    /// that shows it agree.
    #[test]
    fn the_modifier_is_found_by_its_name() {
        for modifier in [
            Modifier::None,
            Modifier::Shift,
            Modifier::Control,
            Modifier::Alt,
        ] {
            assert_eq!(Modifier::from_name(modifier.name()), Some(modifier));
        }
        assert_eq!(Modifier::from_name("Meta"), None);
    }

    /// No modifier means every pointer position counts, so the empty set of
    /// modifiers is held by any state of the keyboard.
    #[test]
    fn no_modifier_is_held_by_any_state() {
        assert!(
            gdk::ModifierType::CONTROL_MASK
                .contains(Modifier::None.mask())
        );
        assert!(gdk::ModifierType::SHIFT_MASK.contains(Modifier::Shift.mask()));
        assert!(
            !gdk::ModifierType::SHIFT_MASK.contains(Modifier::Control.mask())
        );
    }
}
