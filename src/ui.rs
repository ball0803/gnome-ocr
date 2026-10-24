//! GTK4 user interface.
//!
//! One window: a toolbar, a live preview of what is being captured with a
//! draggable region, the recognised text, and a status line. Dragging on the
//! preview restricts recognition to that part of the frame.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use async_channel::Receiver;
use gtk::gdk::prelude::*;
use gtk::prelude::*;
use gtk::{cairo, gio, glib};

use crate::pipeline::{Event, Pipeline};
use crate::region::Region;

const APP_ID: &str = "dev.local.gnome_ocr";

/// Two points in the preview, in fractions of the image.
type Corners = ((f64, f64), (f64, f64));

/// What the preview pane shows and lets the user select.
struct Preview {
    surface: RefCell<Option<cairo::ImageSurface>>,
    region: RefCell<Option<Region>>,
    drag: RefCell<Option<Corners>>,
}

impl Preview {
    fn new() -> Self {
        Self {
            surface: RefCell::new(None),
            region: RefCell::new(None),
            drag: RefCell::new(None),
        }
    }

    /// The rectangle the image is drawn in, given the space available.
    fn image_rect(&self, width: f64, height: f64) -> Option<(f64, f64, f64, f64)> {
        let surface = self.surface.borrow();
        let surface = surface.as_ref()?;
        let image_width = surface.width() as f64;
        let image_height = surface.height() as f64;
        if image_width <= 0.0 || image_height <= 0.0 {
            return None;
        }
        let scale = (width / image_width)
            .min(height / image_height)
            .min(1.0);
        let drawn_width = image_width * scale;
        let drawn_height = image_height * scale;
        Some((
            (width - drawn_width) / 2.0,
            (height - drawn_height) / 2.0,
            drawn_width,
            drawn_height,
        ))
    }
}

pub fn run(japanese_only: bool) -> glib::ExitCode {
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    app.connect_activate(move |app| {
        Window::build(app, japanese_only);
    });

    app.run()
}

struct Window {
    root: gtk::ApplicationWindow,
    text_view: gtk::TextView,
    buffer: gtk::TextBuffer,
    start_button: gtk::Button,
    stop_button: gtk::Button,
    change_button: gtk::Button,
    reset_button: gtk::Button,
    copy_button: gtk::Button,
    clear_button: gtk::Button,
    auto_copy: gtk::Switch,
    canvas: gtk::DrawingArea,
    source_label: gtk::Label,
    area_label: gtk::Label,
    status: gtk::Label,
    preview: Rc<Preview>,
    region: Arc<Mutex<Option<Region>>>,
    events: async_channel::Sender<Event>,
    restore_token: Arc<Mutex<Option<String>>>,
    pipeline: RefCell<Option<Pipeline>>,
    japanese_only: bool,
    lines: RefCell<Vec<String>>,
}

