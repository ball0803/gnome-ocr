use gtk::prelude::*;
use gtk::{gdk, Application, ApplicationWindow, CssProvider};
use gtk4_layer_shell::{Edge, Layer, LayerShell};

const APP_ID: &str = "dev.local.gnome-ocr.layer-shell-smoke";

fn main() {
    let app = Application::builder().application_id(APP_ID).build();

    app.connect_activate(|app| {
        eprintln!(
            "[layer-smoke] GDK display type: {}",
            gdk::Display::default()
                .map(|d| d.type_().name().to_string())
                .unwrap_or_else(|| "none".into())
        );
        eprintln!(
            "[layer-smoke] zwlr-layer-shell-v1 supported: {}",
            gtk4_layer_shell::is_supported()
        );

        let css = CssProvider::new();
        css.load_from_string("window { background: transparent; }");
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

        let win = ApplicationWindow::builder()
            .application(app)
            .title("gnome-ocr layer-shell smoke")
            .decorated(false)
            .resizable(false)
            .can_focus(false)
            .focusable(false)
            .build();

        win.init_layer_shell();
        win.set_layer(Layer::Overlay);
        win.set_anchor(Edge::Top, true);
        win.set_anchor(Edge::Bottom, true);
        win.set_anchor(Edge::Left, true);
        win.set_anchor(Edge::Right, true);
        win.set_keyboard_mode(gtk4_layer_shell::KeyboardMode::None);

        let da = gtk::DrawingArea::new();
        da.set_draw_func(|_area, cr, w, h| {
            cr.set_operator(cairo::Operator::Source);
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
            let _ = cr.paint();
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
