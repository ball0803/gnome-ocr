//! GTK4 overlay window: fullscreen, transparent, click-through by default.
//!
//! Pattern: z13gui. We use the core Wayland protocol `wl_surface::set_input_region`
//! (which Mutter has supported since 2013 — see GNOME bug 707019) instead of
//! `zwlr_layer_shell_v1` (which Mutter refuses to implement). The surface is
//! fullscreen on the primary monitor, the only child is a `GtkDrawingArea` that
//! paints OCR boxes and popups, and the input region starts empty so clicks
//! fall through to whatever is underneath the overlay.
//!
//! When OCR results are present, the input region is the bounding union of the
//! visible OCR boxes (plus an active popup); when idle, the region is empty and
//! the overlay is fully click-through.

use gtk::prelude::*;
use gtk::{gdk, Application, ApplicationWindow, CssProvider};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use crate::config::Config;
use crate::renderer::{self, RenderState};

/// A reference to the overlay window, kept so other code paths can flip the
/// input region as OCR results come and go.
pub struct Overlay {
    pub window: ApplicationWindow,
    state: Arc<Mutex<RenderState>>,
}

impl Overlay {
    pub fn build(
        app: &Application,
        config: Rc<Config>,
        state: Arc<Mutex<RenderState>>,
    ) -> Self {
        // ---- CSS -----------------------------------------------------------
        // A single rule. z13gui uses this exact string; aggressive multi-selector
        // CSS does not help (and previously made the surface look opaque).
        let provider = CssProvider::new();
        provider.load_from_string("window { background: transparent; }");
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

        // ---- Window --------------------------------------------------------
        // Undecorated, non-resizable, NON-FOCUSABLE. The non-focusable bit is
        // important: Mutter's focus stack treats a focused toplevel as "this
        // window is active" and the rest of the screen feels dead. By being
        // non-focusable, the overlay is just paint; input still works on the
        // rest of the screen because we set the input region to empty below.
        let window = ApplicationWindow::builder()
            .application(app)
            .title("gnome-ocr")
            .decorated(false)
            .resizable(false)
            .can_focus(false)
            .focusable(false)
            .build();

        // ---- Drawing area: the only thing that paints ----------------------
        let drawing = gtk::DrawingArea::new();
        drawing.set_hexpand(true);
        drawing.set_vexpand(true);
        drawing.set_can_focus(false);
        drawing.set_focusable(false);

        let cfg_draw = config.clone();
        let state_draw = state.clone();
        drawing.set_draw_func(move |_area, cr, w, h| {
            let rs = state_draw.lock().expect("state mutex");
            renderer::draw_state(cr, w, h, &rs, &cfg_draw);
        });
        window.set_child(Some(&drawing));

        // ---- Fullscreen on the primary monitor -----------------------------
        // Required. Without it Mutter does not give us a surface extent large
        // enough to cover the screen, and the overlay will look broken on
        // HiDPI or multi-monitor setups.
        if let Some(display) = gdk::Display::default() {
            if let Some(monitor_obj) = display.monitors().item(0u32) {
                if let Ok(monitor) = monitor_obj.downcast::<gdk::Monitor>() {
                    window.fullscreen_on_monitor(&monitor);
                }
            }
        }
        if !window.is_fullscreen() {
            window.fullscreen();
        }

        // ---- The actual fix: set the input region after the surface is up --
        // An empty cairo::Region means "all input passes through". We start
        // there. As OCR results and popups appear, idle_add_local will
        // re-compute the region from state.
        let state_for_input = state.clone();
        window.connect_realize(move |w| {
            if let Some(surface) = w.surface() {
                let empty = cairo::Region::create();
                surface.set_input_region(Some(&empty));
                surface.set_opaque_region(Some(&empty));
            }

            // Re-apply the region whenever state changes. Lightweight — just a
            // few rect ops. Runs every frame at the GTK tick rate.
            let w_weak = w.downgrade();
            let st = state_for_input.clone();
            glib::idle_add_local(move || {
                let Some(w) = w_weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                let Some(surface) = w.surface() else {
                    return glib::ControlFlow::Continue;
                };
                let empty = cairo::Region::create();
                let region = compute_input_region(&st.lock().expect("state mutex"));
                if region.is_empty() {
                    surface.set_input_region(Some(&empty));
                    surface.set_opaque_region(Some(&empty));
                } else {
                    surface.set_input_region(Some(&region));
                    // When OCR boxes are visible, declare their union as opaque
                    // so Mutter can skip compositing alpha math for those pixels.
                    surface.set_opaque_region(Some(&region));
                }
                glib::ControlFlow::Continue
            });
        });

        Self { window, state }
    }
}

/// Compute the input region from current render state. Empty when idle (overlay
/// is fully click-through). Includes OCR box rects and the popup rect when
/// either is visible.
fn compute_input_region(state: &RenderState) -> cairo::Region {
    let region = cairo::Region::create();
    for rs in &state.regions {
        let Some(result) = &rs.result else { continue };
        if rs.flash_until.is_some() {
            // During the "show region" flash, capture input on the box outline.
            let rect = cairo::RectangleInt::new(
                result.region.x as i32,
                result.region.y as i32,
                result.region.w as i32,
                result.region.h as i32,
            );
            let _ = region.union_rectangle(&rect);
        }
    }
    if let Some(popup) = &state.popup {
        let lines = popup.lines.len() + popup.readings.len() + popup.definitions.len();
        let height = (22 * lines + 32) as i32;
        let rect = cairo::RectangleInt::new(
            popup.anchor.x as i32,
            popup.anchor.y as i32,
            320,
            height,
        );
        let _ = region.union_rectangle(&rect);
    }
    region
}

// Allow `Overlay::build` to keep the public function name that main.rs uses,
// even though we now return an `Overlay` struct instead of a bare window.
#[allow(dead_code)]
pub fn build_window(
    app: &Application,
    config: Rc<Config>,
    state: Arc<Mutex<RenderState>>,
) -> ApplicationWindow {
    Overlay::build(app, config, state).window
}