impl Window {
    fn build(app: &gtk::Application, japanese_only: bool) -> Rc<Self> {
        let root = gtk::ApplicationWindow::builder()
            .application(app)
            .title("GNOME OCR")
            .default_width(860)
            .default_height(780)
            .build();

        let start_button = pill("Start OCR", Some("suggested-action"));
        let stop_button = pill("Stop", Some("destructive-action"));
        stop_button.set_sensitive(false);
        let change_button = pill("Change window", None);
        change_button.set_tooltip_text(Some(
            "Capture a different screen or window. The portal asks which one to use.",
        ));
        change_button.set_sensitive(false);
        let reset_button = pill("Whole capture", None);
        reset_button.set_tooltip_text(Some("Recognise the whole frame again."));
        reset_button.set_sensitive(false);

        let copy_button = pill("Copy all", None);
        let clear_button = pill("Clear", None);

        let auto_copy = gtk::Switch::builder()
            .valign(gtk::Align::Center)
            .active(true)
            .build();

        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        toolbar.set_margin_top(12);
        toolbar.set_margin_bottom(8);
        toolbar.set_margin_start(12);
        toolbar.set_margin_end(12);
        toolbar.append(&start_button);
        toolbar.append(&stop_button);
        toolbar.append(&change_button);
        toolbar.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        toolbar.append(&gtk::Label::new(Some("Auto-copy new text")));
        toolbar.append(&auto_copy);
        toolbar.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        toolbar.append(&copy_button);
        toolbar.append(&clear_button);

        let source_label = gtk::Label::new(Some("Not capturing yet."));
        source_label.set_xalign(0.0);
        source_label.add_css_class("dim-label");
        source_label.set_margin_start(14);

        let area_label = gtk::Label::new(Some("Area: whole capture"));
        area_label.set_xalign(0.0);
        area_label.add_css_class("dim-label");
        area_label.set_margin_start(14);

        let info_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        info_row.set_margin_bottom(6);
        info_row.append(&source_label);
        info_row.append(&area_label);
        info_row.append(&reset_button);

        let preview = Rc::new(Preview::new());
        let canvas = gtk::DrawingArea::builder()
            .hexpand(true)
            .vexpand(false)
            .content_height(240)
            .build();
        canvas.add_css_class("card");
        canvas.set_tooltip_text(Some(
            "Drag over the part of the screen to recognise only that part.",
        ));

        let status = gtk::Label::new(Some("Idle — press Start OCR to begin."));
        status.set_xalign(0.0);
        status.add_css_class("dim-label");
        status.set_margin_start(14);
        status.set_margin_end(14);
        status.set_margin_top(6);
        status.set_margin_bottom(6);

        let text_view = gtk::TextView::new();
        text_view.set_editable(false);
        text_view.set_cursor_visible(true);
        text_view.set_monospace(true);
        text_view.set_wrap_mode(gtk::WrapMode::WordChar);
        text_view.set_left_margin(12);
        text_view.set_right_margin(12);
        text_view.set_top_margin(8);
        text_view.set_bottom_margin(8);
        text_view.add_css_class("card");
        let buffer = text_view.buffer();

        let scroller = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&text_view)
            .has_frame(true)
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(12)
            .build();

        let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
        layout.append(&toolbar);
        layout.append(&info_row);
        layout.append(&canvas);
        layout.append(&status);
        layout.append(&scroller);
        root.set_child(Some(&layout));

        let (events, inbox) = async_channel::bounded::<Event>(64);

        let window = Rc::new(Self {
            root,
            text_view,
            buffer,
            start_button,
            stop_button,
            change_button,
            reset_button,
            copy_button,
            clear_button,
            auto_copy,
            canvas: canvas.clone(),
            source_label,
            area_label,
            status,
            preview: Rc::clone(&preview),
            region: Arc::new(Mutex::new(None)),
            events,
            restore_token: Arc::new(Mutex::new(None)),
            pipeline: RefCell::new(None),
            japanese_only,
            lines: RefCell::new(Vec::new()),
        });

        window.draw_preview(&canvas);
        window.wire_drag(&canvas);
        window.wire();
        window.consume(inbox);

        // Closing the window stops the capture, otherwise the portal session
        // is torn down with the process.
        let closing = Rc::clone(&window);
        window.root.connect_close_request(move |_| {
            if let Some(pipeline) = closing.pipeline.borrow().as_ref() {
                pipeline.stop_capture();
            }
            gtk::glib::Propagation::Proceed
        });

