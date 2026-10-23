//! Region selection stub.
//!
//! Real interactive drag-selection in the overlay requires capturing input dynamically, which
//! GTK4's signal-callback model (Fn, no &mut captures) doesn't support cleanly. v1 sidesteps
//! this by:
//!
//!   - Full-monitor scan via IPC (`gnome-ocr-cli scan --monitor <name>`)
//!   - Region selection via GNOME's built-in `gnome-screenshot -a` (which works on GNOME)
//!     and piping the file through to OCR
//!
//! Future revs can replace this with a poll-based selector that reads the cursor position
//! from a glib timer.

use crate::types::Aabb;
use gtk::prelude::*;
use gtk::Widget;

pub struct RegionSelector;

impl RegionSelector {
    pub fn new(_widget: &impl IsA<Widget>, _on_commit: impl Fn(Aabb) + 'static, _on_cancel: impl Fn() + 'static) -> Self {
        Self
    }

    pub fn begin(&self) {}
    pub fn is_active(&self) -> bool { false }
    pub fn cancel(&self) {}
    pub fn current_rect(&self) -> Option<Aabb> { None }
}