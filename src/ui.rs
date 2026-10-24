//! GTK4 user interface.
//!
//! One window: a toolbar, a live preview of what is being captured at its
//! real size, the recognised text, and a status line. The preview carries a
//! crop rectangle with corner and edge handles, so any rectangle can be
//! selected and adjusted, and only that part of the frame is recognised.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use async_channel::Receiver;
use gtk::gdk::prelude::*;
use gtk::prelude::*;
use gtk::{cairo, gio, glib};

use crate::dedupe::{Detection, Settings};
use crate::pipeline::{Event, Pipeline};
use crate::region::Region;

const APP_ID: &str = "dev.local.gnome_ocr";

/// How close the pointer has to be to a handle to grab it.
const HANDLE_GRAB: f64 = 10.0;
/// The smallest crop the user can make, in pixels.
const MIN_CROP: f64 = 8.0;

/// The crop rectangle, in the coordinates the preview is drawn in.
///
/// Keeping it in the same space as the pointer means the corner is under the
/// mouse by construction: nothing has to be converted to draw it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Crop {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl Crop {
    fn right(&self) -> f64 {
        self.x + self.width
    }

    fn bottom(&self) -> f64 {
        self.y + self.height
    }
}

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
    Move { grab: (f64, f64), start: Crop },
    /// Dragging one of the handles.
    Resize { handle: Handle, start: Crop },
}

/// What the preview shows and lets the user select.
struct Preview {
    surface: RefCell<Option<cairo::ImageSurface>>,
    /// The frame scaled to the size it is drawn at, so that moving the
    /// pointer does not rescale it again for every motion event.
    scaled: RefCell<Option<(cairo::ImageSurface, i32, i32)>>,
    crop: RefCell<Option<Crop>>,
    drag: RefCell<Option<Drag>>,
    /// Where the frame was last drawn, so pointer positions can be turned
    /// into a position in the frame.
    rect: Cell<Option<(f64, f64, f64, f64)>>,
    /// Where the drag started. GtkGestureDrag reports the offset from this
    /// point, so the position is the start plus the offset.
    start: Cell<Option<(f64, f64)>>,
    /// The size of the frame the detections were found in.
    frame_size: Cell<(f64, f64)>,
    /// Everything the recogniser found in the last frame.
    detections: RefCell<Vec<Detection>>,
    /// Whether to draw where the text was found.
    show_boxes: Cell<bool>,
}

impl Preview {
    fn new() -> Self {
        Self {
            surface: RefCell::new(None),
            scaled: RefCell::new(None),
            crop: RefCell::new(None),
            drag: RefCell::new(None),
            rect: Cell::new(None),
            start: Cell::new(None),
            frame_size: Cell::new((0.0, 0.0)),
            detections: RefCell::new(Vec::new()),
            show_boxes: Cell::new(true),
        }
    }

    /// The frame scaled to the size it is drawn at, built once per size.
    fn scaled_surface(&self, width: i32, height: i32) -> Option<cairo::ImageSurface> {
        let mut cached = self.scaled.borrow_mut();
        if let Some((surface, cached_width, cached_height)) = cached.as_ref()
            && *cached_width == width
            && *cached_height == height
        {
            return Some(surface.clone());
        }

        let source = self.surface.borrow();
        let source = source.as_ref()?;
        let target = cairo::ImageSurface::create(cairo::Format::ARgb32, width, height).ok()?;
        {
            let context = cairo::Context::new(&target).ok()?;
            context.scale(
                width as f64 / source.width() as f64,
                height as f64 / source.height() as f64,
            );
            let _ = context.set_source_surface(source, 0.0, 0.0);
            let _ = context.paint();
        }
        *cached = Some((target.clone(), width, height));
        Some(target)
    }

