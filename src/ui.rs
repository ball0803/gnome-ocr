//! GTK4 user interface.
//!
//! One window: a toolbar, a live preview of what is being captured at its
//! real size, the recognised text, and a status line. The preview carries a
//! crop rectangle with corner and edge handles, so any rectangle can be
//! selected and adjusted, and only that part of the frame is recognised.

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

/// How close the pointer has to be to a handle to grab it.
const HANDLE_GRAB: f64 = 10.0;
/// The smallest crop the user can make, as a fraction of the frame.
const MIN_CROP: f64 = 0.01;

/// The eight handles of a crop rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handle {
    NorthWest,
    North,
    NorthEast,
    East,
    SouthEast,
    South,
    SouthWest,
    West,
}

/// What the pointer is doing.
#[derive(Clone)]
enum Drag {
    /// Dragging out a new rectangle, from the point it started at.
    New { anchor: (f64, f64) },
    /// Moving the whole rectangle.
    Move { grab: (f64, f64), start: Region },
    /// Dragging one of the handles.
    Resize { handle: Handle, start: Region },
}

/// What the preview shows and lets the user select.
struct Preview {
    surface: RefCell<Option<cairo::ImageSurface>>,
    crop: RefCell<Option<Region>>,
    drag: RefCell<Option<Drag>>,
}

impl Preview {
    fn new() -> Self {
        Self {
            surface: RefCell::new(None),
            crop: RefCell::new(None),
            drag: RefCell::new(None),
        }
    }

    /// The size of the captured frame in pixels.
    fn size(&self) -> (f64, f64) {
        match self.surface.borrow().as_ref() {
            Some(surface) => (surface.width() as f64, surface.height() as f64),
            None => (0.0, 0.0),
        }
    }

    /// The crop rectangle in pixels.
    fn crop_pixels(&self) -> Option<(f64, f64, f64, f64)> {
        let crop = *self.crop.borrow();
        let crop = crop?;
        let (width, height) = self.size();
        Some((
            crop.x * width,
            crop.y * height,
            crop.width * width,
            crop.height * height,
        ))
    }

    /// The handle under the pointer, if any.
    fn handle_at(&self, x: f64, y: f64) -> Option<Handle> {
        let (left, top, width, height) = self.crop_pixels()?;
        let right = left + width;
        let bottom = top + height;
        let middle_x = (left + right) / 2.0;
        let middle_y = (top + bottom) / 2.0;

        let handles = [
            (Handle::NorthWest, left, top),
            (Handle::North, middle_x, top),
            (Handle::NorthEast, right, top),
            (Handle::East, right, middle_y),
            (Handle::SouthEast, right, bottom),
            (Handle::South, middle_x, bottom),
            (Handle::SouthWest, left, bottom),
            (Handle::West, left, middle_y),
        ];
        handles
            .into_iter()
            .find(|(_, handle_x, handle_y)| {
                (x - handle_x).abs() <= HANDLE_GRAB && (y - handle_y).abs() <= HANDLE_GRAB
            })
            .map(|(handle, _, _)| handle)
    }

    /// True when the point is inside the crop rectangle.
    fn inside_crop(&self, x: f64, y: f64) -> bool {
        let Some((left, top, width, height)) = self.crop_pixels() else {
            return false;
        };
        x >= left && x <= left + width && y >= top && y <= top + height
    }
}