        window.root.present();
        window
    }

    /// Paints the preview and the selected region.
    fn draw_preview(self: &Rc<Self>, canvas: &gtk::DrawingArea) {
        let preview = Rc::clone(&self.preview);
        canvas.set_draw_func(move |_area, context, width, height| {
            let width = width as f64;
            let height = height as f64;

            context.set_source_rgb(0.12, 0.12, 0.12);
            let _ = context.paint();

            let Some((x, y, drawn_width, drawn_height)) = preview.image_rect(width, height) else {
                return;
            };

            if let Some(surface) = preview.surface.borrow().as_ref() {
                let _ = context.save();
                context.translate(x, y);
                context.scale(
                    drawn_width / surface.width() as f64,
                    drawn_height / surface.height() as f64,
                );
                let _ = context.set_source_surface(surface, 0.0, 0.0);
                let _ = context.paint();
                let _ = context.restore();
            }

            // Dim everything outside the selection.
            let region = preview.region.borrow().or_else(|| preview.drag.borrow().map(|d| {
                Region::from_corners(d.0, d.1)
            }));
            if let Some(region) = region
                && !region.is_empty()
            {
                let left = x + region.x * drawn_width;
                let top = y + region.y * drawn_height;
                let selection_width = region.width * drawn_width;
                let selection_height = region.height * drawn_height;

                context.set_source_rgba(0.0, 0.0, 0.0, 0.55);
                context.rectangle(x, y, drawn_width, drawn_height);
                let _ = context.fill_preserve();
                context.set_source_rgba(0.0, 0.0, 0.0, 0.0);
                context.rectangle(left, top, selection_width, selection_height);
                let _ = context.fill();

                context.set_source_rgb(0.35, 0.75, 1.0);
                context.set_line_width(2.0);
                context.rectangle(left, top, selection_width, selection_height);
                let _ = context.stroke();
            }
        });
    }

    /// Lets the user drag a rectangle over the preview.
    fn wire_drag(self: &Rc<Self>, canvas: &gtk::DrawingArea) {
        let drag = gtk::GestureDrag::new();

        let begin = Rc::clone(&self.preview);
        drag.connect_drag_begin(move |_gesture, x, y| {
            *begin.drag.borrow_mut() = Some(((x, y), (x, y)));
        });

        let update = Rc::clone(&self.preview);
        drag.connect_drag_update(move |_gesture, x, y| {
            let mut current = update.drag.borrow_mut();
            if let Some((start, _)) = *current {
                *current = Some((start, (x, y)));
            }
        });

        let owner = Rc::clone(self);
        drag.connect_drag_end(move |_gesture, _x, _y| {
            let finished = owner.preview.drag.borrow_mut().take();
            let Some((start, end)) = finished else {
                return;
            };
            let region = Region::from_corners(start, end);
            if region.is_empty() {
                owner.canvas_queue_draw();
                return;
            }
            *owner.preview.region.borrow_mut() = Some(region);
            if let Ok(mut shared) = owner.region.lock() {
                *shared = Some(region);
            }
            owner.area_label.set_text(&format!(
                "Area: {:.0}%,{:.0}% to {:.0}%,{:.0}%",
                region.x * 100.0,
                region.y * 100.0,
                (region.x + region.width) * 100.0,
                (region.y + region.height) * 100.0
            ));
            owner.reset_button.set_sensitive(true);
            owner.canvas_queue_draw();
            owner.set_status("Recognising only the selected part of the frame.");
        });
        canvas.add_controller(drag);
    }

    fn wire(self: &Rc<Self>) {
        let owner = Rc::clone(self);
        self.start_button
            .connect_clicked(move |_| owner.start_capture());

        let owner = Rc::clone(self);
        self.stop_button.connect_clicked(move |_| {
            if let Some(pipeline) = owner.pipeline.borrow().as_ref() {
                pipeline.stop_capture();
            }
            owner.stop_button.set_sensitive(false);
            owner.set_status("Stopping…");
        });

        let owner = Rc::clone(self);
        self.change_button
            .connect_clicked(move |_| owner.change_source());

        let owner = Rc::clone(self);
        self.reset_button.connect_clicked(move |_| {
            *owner.preview.region.borrow_mut() = None;
            if let Ok(mut region) = owner.region.lock() {
                *region = None;
            }
            owner.area_label.set_text("Area: whole capture");
            owner.reset_button.set_sensitive(false);
            owner.canvas_queue_draw();
            owner.set_status("Recognising the whole capture again.");
        });

        let owner = Rc::clone(self);
        self.copy_button
            .connect_clicked(move |_| owner.copy_all());

        let owner = Rc::clone(self);
        self.clear_button
            .connect_clicked(move |_| owner.clear());

        let owner = Rc::clone(self);
        self.auto_copy.connect_active_notify(move |switch| {
            let state = if switch.is_active() { "on" } else { "off" };
            owner.set_status(&format!("Auto-copy is {state}."));
        });
    }

    fn consume(self: &Rc<Self>, inbox: Receiver<Event>) {
        let owner = Rc::clone(self);
        glib::spawn_future_local(async move {
            while let Ok(event) = inbox.recv().await {
                owner.handle(event);
            }
        });
    }

    fn change_source(self: &Rc<Self>) {
        if self.pipeline.borrow().is_none() {
            return;
        }
        self.set_status("Choose the new screen or window in the dialog…");
        if let Some(pipeline) = self.pipeline.borrow().as_ref() {
            pipeline.change_source();
        }
    }

    fn start_capture(self: &Rc<Self>) {
        if self.pipeline.borrow().is_none() {
            self.clear();
            let pipeline = Pipeline::start(
                self.events.clone(),
                self.japanese_only,
                Arc::clone(&self.restore_token),
                Arc::clone(&self.region),
            );
            *self.pipeline.borrow_mut() = Some(pipeline);
        }

        self.start_button.set_sensitive(false);
        self.stop_button.set_sensitive(true);
        self.root.present();
        self.set_status("Starting the capture…");

        if let Some(pipeline) = self.pipeline.borrow().as_ref() {
            pipeline.start_capture();
        }
    }

    fn handle(&self, event: Event) {
        match event {
            Event::Started => {
                self.start_button.set_sensitive(false);
                self.stop_button.set_sensitive(true);
                self.change_button.set_sensitive(true);
                self.set_status("Capturing — new text appears below.");
            }
            Event::Stopped => {
                self.start_button.set_sensitive(true);
                self.stop_button.set_sensitive(false);
                self.change_button.set_sensitive(false);
                self.set_status("Stopped.");
            }
            Event::Fresh { lines, total } => {
                let joined = lines.join("\n");
                self.append(&format!("{joined}\n"));
                if self.auto_copy.is_active() {
                    self.root.clipboard().set_text(&joined);
                }
                self.set_status(&format!("{total} line(s) captured."));
            }
            Event::Failed(err) => {
                self.set_status(&format!("Problem: {err}"));
                self.start_button.set_sensitive(true);
                self.stop_button.set_sensitive(false);
                self.change_button.set_sensitive(false);
            }
            Event::Source(description) => {
                self.source_label.set_text(&description);
            }
            Event::Preview(png) => {
                if let Ok(surface) = cairo::ImageSurface::create_from_png(&mut &png[..]) {
                    *self.preview.surface.borrow_mut() = Some(surface);
                    self.canvas_queue_draw();
                }
            }
        }
    }

    fn append(&self, text: &str) {
        self.lines.borrow_mut().push(text.to_owned());
        let all = self.lines.borrow().concat();
        self.buffer.set_text(&all);
        self.scroll_to_end();
    }

    fn copy_all(&self) {
        let all = self.lines.borrow().concat();
        let trimmed = all.trim_end();
        if trimmed.is_empty() {
            self.set_status("Nothing to copy yet.");
            return;
        }
        self.root.clipboard().set_text(trimmed);
        self.set_status("Copied everything to the clipboard.");
    }

    fn clear(&self) {
        self.lines.borrow_mut().clear();
        self.buffer.set_text("");
        self.set_status("Cleared.");
    }

    fn set_status(&self, text: &str) {
        self.status.set_text(text);
    }

    fn canvas_queue_draw(&self) {
        self.canvas.queue_draw();
    }

    fn scroll_to_end(&self) {
        let end = self.buffer.end_iter();
        let mark = self.buffer.create_mark(None, &end, false);
        self.text_view.scroll_to_mark(&mark, 0.0, true, 0.0, 0.0);
    }
}

fn pill(label: &str, style: Option<&str>) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    button.add_css_class("pill");
    if let Some(style) = style {
        button.add_css_class(style);
    }
    button
}