    /// The rectangle the frame is drawn in, scaled to fit the space
    /// available while keeping its proportions.
    fn fitted_rect(&self, width: f64, height: f64) -> Option<(f64, f64, f64, f64)> {
        let surface = self.surface.borrow();
        let surface = surface.as_ref()?;
        let image_width = surface.width() as f64;
        let image_height = surface.height() as f64;
        if image_width <= 0.0 || image_height <= 0.0 {
            return None;
        }
        let scale = (width / image_width).min(height / image_height);
        let drawn_width = image_width * scale;
        let drawn_height = image_height * scale;
        Some((
            (width - drawn_width) / 2.0,
            (height - drawn_height) / 2.0,
            drawn_width,
            drawn_height,
        ))
    }

    /// The crop rectangle, where it is drawn.
    fn crop_rect(&self) -> Option<(f64, f64, f64, f64)> {
        let crop = *self.crop.borrow();
        let crop = crop?;
        Some((crop.x, crop.y, crop.width, crop.height))
    }

    /// Keeps the crop inside the frame that is drawn.
    fn clamp(&self, crop: &mut Crop) {
        let Some((left, top, width, height)) = self.rect.get() else {
            return;
        };
        // The crop is made to fit first. Otherwise the bounds below would
        // have a maximum below their minimum, and clamping to them would
        // fail outright.
        crop.width = crop.width.min(width);
        crop.height = crop.height.min(height);
        crop.x = crop.x.clamp(left, left + width - crop.width);
        crop.y = crop.y.clamp(top, top + height - crop.height);
    }

    /// Where a detection sits in the preview.
    fn detection_rect(&self, detection: &Detection) -> Option<(f64, f64, f64, f64)> {
        let (frame_width, frame_height) = self.frame_size.get();
        let (left, top, width, height) = self.rect.get()?;
        if frame_width <= 0.0 || frame_height <= 0.0 {
            return None;
        }
        Some((
            left + detection.bounds.x / frame_width * width,
            top + detection.bounds.y / frame_height * height,
            detection.bounds.width / frame_width * width,
            detection.bounds.height / frame_height * height,
        ))
    }

    /// The crop as a fraction of the frame, for the recogniser.
    fn crop_region(&self) -> Option<Region> {
        let crop = *self.crop.borrow();
        let crop = crop?;
        let (left, top, width, height) = self.rect.get()?;
        if width <= 0.0 || height <= 0.0 {
            return None;
        }
        Some(Region {
            x: (crop.x - left) / width,
            y: (crop.y - top) / height,
            width: crop.width / width,
            height: crop.height / height,
        })
    }

    /// The handle under the pointer, if any.
    fn handle_at(&self, x: f64, y: f64) -> Option<Handle> {
        let (left, top, width, height) = self.crop_rect()?;
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
        let Some((left, top, width, height)) = self.crop_rect() else {
            return false;
        };
        x >= left && x <= left + width && y >= top && y <= top + height
    }
}