/// Moves one edge or corner of the rectangle, keeping the opposite edges
/// where they were.
fn resize(region: &mut Region, handle: Handle, x: f64, y: f64) {
    let mut left = region.x;
    let mut top = region.y;
    let mut right = region.x + region.width;
    let mut bottom = region.y + region.height;

    match handle {
        Handle::NorthWest => {
            left = x.min(right - MIN_CROP);
            top = y.min(bottom - MIN_CROP);
        }
        Handle::North => top = y.min(bottom - MIN_CROP),
        Handle::NorthEast => {
            right = x.max(left + MIN_CROP);
            top = y.min(bottom - MIN_CROP);
        }
        Handle::East => right = x.max(left + MIN_CROP),
        Handle::SouthEast => {
            right = x.max(left + MIN_CROP);
            bottom = y.max(top + MIN_CROP);
        }
        Handle::South => bottom = y.max(top + MIN_CROP),
        Handle::SouthWest => {
            left = x.min(right - MIN_CROP);
            bottom = y.max(top + MIN_CROP);
        }
        Handle::West => left = x.min(right - MIN_CROP),
    }

    region.x = left;
    region.y = top;
    region.width = right - left;
    region.height = bottom - top;
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
    canvas: gtk::DrawingArea,
    start_button: gtk::Button,
    stop_button: gtk::Button,
    change_button: gtk::Button,
    reset_button: gtk::Button,
    copy_button: gtk::Button,
    clear_button: gtk::Button,
    auto_copy: gtk::Switch,
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
            .default_width(900)
            .default_height(820)
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

        let canvas = gtk::DrawingArea::builder().hexpand(true).vexpand(true).build();
        canvas.add_css_class("card");
        canvas.set_tooltip_text(Some(
            "Drag to draw a crop, drag a handle to resize it, drag inside it to move it.",
        ));

        let preview_scroller = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&canvas)
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(8)
            .build();

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
        layout.append(&preview_scroller);
        layout.append(&status);
        layout.append(&scroller);
        root.set_child(Some(&layout));

        let (events, inbox) = async_channel::bounded::<Event>(64);

        let window = Rc::new(Self {
            root,
            text_view,
            buffer,
            canvas: canvas.clone(),
            start_button,
            stop_button,
            change_button,
            reset_button,
            copy_button,
            clear_button,
            auto_copy,
            source_label,
            area_label,
            status,
            preview: Rc::new(Preview::new()),
            region: Arc::new(Mutex::new(None)),
            events,
            restore_token: Arc::new(Mutex::new(None)),
            pipeline: RefCell::new(None),
            japanese_only,
            lines: RefCell::new(Vec::new()),
        });

        window.draw_preview();
        window.wire_drag();
        window.wire();
        window.consume(inbox);

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

    /// Paints the captured frame at its own size, with the crop on top.
    fn draw_preview(self: &Rc<Self>) {
        let preview = Rc::clone(&self.preview);
        self.canvas.set_draw_func(move |_area, context, _width, _height| {
            let surface = preview.surface.borrow();
            let Some(surface) = surface.as_ref() else {
                return;
            };
            let _ = context.save();
            let _ = context.set_source_surface(surface, 0.0, 0.0);
            let _ = context.paint();
            let _ = context.restore();

            let Some((left, top, width, height)) = preview.crop_pixels() else {
                return;
            };

            // Dim everything outside the crop.
            context.set_source_rgba(0.0, 0.0, 0.0, 0.55);
            let (frame_width, frame_height) = preview.size();
            context.rectangle(0.0, 0.0, frame_width, frame_height);
            let _ = context.fill_preserve();
            context.set_source_rgba(0.0, 0.0, 0.0, 0.0);
            context.rectangle(left, top, width, height);
            let _ = context.fill();

            context.set_source_rgb(0.35, 0.75, 1.0);
            context.set_line_width(1.5);
            context.rectangle(left, top, width, height);
            let _ = context.stroke();

            // The eight handles.
            let right = left + width;
            let bottom = top + height;
            let middle_x = (left + right) / 2.0;
            let middle_y = (top + bottom) / 2.0;
            let handles = [
                (left, top),
                (middle_x, top),
                (right, top),
                (right, middle_y),
                (right, bottom),
                (middle_x, bottom),
                (left, bottom),
                (left, middle_y),
            ];
            context.set_source_rgb(1.0, 1.0, 1.0);
            for (handle_x, handle_y) in handles {
                context.rectangle(handle_x - 4.0, handle_y - 4.0, 8.0, 8.0);
                let _ = context.fill_preserve();
                context.set_source_rgb(0.1, 0.1, 0.1);
                context.set_line_width(1.0);
                let _ = context.stroke();
                context.set_source_rgb(1.0, 1.0, 1.0);
            }
        });
    }

    /// Lets the user draw, move and resize the crop rectangle.
    fn wire_drag(self: &Rc<Self>) {
        let drag = gtk::GestureDrag::new();

        let begin = Rc::clone(&self.preview);
        drag.connect_drag_begin(move |_gesture, x, y| {
            let (width, height) = begin.size();
            if width <= 0.0 || height <= 0.0 {
                return;
            }
            if let Some(handle) = begin.handle_at(x, y) {
                let start = begin.crop.borrow().unwrap_or_default();
                *begin.drag.borrow_mut() = Some(Drag::Resize { handle, start });
            } else if begin.inside_crop(x, y) {
                let start = begin.crop.borrow().unwrap_or_default();
                *begin.drag.borrow_mut() = Some(Drag::Move {
                    grab: (x, y),
                    start,
                });
            } else {
                *begin.drag.borrow_mut() = Some(Drag::New {
                    anchor: (x / width, y / height),
                });
                *begin.crop.borrow_mut() = None;
            }
        });

        let update = Rc::clone(&self.preview);
        drag.connect_drag_update(move |_gesture, x, y| {
            let (width, height) = update.size();
            if width <= 0.0 || height <= 0.0 {
                return;
            }
            let current = update.drag.borrow().clone();
            match current {
                Some(Drag::New { anchor }) => {
                    let region = Region::from_corners(anchor, (x / width, y / height));
                    *update.crop.borrow_mut() = Some(region);
                }
                Some(Drag::Move { grab, start }) => {
                    let dx = (x - grab.0) / width;
                    let dy = (y - grab.1) / height;
                    let mut region = start;
                    region.x = (region.x + dx).clamp(0.0, 1.0 - region.width);
                    region.y = (region.y + dy).clamp(0.0, 1.0 - region.height);
                    *update.crop.borrow_mut() = Some(region);
                }
                Some(Drag::Resize { handle, start }) => {
                    let mut region = start;
                    resize(&mut region, handle, x / width, y / height);
                    *update.crop.borrow_mut() = Some(region);
                }
                None => {}
            }
        });

        let owner = Rc::clone(self);
        drag.connect_drag_end(move |_gesture, _x, _y| {
            *owner.preview.drag.borrow_mut() = None;
            owner.publish_region();
        });

        self.canvas.add_controller(drag);
    }

    /// Sends the current crop to the recogniser and updates the label.
    fn publish_region(self: &Rc<Self>) {
        let crop = *self.preview.crop.borrow();
        let usable = crop.filter(|crop| !crop.is_empty());

        if let Ok(mut shared) = self.region.lock() {
            *shared = usable;
        }

        match usable {
            Some(region) => {
                self.area_label.set_text(&format!(
                    "Area: {}x{} at {},{}",
                    (region.width * 100.0).round() as i64,
                    (region.height * 100.0).round() as i64,
                    (region.x * 100.0).round() as i64,
                    (region.y * 100.0).round() as i64
                ));
                self.reset_button.set_sensitive(true);
                self.set_status("Recognising only the selected part of the frame.");
            }
            None => {
                self.area_label.set_text("Area: whole capture");
                self.reset_button.set_sensitive(false);
                self.set_status("Recognising the whole capture again.");
            }
        }
        self.canvas.queue_draw();
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
            *owner.preview.crop.borrow_mut() = None;
            owner.publish_region();
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
                    let width = surface.width();
                    let height = surface.height();
                    *self.preview.surface.borrow_mut() = Some(surface);
                    self.canvas.set_content_width(width);
                    self.canvas.set_content_height(height);
                    self.canvas.queue_draw();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn region(x: f64, y: f64, width: f64, height: f64) -> Region {
        Region {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn a_corner_handle_moves_two_edges_and_leaves_the_others() {
        let mut crop = region(0.2, 0.2, 0.4, 0.4);
        resize(&mut crop, Handle::NorthWest, 0.1, 0.15);
        assert!((crop.x - 0.1).abs() < 1e-9);
        assert!((crop.y - 0.15).abs() < 1e-9);
        assert!((crop.width - 0.5).abs() < 1e-9);
        assert!((crop.height - 0.45).abs() < 1e-9);
    }

    #[test]
    fn an_edge_handle_moves_only_that_edge() {
        let mut crop = region(0.2, 0.2, 0.4, 0.4);
        resize(&mut crop, Handle::East, 0.8, 0.0);
        assert!((crop.x - 0.2).abs() < 1e-9);
        assert!((crop.y - 0.2).abs() < 1e-9);
        assert!((crop.width - 0.6).abs() < 1e-9);
        assert!((crop.height - 0.4).abs() < 1e-9);
    }

    #[test]
    fn a_crop_cannot_be_squeezed_out_of_existence() {
        let mut crop = region(0.2, 0.2, 0.4, 0.4);
        resize(&mut crop, Handle::NorthWest, 0.9, 0.9);
        assert!(crop.width >= MIN_CROP);
        assert!(crop.height >= MIN_CROP);
    }

    #[test]
    fn a_crop_cannot_be_pushed_past_the_other_edge() {
        let mut crop = region(0.2, 0.2, 0.4, 0.4);
        resize(&mut crop, Handle::West, 0.9, 0.2);
        assert!((crop.x + crop.width - 0.6).abs() < 1e-9);
    }
}
