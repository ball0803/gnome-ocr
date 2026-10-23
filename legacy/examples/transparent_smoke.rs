//! Minimal reproducer: a fullscreen red rectangle that is click-through.
//!
//! Run:    cargo run --release --example transparent_smoke
//! Exit:   Ctrl+C in the terminal that launched it.
//! Success: you see your real desktop with a red box hovering in the middle,
//!           and clicks pass through everywhere except the box.
//! Failure: you see solid black (the surface is opaque) or the box never
//!           appears (input region swallowed everything).
//!
//! This is a stand-alone smoke test for the pattern; it does NOT depend on
//! gnome-ocr's renderer, daemon, IPC, or config.

use gtk::prelude::*;
use gtk::{gdk, Application, ApplicationWindow, CssProvider};

const APP_ID: &str = "dev.local.gnome-ocr.smoke";

fn main() {
    let app = Application::builder().application_id(APP_ID).build();

    app.connect_activate(move |app| {
        if let Some(display) = gdk::Display::default() {
            eprintln!("[smoke] GDK display type: {}", display.type_().name());
        } else {
            eprintln!("[smoke] no GDK display");
        }
        // 1. CSS — a single rule. z13gui uses this exact string and it works on
        //    Mutter/Wayland. Matches the CSS node "window" of GtkWindow.
        let css = CssProvider::new();
        css.load_from_string("window { background: transparent; }");
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

        // 2. Undecorated, non-resizable, non-focusable window. The "not focusable"
        //    part is crucial: it stops Mutter from putting the window in the
        //    focus stack at all, which is what makes the rest of the screen
        //    still clickable for normal work.
        let win = ApplicationWindow::builder()
            .application(app)
            .title("gnome-ocr transparent smoke")
            .decorated(false)
            .resizable(false)
            .can_focus(false)
            .focusable(false)
            .build();

        // 3. Drawing area: the only widget that paints. We clear to fully
        //    transparent and then draw a single semi-opaque red rectangle.
        let da = gtk::DrawingArea::new();
        da.set_draw_func(|_area, cr, w, h| {
            use std::any::type_name_of_val;
            eprintln!("[smoke] cairo target type: {}", type_name_of_val(&cr));
            cr.set_operator(cairo::Operator::Source);
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
            if cr.paint().is_err() {
                return;
            }
            cr.set_operator(cairo::Operator::Over);
            let bw = 400.0_f64.min(w as f64);
            let bh = 300.0_f64.min(h as f64);
            let bx = (w as f64 - bw) / 2.0;
            let by = (h as f64 - bh) / 2.0;
            cr.set_source_rgba(1.0, 0.2, 0.2, 0.85);
            cr.rectangle(bx, by, bw, bh);
            let _ = cr.fill();
        });
        win.set_child(Some(&da));

        // 4. NOT fullscreen. Set size to monitor dimensions to see where Mutter
        //    places it and whether it covers the screen.
        if let Some(display) = gdk::Display::default() {
            if let Some(monitor_obj) = display.monitors().item(0u32) {
                if let Ok(monitor) = monitor_obj.downcast::<gdk::Monitor>() {
                    let geo = monitor.geometry();
                    win.set_default_size(geo.width(), geo.height());
                }
            }
        }

        // 5. The actual fix. After the surface is realized, push EMPTY input
        //    and opaque regions to wl_surface. Empty = "all input passes
        //    through" + "this surface is not opaque, use the alpha channel".
        //    Both are CORE Wayland protocols; Mutter has supported them since
        //    2013 (bug 707019) — totally independent of zwlr_layer_shell_v1
        //    which Mutter refuses to implement.
        win.connect_realize(|w| {
            if let Some(surface) = w.surface() {
                let empty = cairo::Region::create();
                surface.set_input_region(Some(&empty));
                surface.set_opaque_region(Some(&empty));
            }
        });

        win.present();
    });

    app.run();
}