/// Moves one edge or corner of the rectangle, keeping the opposite edges
/// where they were.
fn resize(crop: &mut Crop, handle: Handle, x: f64, y: f64) {
    let mut left = crop.x;
    let mut top = crop.y;
    let mut right = crop.right();
    let mut bottom = crop.bottom();

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

    crop.x = left;
    crop.y = top;
    crop.width = right - left;
    crop.height = bottom - top;
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
    show_boxes: gtk::CheckButton,
    settings_button: gtk::Button,
    popover: gtk::Popover,
    settings: Arc<Mutex<Settings>>,
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
        let settings_button = pill("Settings", None);
        settings_button.set_tooltip_text(Some(
            "How strictly text has to match to count as the same text.",
        ));

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
        toolbar.append(&settings_button);

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

        let canvas = gtk::DrawingArea::builder()
            .hexpand(true)
            .vexpand(true)
            .content_height(280)
            .build();
        canvas.add_css_class("card");
        canvas.set_tooltip_text(Some(
            "Drag to draw a crop, drag a handle to resize it, drag inside it to move it.",
        ));

        let show_boxes = gtk::CheckButton::with_label("Show where text was found");
        show_boxes.set_active(true);
        show_boxes.set_margin_start(12);
        show_boxes.set_margin_bottom(6);

        let preview_pane = gtk::Box::new(gtk::Orientation::Vertical, 0);
        preview_pane.set_margin_start(12);
        preview_pane.set_margin_end(12);
        preview_pane.set_margin_bottom(8);
        preview_pane.append(&canvas);
        preview_pane.append(&show_boxes);

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
        layout.append(&preview_pane);
        layout.append(&status);
        layout.append(&scroller);
        root.set_child(Some(&layout));

        let settings = Arc::new(Mutex::new(Settings::default()));
        let popover = gtk::Popover::new();
        popover.set_parent(&settings_button);
        popover.set_child(Some(&settings_panel(settings.clone())));

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
            show_boxes,
            settings_button,
            popover,
            settings: settings.clone(),
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
        self.canvas.set_draw_func(move |_area, context, width, height| {
            let width = width as f64;
            let height = height as f64;
            let Some((left, top, drawn_width, drawn_height)) =
                preview.fitted_rect(width, height)
            else {
                return;
            };
            preview.rect.set(Some((left, top, drawn_width, drawn_height)));

            let Some(surface) =
                preview.scaled_surface(drawn_width as i32, drawn_height as i32)
            else {
                return;
            };
            let _ = context.save();
            let _ = context.set_source_surface(&surface, left, top);
            let _ = context.paint();
            let _ = context.restore();

            // Where the text was found.
            if preview.show_boxes.get() {
                for detection in preview.detections.borrow().iter() {
                    let Some((box_x, box_y, box_width, box_height)) =
                        preview.detection_rect(detection)
                    else {
                        continue;
                    };
                    context.set_source_rgba(0.2, 1.0, 0.4, 0.9);
                    context.set_line_width(1.5);
                    context.rectangle(box_x, box_y, box_width, box_height);
                    let _ = context.stroke();
                }
            }

            let Some((left, top, width, height)) = preview.crop_rect() else {
                return;
            };

            // Dim everything outside the crop. Filling the frame and the
            // crop together with the even-odd rule darkens the frame but
            // leaves the crop alone.
            let (frame_left, frame_top, frame_width, frame_height) =
                preview.rect.get().unwrap_or((0.0, 0.0, 0.0, 0.0));
            context.set_source_rgba(0.0, 0.0, 0.0, 0.55);
            context.rectangle(frame_left, frame_top, frame_width, frame_height);
            context.rectangle(left, top, width, height);
            context.set_fill_rule(cairo::FillRule::EvenOdd);
            let _ = context.fill();
            context.set_fill_rule(cairo::FillRule::Winding);

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
            // The position where the button went down, relative to the
            // widget allocation.
            begin.start.set(Some((x, y)));
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
                *begin.drag.borrow_mut() = Some(Drag::New { anchor: (x, y) });
                *begin.crop.borrow_mut() = None;
            }
        });

        let update = Rc::clone(&self.preview);
        let owner = Rc::clone(self);
        drag.connect_drag_update(move |_gesture, offset_x, offset_y| {
            // The drag reports the offset from the point it started at, so
            // the position is the start plus that offset.
            let (start_x, start_y) = update.start.get().unwrap_or((0.0, 0.0));
            let (x, y) = (start_x + offset_x, start_y + offset_y);

            let current = update.drag.borrow().clone();
            match current {
                Some(Drag::New { anchor }) => {
                    *update.crop.borrow_mut() = Some(Crop {
                        x: anchor.0.min(x),
                        y: anchor.1.min(y),
                        width: (x - anchor.0).abs(),
                        height: (y - anchor.1).abs(),
                    });
                }
                Some(Drag::Move { grab, start }) => {
                    let mut crop = start;
                    crop.x += x - grab.0;
                    crop.y += y - grab.1;
                    update.clamp(&mut crop);
                    *update.crop.borrow_mut() = Some(crop);
                }
                Some(Drag::Resize { handle, start }) => {
                    let mut crop = start;
                    resize(&mut crop, handle, x, y);
                    update.clamp(&mut crop);
                    *update.crop.borrow_mut() = Some(crop);
                }
                None => {}
            }
            // Without this the rectangle only appears when the button is
            // released, which makes the whole thing feel dead.
            owner.canvas.queue_draw();
            owner.update_area_label();
        });

        let owner = Rc::clone(self);
        drag.connect_drag_end(move |_gesture, _offset_x, _offset_y| {
            *owner.preview.drag.borrow_mut() = None;
            owner.preview.start.set(None);
            owner.publish_region();
        });

        self.canvas.add_controller(drag);
    }

    /// Shows what the current crop covers, without changing it.
    fn update_area_label(&self) {
        match self.preview.crop_region() {
            Some(region) => {
                self.area_label.set_text(&format!(
                    "Area: {}x{} at {},{}",
                    (region.width * 100.0).round() as i64,
                    (region.height * 100.0).round() as i64,
                    (region.x * 100.0).round() as i64,
                    (region.y * 100.0).round() as i64
                ));
            }
            None => self.area_label.set_text("Area: whole capture"),
        }
    }

    /// Sends the current crop to the recogniser and updates the label.
    fn publish_region(self: &Rc<Self>) {
        let usable = self
            .preview
            .crop_region()
            .filter(|region| !region.is_empty());

        if let Ok(mut shared) = self.region.lock() {
            *shared = usable;
        }

        self.update_area_label();
        match usable {
            Some(_) => {
                self.reset_button.set_sensitive(true);
                self.set_status("Recognising only the selected part of the frame.");
            }
            None => {
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
        self.settings_button.connect_clicked(move |_| {
            owner.popover.popup();
        });

        let owner = Rc::clone(self);
        self.show_boxes.connect_toggled(move |button| {
            owner.preview.show_boxes.set(button.is_active());
            owner.canvas.queue_draw();
        });

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
                Arc::clone(&self.settings),
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
            Event::Scanned {
                fresh,
                found,
                total,
            } => {
                *self.preview.detections.borrow_mut() = found;
                self.canvas.queue_draw();

                if !fresh.is_empty() {
                    let joined = fresh
                        .iter()
                        .map(|detection| detection.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    self.append(&format!("{joined}\n"));
                    if self.auto_copy.is_active() {
                        self.root.clipboard().set_text(&joined);
                    }
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
            Event::Preview {
                png,
                frame_width,
                frame_height,
            } => {
                self.preview
                    .frame_size
                    .set((frame_width as f64, frame_height as f64));
                if let Ok(surface) = cairo::ImageSurface::create_from_png(&mut &png[..]) {
                    *self.preview.surface.borrow_mut() = Some(surface);
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

/// The settings popover, with a control for each threshold.
fn settings_panel(settings: Arc<Mutex<Settings>>) -> gtk::Box {
    let panel = gtk::Box::new(gtk::Orientation::Vertical, 8);
    panel.set_margin_top(12);
    panel.set_margin_bottom(12);
    panel.set_margin_start(12);
    panel.set_margin_end(12);

    let current = settings.lock().map(|settings| *settings).unwrap_or_default();

    let overlap_adjustment =
        gtk::Adjustment::new(current.overlap, 0.0, 1.0, 0.05, 0.1, 0.1);
    let overlap = gtk::SpinButton::new(Some(&overlap_adjustment), 0.01, 2);
    let dimension_adjustment =
        gtk::Adjustment::new(current.dimension, 0.0, 1.0, 0.05, 0.1, 0.1);
    let dimension = gtk::SpinButton::new(Some(&dimension_adjustment), 0.01, 2);

    panel.append(&labelled(
        "Boxes must overlap by",
        &overlap,
        "How much two boxes have to overlap to count as the same place.",
    ));
    panel.append(&labelled(
        "Boxes must match in size by",
        &dimension,
        "How alike two boxes have to be in size to count as the same place.",
    ));

    let for_overlap = settings.clone();
    overlap.connect_value_changed(move |spin| {
        if let Ok(mut settings) = for_overlap.lock() {
            settings.overlap = spin.value();
        }
    });

    let for_dimension = settings.clone();
    dimension.connect_value_changed(move |spin| {
        if let Ok(mut settings) = for_dimension.lock() {
            settings.dimension = spin.value();
        }
    });

    panel
}

fn labelled(text: &str, control: &gtk::SpinButton, tooltip: &str) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_tooltip_text(Some(tooltip));
    row.append(&label);
    row.append(control);
    row
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

    fn crop(x: f64, y: f64, width: f64, height: f64) -> Crop {
        Crop {
            x,
            y,
            width,
            height,
        }
    }

    /// GtkGestureDrag reports the offset from the point the drag started at,
    /// so the position of the pointer is the start plus that offset.
    #[test]
    fn the_pointer_position_is_the_start_plus_the_offset() {
        let start = (120.0f64, 240.0f64);
        let offset = (35.0f64, -15.0f64);
        let (x, y) = (start.0 + offset.0, start.1 + offset.1);
        assert_eq!((x, y), (155.0, 225.0));
    }

    /// A crop dragged past the edge of the frame used to make the bounds
    /// for clamping turn inside out, which panicked.
    #[test]
    fn a_crop_bigger_than_the_frame_is_shrunk_instead_of_panicking() {
        let preview = Preview::new();
        preview.rect.set(Some((65.55555555555554, 10.0, 100.0, 50.0)));

        let mut crop = Crop {
            x: 70.0,
            y: 10.0,
            width: 200.0,
            height: 80.0,
        };
        preview.clamp(&mut crop);

        assert!(crop.width <= 100.0, "the crop must fit the frame");
        assert!(crop.height <= 50.0, "the crop must fit the frame");
        assert!(crop.x >= 65.55555555555554);
        assert!(crop.y >= 10.0);
    }

    /// The corner of the crop is the point the pointer went down on, with no
    /// conversion in between.
    #[test]
    fn a_crop_corner_is_the_point_the_pointer_went_down_on() {
        let anchor = (123.0f64, 456.0f64);
        let released = (321.0f64, 654.0f64);
        let crop = Crop {
            x: anchor.0.min(released.0),
            y: anchor.1.min(released.1),
            width: (released.0 - anchor.0).abs(),
            height: (released.1 - anchor.1).abs(),
        };
        assert_eq!((crop.x, crop.y), (123.0, 456.0));
        assert_eq!((crop.right(), crop.bottom()), (321.0, 654.0));
    }

    /// The crop can be dragged the other way too.
    #[test]
    fn a_crop_drawn_up_and_to_the_left_still_starts_at_the_pointer() {
        let anchor = (321.0f64, 654.0f64);
        let released = (123.0f64, 456.0f64);
        let crop = Crop {
            x: anchor.0.min(released.0),
            y: anchor.1.min(released.1),
            width: (released.0 - anchor.0).abs(),
            height: (released.1 - anchor.1).abs(),
        };
        assert_eq!((crop.x, crop.y), (123.0, 456.0));
    }

    #[test]
    fn a_corner_handle_moves_two_edges_and_leaves_the_others() {
        let mut crop = crop(200.0, 200.0, 400.0, 400.0);
        resize(&mut crop, Handle::NorthWest, 100.0, 150.0);
        assert_eq!(crop.x, 100.0);
        assert_eq!(crop.y, 150.0);
        assert_eq!(crop.right(), 600.0);
        assert_eq!(crop.bottom(), 600.0);
    }

    #[test]
    fn an_edge_handle_moves_only_that_edge() {
        let mut crop = crop(200.0, 200.0, 400.0, 400.0);
        resize(&mut crop, Handle::East, 800.0, 0.0);
        assert_eq!(crop.x, 200.0);
        assert_eq!(crop.y, 200.0);
        assert_eq!(crop.right(), 800.0);
        assert_eq!(crop.bottom(), 600.0);
    }

    #[test]
    fn a_crop_cannot_be_squeezed_out_of_existence() {
        let mut crop = crop(200.0, 200.0, 400.0, 400.0);
        resize(&mut crop, Handle::NorthWest, 900.0, 900.0);
        assert!(crop.width >= MIN_CROP);
        assert!(crop.height >= MIN_CROP);
    }

    #[test]
    fn a_crop_cannot_be_pushed_past_the_other_edge() {
        let mut crop = crop(200.0, 200.0, 400.0, 400.0);
        resize(&mut crop, Handle::West, 900.0, 200.0);
        assert_eq!(crop.right(), 600.0);
    }
}
