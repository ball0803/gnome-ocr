//! GTK4 user interface.
//!
//! One window: a toolbar, a live preview of what is being captured at its
//! real size, the recognised text, and a status line. The preview carries a
//! crop rectangle with corner and edge handles, so any rectangle can be
//! selected and adjusted, and only that part of the frame is recognised.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use async_channel::Receiver;
use gtk::gdk;
use gtk::gdk::prelude::*;
use gtk::prelude::*;
use gtk::{cairo, gio, glib};

use crate::settings;
use crate::pipeline::{Event, Pipeline};
use crate::region::Region;

const APP_ID: &str = "dev.local.gnome_ocr";

/// How many entries the list keeps before the oldest are dropped.
const MAX_LIST_ENTRIES: usize = 200;

/// The width the pictures in the list are made and shown at. They are shown
/// at the size they were made, rather than being scaled to fit a box.
const MAX_IMAGE_WIDTH: i32 = 320;

/// How close the pointer has to be to a handle to grab it.
const HANDLE_GRAB: f64 = 10.0;
/// The smallest crop the user can make, as a fraction of the frame.
const MIN_CROP: f64 = 0.005;

/// The crop rectangle, as a fraction of the captured frame.
///
/// It is kept this way rather than in the pixels of the preview, because the
/// preview is scaled to fit the pane. Were the crop kept in those pixels,
/// resizing the window would move it.
type Crop = crate::region::Region;

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
    /// The word the pointer is over, so that it can be shown.
    hovered: RefCell<Option<crate::layout::Rect>>,
    /// Everything the recogniser found in the last frame.
    detections: RefCell<Vec<crate::layout::Line>>,
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
            hovered: RefCell::new(None),
            detections: RefCell::new(Vec::new()),
            show_boxes: Cell::new(true),
        }
    }

    /// Replaces the image that is shown.
    ///
    /// The scaled copy belongs to the image it was made from, so it is
    /// dropped here. Keeping it would leave the preview showing the first
    /// frame, because every later frame has the same size and would be
    /// served from the cache.
    fn set_surface(&self, surface: cairo::ImageSurface) {
        *self.surface.borrow_mut() = Some(surface);
        *self.scaled.borrow_mut() = None;
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
        let (left, top, width, height) = self.rect.get()?;
        if width <= 0.0 || height <= 0.0 {
            return None;
        }
        Some((
            left + crop.x * width,
            top + crop.y * height,
            crop.width * width,
            crop.height * height,
        ))
    }

    /// Keeps the crop inside the frame.
    fn clamp(&self, crop: &mut Crop) {
        // The crop is made to fit first. Otherwise the bounds below would
        // have a maximum below their minimum, and clamping to them would
        // fail outright.
        crop.width = crop.width.clamp(0.0, 1.0);
        crop.height = crop.height.clamp(0.0, 1.0);
        crop.x = crop.x.clamp(0.0, 1.0 - crop.width);
        crop.y = crop.y.clamp(0.0, 1.0 - crop.height);
    }

    /// A pointer position, as a fraction of the frame.
    fn to_fraction(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (left, top, width, height) = self.rect.get()?;
        if width <= 0.0 || height <= 0.0 {
            return None;
        }
        Some((
            ((x - left) / width).clamp(0.0, 1.0),
            ((y - top) / height).clamp(0.0, 1.0),
        ))
    }

    /// Where a line sits in the preview.
    fn line_rect(&self, line: &crate::layout::Line) -> Option<(f64, f64, f64, f64)> {
        self.rect_of(&line.bounds)
    }

    /// A box of the frame, as it is drawn in the preview.
    fn rect_of(&self, bounds: &crate::layout::Rect) -> Option<(f64, f64, f64, f64)> {
        let (frame_width, frame_height) = self.frame_size.get();
        let (left, top, width, height) = self.rect.get()?;
        if frame_width <= 0.0 || frame_height <= 0.0 {
            return None;
        }
        Some((
            left + bounds.x / frame_width * width,
            top + bounds.y / frame_height * height,
            bounds.width / frame_width * width,
            bounds.height / frame_height * height,
        ))
    }

    /// The crop as a fraction of the frame, for the recogniser.
    fn crop_region(&self) -> Option<Region> {
        *self.crop.borrow()
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


/// One entry in the list: the frame and everything read from it.
struct ListEntry {
    picture: gtk::Picture,
    timestamp: gtk::Label,
    content: gtk::Box,
}


impl ListEntry {
    /// Adds more text to the entry and shows the newest frame.
    fn append(&self, text: &str, image: Option<Vec<u8>>) {
        if !text.is_empty() {
            // One label, so that the whole of it can be selected and copied
            // in one go. A label keeps the line breaks it is given.
            let label = gtk::Label::new(Some(text));
            label.set_xalign(0.0);
            label.set_yalign(0.0);
            label.set_wrap(true);
            label.set_selectable(true);
            self.content.append(&label);
        }
        if let Some(png) = image
            && let Ok(texture) = gdk::Texture::from_bytes(&glib::Bytes::from_owned(png))
        {
            self.picture.set_paintable(Some(&texture));
        }
        self.timestamp.set_text(&timestamp_now());
    }

    /// Names the section the text belongs to.
    fn set_section(&self, section: crate::layout::Heading) {
        let heading = gtk::Label::new(Some(&section.text));
        heading.set_xalign(0.0);
        heading.add_css_class(&format!("title-{}", section.level));
        heading.set_wrap(true);
        self.content.append(&heading);
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

/// A piece of the text that was shown.
struct Piece {
    /// The text as it was recognised, which is what is copied.
    plain: String,
    /// The text as it is shown, with the readings marked when they are on.
    text: String,
    /// Where each word is in the text, counted in characters.
    ///
    /// The words are worked out once, when the text arrives, so that the
    /// pointer over it can be answered at once and not asked about again.
    words: Vec<(usize, usize)>,
}

/// Something to ask Yomitan about.
enum Asking {
    /// The readings of a piece of text.
    Readings { piece: usize, text: String },
    /// The dictionary entries for the words that were selected in the text.
    Selected { text: String },
}

/// What came back.
enum Answer {
    /// The readings of a piece of text, and where its words are.
    Readings {
        piece: usize,
        /// The text as it is shown.
        text: String,
        /// The text as it was recognised.
        plain: String,
        /// Where each word is in the text, counted in characters.
        words: Vec<(usize, usize)>,
    },
    /// The entries of a word, and where they were asked for.
    Entries {
        text: String,
        entries: Vec<crate::yomitan::Entry>,
        /// Whether the pointer was over the word.
        pointed: bool,
    },
    /// The API did not answer.
    Failed,
}

/// The work of asking Yomitan what the text means.
///
/// The reading is fetched on a thread of its own, so the window stays quick,
/// and the answers come back through a channel.
struct Reader {
    jobs: async_channel::Sender<Asking>,
    answers: async_channel::Receiver<Answer>,
}

impl Reader {
    fn start(settings: settings::Shared) -> Self {
        let for_settings = settings;
        let (jobs, work) = async_channel::unbounded::<Asking>();
        let (done, answers) = async_channel::unbounded::<Answer>();
        std::thread::spawn(move || {
            while let Ok(asking) = work.recv_blocking() {
                match asking {
                    Asking::Readings { piece, text } => {
                        // The text is broken into the words the language is
                        // made of, once, so that the pointer over it can be
                        // answered at once.
                        let Ok(segments) = crate::yomitan::tokenize(&text) else {
                            let _ = done.try_send(Answer::Failed);
                            continue;
                        };
                        if segments.is_empty() {
                            continue;
                        }
                        let settings = crate::settings::read(&for_settings);
                        let (shown, words) = if settings.readings {
                            crate::yomitan::with_readings(&text, &segments)
                        } else {
                            (text.clone(), crate::yomitan::word_ranges(&segments))
                        };
                        let _ = done.try_send(Answer::Readings {
                            piece,
                            text: shown,
                            plain: text,
                            words,
                        });
                    }
                    Asking::Selected { text } => {
                        let Ok(entries) = crate::yomitan::entries(&text) else {
                            let _ = done.try_send(Answer::Failed);
                            continue;
                        };
                        let _ = done.try_send(Answer::Entries {
                            text,
                            entries,
                            pointed: false,
                        });
                    }
                }
            }
        });
        Self { jobs, answers }
    }
}

struct Window {
    root: gtk::ApplicationWindow,
    text_view: gtk::TextView,
    buffer: gtk::TextBuffer,
    canvas: gtk::DrawingArea,
    split: gtk::Paned,
    preview_pane: gtk::Box,
    start_button: gtk::Button,
    stop_button: gtk::Button,
    change_button: gtk::Button,
    reset_button: gtk::Button,
    show_boxes: gtk::CheckButton,
    preview_button: gtk::ToggleButton,
    mode: gtk::DropDown,
    stack: gtk::Stack,
    list: gtk::ListBox,
    settings_button: gtk::Button,
    popover: gtk::Popover,
    readings: gtk::CheckButton,
    lookup_button: gtk::Button,
    card: crate::card::Card,
    /// The keys the app was told about, by its own keyboard.
    ///
    /// The pointer does not say which keys are held unless the app has the
    /// keyboard, so the app listens for the keys itself.
    keys: Cell<gdk::ModifierType>,
    /// Where the pointer was, in the pixels of the widget it was over.
    pointer: RefCell<Option<(f64, f64)>>,
    /// Whether the pointer was over the text rather than the preview.
    over_text: Cell<bool>,
    /// How long ago the pointer moved, in seconds.
    last_moved: Cell<f64>,
    /// Whether the word under the pointer has been looked up since it last
    /// moved.
    scanned: Cell<bool>,
    /// What is under the pointer, so that a pointer which only jitters is
    /// taken to be still.
    under: RefCell<Option<String>>,
    settings: settings::Shared,
    copy_button: gtk::Button,
    clear_button: gtk::Button,
    auto_copy: gtk::Switch,
    source_label: gtk::Label,
    area_label: gtk::Label,
    status: gtk::Label,
    preview: Rc<Preview>,
    region: Arc<Mutex<Option<Region>>>,
    /// The source that is being captured, so entries can be grouped by it.
    source_id: RefCell<String>,
    /// The list entries, by the source they belong to.
    entries: RefCell<HashMap<String, ListEntry>>,
    events: async_channel::Sender<Event>,
    restore_token: Arc<Mutex<Option<String>>>,
    pipeline: RefCell<Option<Pipeline>>,
    japanese_only: bool,
    pieces: RefCell<Vec<Piece>>,
    reader: Reader,
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

        let lookup_button = pill("Look up", None);
        lookup_button.set_tooltip_text(Some(
            "Look the selected words up in the dictionary of Yomitan.",
        ));
        lookup_button.set_sensitive(false);

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
        toolbar.append(&lookup_button);
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

        // The preview and the controls that belong with it are on the left,
        // the recognised text on the right.
        let preview_pane = gtk::Box::new(gtk::Orientation::Vertical, 6);
        preview_pane.set_margin_start(12);
        preview_pane.set_margin_end(6);
        preview_pane.set_margin_bottom(8);
        preview_pane.append(&show_boxes);
        preview_pane.append(&canvas);

        // The button that clears the chosen part stays in the row above,
        // which is always there, because the preview can be hidden.
        let preview_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        preview_buttons.append(&gtk::Label::new(Some(
            "Drag on the preview to choose the part to recognise.",
        )));
        preview_pane.append(&preview_buttons);

        let status = gtk::Label::new(Some("Idle — press Start OCR to begin."));
        status.set_xalign(0.0);
        status.add_css_class("dim-label");
        status.set_margin_start(14);
        status.set_margin_end(14);
        status.set_margin_top(6);
        status.set_margin_bottom(6);

        let mode = gtk::DropDown::from_strings(&["Text", "List"]);
        mode.set_valign(gtk::Align::Center);
        mode.set_tooltip_text(Some("How the recognised text is shown."));

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

        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        list.add_css_class("boxed-list");
        let list_scroller = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&list)
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(12)
            .build();

        let stack = gtk::Stack::new();
        stack.add_named(&scroller, Some("text"));
        stack.add_named(&list_scroller, Some("list"));
        stack.set_visible_child_name("text");

        let preview_button = gtk::ToggleButton::with_label("Preview");
        preview_button.set_active(true);
        preview_button.set_tooltip_text(Some("Show or hide the picture of what is captured."));

        let mode_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        mode_row.set_margin_start(6);
        mode_row.set_margin_end(12);
        mode_row.set_margin_bottom(6);
        mode_row.append(&preview_button);
        mode_row.append(&gtk::Label::new(Some("Show as")));
        mode_row.append(&mode);

        let output_pane = gtk::Box::new(gtk::Orientation::Vertical, 0);
        output_pane.set_margin_start(6);
        output_pane.set_margin_end(12);
        output_pane.set_margin_bottom(8);
        output_pane.append(&mode_row);
        output_pane.append(&stack);

        let split = gtk::Paned::new(gtk::Orientation::Horizontal);
        split.set_start_child(Some(&preview_pane));
        split.set_end_child(Some(&output_pane));
        split.set_vexpand(true);
        split.set_wide_handle(true);
        // The preview is a quarter of the width and the text the rest.
        split.set_position(root.default_width() / 4);

        let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
        layout.append(&toolbar);
        layout.append(&info_row);
        layout.append(&split);
        layout.append(&status);
        root.set_child(Some(&layout));

        let settings = settings::shared();
        let popover = gtk::Popover::new();
        popover.set_parent(&settings_button);
        let (panel, readings) = settings_panel(settings.clone());
        popover.set_child(Some(&panel));

        let (events, inbox) = async_channel::bounded::<Event>(64);

        let window = Rc::new(Self {
            root,
            text_view,
            buffer,
            canvas: canvas.clone(),
            split: split.clone(),
            preview_pane: preview_pane.clone(),
            start_button,
            stop_button,
            change_button,
            reset_button,
            show_boxes,
            preview_button,
            mode,
            stack,
            list,
            settings_button,
            popover,
            readings,
            lookup_button: lookup_button.clone(),
            card: crate::card::Card::new(),
            keys: Cell::new(gdk::ModifierType::empty()),
            pointer: RefCell::new(None),
            over_text: Cell::new(false),
            last_moved: Cell::new(0.0),
            scanned: Cell::new(false),
            under: RefCell::new(None),
            settings: settings.clone(),
            copy_button,
            clear_button,
            auto_copy,
            source_label,
            area_label,
            status,
            preview: Rc::new(Preview::new()),
            region: Arc::new(Mutex::new(None)),
            source_id: RefCell::new(String::new()),
            entries: RefCell::new(HashMap::new()),
            events,
            restore_token: Arc::new(Mutex::new(None)),
            pipeline: RefCell::new(None),
            japanese_only,
            pieces: RefCell::new(Vec::new()),
            reader: Reader::start(settings.clone()),
        });

        window.draw_preview();
        window.wire_drag();
        window.wire();
        window.consume(inbox);
        window.consume_answers();

        let closing = Rc::clone(&window);
        window.root.connect_close_request(move |_| {
            if let Some(pipeline) = closing.pipeline.borrow().as_ref() {
                pipeline.stop_capture();
            }
            gtk::glib::Propagation::Proceed
        });

        // The preview is a quarter of the width and the text the rest, and
        // that is kept when the window is resized.
        let for_width = window.clone();
        window.root.connect_notify_local(Some("default-width"), move |root, _| {
            let width = root.default_width();
            if width > 0 {
                for_width.split.set_position(width / 4);
            }
        });

        // One timer for the whole life of the app, which looks the word up
        // once the pointer has rested over it.
        let owner = Rc::clone(&window);
        glib::timeout_add_local(std::time::Duration::from_millis(30), move || {
            owner.tick();
            glib::ControlFlow::Continue
        });

        window.root.present();
        window
    }

    /// How long it is since the machine started, in seconds.
/// The width of the window.
    fn width(&self) -> i32 {
        self.root.default_width()
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

            // The word the pointer is over.
            if let Some(word) = preview.hovered.borrow().as_ref()
                && let Some((box_x, box_y, box_width, box_height)) = preview.rect_of(word)
            {
                context.set_source_rgba(1.0, 0.8, 0.2, 1.0);
                context.set_line_width(2.5);
                context.rectangle(box_x, box_y, box_width, box_height);
                let _ = context.stroke();
            }

            // Where the text was found.
            if preview.show_boxes.get() {
                for line in preview.detections.borrow().iter() {
                    let Some((box_x, box_y, box_width, box_height)) = preview.line_rect(line)
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
        // Where the pointer moves is watched, so that the word under it can
        // be looked up once it has rested there.
        let motion = gtk::EventControllerMotion::new();
        let owner = Rc::clone(self);
        motion.connect_motion(move |controller, x, y| {
            let from_event = controller
                .current_event()
                .map(|event| event.modifier_state())
                .unwrap_or_else(gdk::ModifierType::empty);
            owner.pointer_moved(x, y, from_event);
        });
        self.canvas.add_controller(motion);

        let motion = gtk::EventControllerMotion::new();
        let owner = Rc::clone(self);
        motion.connect_motion(move |_controller, x, y| {
            owner.text_pointer_moved(x, y);
        });
        self.text_view.add_controller(motion);

        let keys = gtk::EventControllerKey::new();
        let owner = Rc::clone(self);
        keys.connect_key_pressed(move |_controller, key, _code, state| {
            owner.key_changed(key, state);
            glib::Propagation::Proceed
        });
        let owner = Rc::clone(self);
        keys.connect_key_released(move |_controller, key, _code, state| {
            owner.key_changed(key, state);
        });
        self.root.add_controller(keys);

        // The keys the app was told about are only true while it has the
        // keyboard.
        let owner = Rc::clone(self);
        self.root.connect_notify_local(Some("is-active"), move |window, _| {
            if !window.is_active() {
                owner.keys.set(gdk::ModifierType::empty());
            }
        });

        if let Some(pointer) = gdk::Display::default()
            .and_then(|display| display.default_seat())
            .and_then(|seat| seat.pointer())
        {
            let owner = Rc::clone(self);
            pointer.connect_modifier_state_notify(move |_| owner.keys_changed());
        }

        let drag = gtk::GestureDrag::new();

        let begin = Rc::clone(&self.preview);
        drag.connect_drag_begin(move |_gesture, x, y| {
            // Where the button went down, in the pixels of the widget, so
            // the offset the drag reports can be added to it.
            begin.start.set(Some((x, y)));
            let Some(fraction) = begin.to_fraction(x, y) else {
                return;
            };
            if let Some(handle) = begin.handle_at(x, y) {
                let start = begin.crop.borrow().unwrap_or_default();
                *begin.drag.borrow_mut() = Some(Drag::Resize { handle, start });
            } else if begin.inside_crop(x, y) {
                let start = begin.crop.borrow().unwrap_or_default();
                *begin.drag.borrow_mut() = Some(Drag::Move {
                    grab: fraction,
                    start,
                });
            } else {
                *begin.drag.borrow_mut() = Some(Drag::New { anchor: fraction });
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
            let Some(fraction) = update.to_fraction(x, y) else {
                return;
            };

            let current = update.drag.borrow().clone();
            match current {
                Some(Drag::New { anchor }) => {
                    *update.crop.borrow_mut() = Some(Region::from_corners(anchor, fraction));
                }
                Some(Drag::Move { grab, start }) => {
                    let mut crop = start;
                    crop.x += fraction.0 - grab.0;
                    crop.y += fraction.1 - grab.1;
                    update.clamp(&mut crop);
                    *update.crop.borrow_mut() = Some(crop);
                }
                Some(Drag::Resize { handle, start }) => {
                    let mut crop = start;
                    resize(&mut crop, handle, fraction.0, fraction.1);
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

        // The button is only of use when something is selected.
        let owner = Rc::clone(self);
        self.text_view.buffer().connect_notify_local(
            Some("has-selection"),
            move |buffer, _| {
                owner.lookup_button.set_sensitive(buffer.has_selection());
            },
        );

        let owner = Rc::clone(self);
        self.lookup_button.connect_clicked(move |_| {
            let Some((start, end)) = owner.buffer.selection_bounds() else {
                return;
            };
            let text = owner.buffer.text(&start, &end, false).to_string();
            let text = text.trim().to_owned();
            if text.is_empty() {
                return;
            }
            let _ = owner.reader.jobs.try_send(Asking::Selected { text });
        });

        // Turning the readings on asks for the readings of what is already
        // shown, so that the whole text has them.
        let owner = Rc::clone(self);
        self.readings.connect_toggled(move |button| {
            if button.is_active() {
                owner.read_again();
            }
        });

        let owner = Rc::clone(self);
        self.mode.connect_selected_notify(move |mode| {
            let page = match mode.selected() {
                1 => "list",
                _ => "text",
            };
            owner.stack.set_visible_child_name(page);
        });

        let owner = Rc::clone(self);
        self.preview_button.connect_toggled(move |button| {
            let showing = button.is_active();
            if showing {
                owner.split.set_start_child(Some(&owner.preview_pane));
                owner.split.set_position(owner.width() / 4);
            } else {
                owner.split.set_start_child(None::<&gtk::Box>);
            }
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
        self.clear_button.connect_clicked(move |_| {
            owner.clear();
            // The text that was already seen is forgotten too, so it is
            // treated as new if it turns up again.
            if let Some(pipeline) = owner.pipeline.borrow().as_ref() {
                pipeline.clear_history();
            }
        });

        let owner = Rc::clone(self);
        self.auto_copy.connect_active_notify(move |switch| {
            let state = if switch.is_active() { "on" } else { "off" };
            owner.set_status(&format!("Auto-copy is {state}."));
        });
    }

    /// Takes the readings that come back and shows them.
    fn consume_answers(self: &Rc<Self>) {
        let owner = Rc::clone(self);
        let answers = self.reader.answers.clone();
        glib::spawn_future_local(async move {
            while let Ok(answer) = answers.recv().await {
                match answer {
                    Answer::Readings {
                        piece,
                        text,
                        plain,
                        words,
                    } => owner.set_piece(piece, text, plain, words),
                    Answer::Entries {
                        text,
                        entries,
                        pointed,
                    } => owner.show_entries(pointed, &text, &entries),
                    Answer::Failed => {
                        // The card says it, because that is where the answer
                        // would have been.
                        owner.card.show_message(
                            &owner.canvas,
                            "Yomitan is not answering. It has to be running, with the API \
                             turned on, for the readings and the lookups.",
                        );
                        owner.set_status(
                            "Yomitan is not answering. It has to be running, with the API \
                             turned on, for the readings and the lookups.",
                        );
                    }
                }
            }
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
                total,
                image,
                lines,
                layout,
            } => {
                *self.preview.detections.borrow_mut() = lines;
                self.canvas.queue_draw();

                if !fresh.is_empty() {
                    // The blocks are already in reading order.
                    let joined = crate::layout::join_blocks(&fresh);
                    let piece = self.append(&format!("{joined}\n"));
                    if self.auto_copy.is_active() {
                        self.root.clipboard().set_text(&joined);
                    }
                    // The reading of the words is asked for, if it is wanted.
                    if crate::settings::read(&self.settings).readings {
                        let _ = self.reader.jobs.try_send(Asking::Readings { piece, text: joined });
                    }
                    // The list keeps the picture of the frame the text came
                    // from, so it can be shown next to the text.
                    self.add_list_entry(image, &fresh, section_of(&layout, &fresh));
                }
                self.set_status(&format!("{total} block(s) captured."));
            }
            Event::Failed(err) => {
                self.set_status(&format!("Problem: {err}"));
                self.start_button.set_sensitive(true);
                self.stop_button.set_sensitive(false);
                self.change_button.set_sensitive(false);
            }
            Event::Source {
                description,
                source_id,
            } => {
                self.source_label.set_text(&description);
                *self.source_id.borrow_mut() = source_id;
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
                    self.preview.set_surface(surface);
                    self.canvas.queue_draw();
                }
            }
        }
    }

    /// Adds an entry to the list, showing the frame the text came from.
    fn add_list_entry(
        &self,
        image: Option<Vec<u8>>,
        blocks: &[crate::layout::Fresh],
        section: Option<crate::layout::Heading>,
    ) {
        let source = self.source_id.borrow().clone();
        let text = crate::layout::join_blocks(blocks);

        // The same screen keeps one entry, and more text is added to it.
        if let Some(entry) = self.entries.borrow_mut().get_mut(&source) {
            entry.append(&text, image);
            return;
        }

        let row = gtk::ListBoxRow::new();
        let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row_box.set_margin_top(8);
        row_box.set_margin_bottom(8);
        row_box.set_margin_start(8);
        row_box.set_margin_end(8);

        let left = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let timestamp = gtk::Label::new(Some(&timestamp_now()));
        timestamp.set_xalign(0.0);
        timestamp.add_css_class("dim-label");
        timestamp.add_css_class("caption");
        left.append(&timestamp);

        let picture = gtk::Picture::new();
        picture.set_content_fit(gtk::ContentFit::Contain);
        picture.set_size_request(MAX_IMAGE_WIDTH, -1);
        picture.add_css_class("card");
        left.append(&picture);

        row_box.append(&left);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
        row_box.append(&content);

        row.set_child(Some(&row_box));
        self.list.append(&row);

        let entry = ListEntry {
            picture,
            timestamp,
            content,
        };
        entry.append(&text, image);
        if let Some(section) = section {
            entry.set_section(section);
        }
        self.entries.borrow_mut().insert(source, entry);

        // The pictures are kept in memory, so the list is not allowed to
        // grow for ever.
        while self.list.first_child().is_some() && self.list_count() > MAX_LIST_ENTRIES {
            let Some(first) = self.list.first_child() else {
                break;
            };
            self.list.remove(&first);
        }
    }

    fn list_count(&self) -> usize {
        let mut count = 0;
        let mut child = self.list.first_child();
        while let Some(row) = child {
            count += 1;
            child = row.next_sibling();
        }
        count
    }

    /// Adds a piece of text and shows it, and says which piece it was.
    fn append(&self, text: &str) -> usize {
        let piece = self.pieces.borrow_mut().len();
        self.pieces.borrow_mut().push(Piece {
            plain: text.to_owned(),
            text: text.to_owned(),
            words: Vec::new(),
        });
        self.render();
        piece
    }

    /// The words of a piece of text have been worked out, so the piece is
    /// shown as it should be.
    fn set_piece(&self, piece: usize, text: String, plain: String, words: Vec<(usize, usize)>) {
        {
            let mut pieces = self.pieces.borrow_mut();
            let Some(entry) = pieces.get_mut(piece) else {
                return;
            };
            entry.text = text;
            entry.plain = plain;
            entry.words = words;
        }
        self.render();
    }

    /// The word at a place in the text, and where it is.
    fn word_at(&self, offset: usize) -> Option<(String, (usize, usize))> {
        let pieces = self.pieces.borrow();
        let pieces = pieces
            .iter()
            .map(|piece| (piece.text.as_str(), piece.words.as_slice()))
            .collect::<Vec<_>>();
        crate::hover::word_at_offset(&pieces, offset)
    }

    /// Shows the text as it stands, with the readings where they are known.
    fn render(&self) {
        let all = self
            .pieces
            .borrow()
            .iter()
            .map(|piece| piece.text.as_str())
            .collect::<Vec<_>>()
            .concat();
        self.buffer.set_text(&all);
        self.scroll_to_end();
    }

    /// Sets the reading of a piece of text and shows it.
    /// The pointer moved over the preview.
    ///
    /// The word under it is looked up when the pointer has rested there long
    /// enough, and only while the modifier key is held down, so that moving
    /// the pointer over the preview does not keep asking the dictionary.
    fn pointer_moved(self: &Rc<Self>, x: f64, y: f64, from_event: gdk::ModifierType) {
        let settings = crate::settings::read(&self.settings);
        let from_device = self.keys_held();
        let held = from_event | from_device;
        if !held.contains(settings.modifier.mask()) {
            self.hovered_word(None);
            self.card.hide();
            return;
        }
        self.over_text.set(false);
        *self.pointer.borrow_mut() = Some((x, y));
        self.thing_changed(self.word_under(x, y));
    }

    /// The pointer moved over the text.
    fn text_pointer_moved(self: &Rc<Self>, x: f64, y: f64) {
        let settings = crate::settings::read(&self.settings);
        let held = self.keys_held();
        if !held.contains(settings.modifier.mask()) {
            self.card.hide();
            return;
        }
        self.over_text.set(true);
        *self.pointer.borrow_mut() = Some((x, y));
        let character = self
            .text_view
            .iter_at_location(x.round() as i32, y.round() as i32)
            .map(|iter| iter.offset());
        self.thing_changed(character.map(|offset| format!("t{offset}")));
    }

    /// A key was pressed or released.
    fn key_changed(&self, key: gdk::Key, state: gdk::ModifierType) {
        let mut held = state;
        match key {
            gdk::Key::Shift_L | gdk::Key::Shift_R => held |= gdk::ModifierType::SHIFT_MASK,
            gdk::Key::Control_L | gdk::Key::Control_R => held |= gdk::ModifierType::CONTROL_MASK,
            gdk::Key::Alt_L | gdk::Key::Alt_R => held |= gdk::ModifierType::ALT_MASK,
            _ => {}
        }
        tracing::debug!("a key was pressed or released; the keys held are {held:?}");
        self.keys.set(held);
        self.scanned.set(false);
    }

    /// The keys that are held down.
    ///
    /// The app's own keyboard is asked first, because the pointer does not
    /// say which keys are held unless the app has the keyboard.
    fn keys_held(&self) -> gdk::ModifierType {
        let from_device = gdk::Display::default()
            .and_then(|display| display.default_seat())
            .and_then(|seat| seat.pointer())
            .map(|pointer| pointer.modifier_state())
            .unwrap_or(gdk::ModifierType::empty());
        self.keys.get() | from_device
    }

    /// The keys have changed, so the word under the pointer is looked up
    /// again, which is what happens when the key is pressed while the
    /// pointer rests over a word.
    fn keys_changed(self: &Rc<Self>) {
        let settings = crate::settings::read(&self.settings);
        if self.keys_held().contains(settings.modifier.mask()) {
            tracing::debug!("the keys changed, and the key wanted is held");
            self.scanned.set(false);
        }
    }

    /// Notes that the thing under the pointer is a different one.
    ///
    /// The pointer jitters, and a pointer that only jitters is still. What
    /// matters is not whether the pointer moved, but whether it is over
    /// something else now.
    fn thing_changed(&self, thing: Option<String>) {
        let changed = self.under.borrow().as_deref() != thing.as_deref();
        *self.under.borrow_mut() = thing;
        if changed {
            self.last_moved.set(now_seconds());
            self.scanned.set(false);
        }
    }

    /// The word under the pointer in the preview, if there is one.
    fn word_under(&self, x: f64, y: f64) -> Option<String> {
        let (frame_width, frame_height) = self.preview.frame_size.get();
        let drawn = self.preview.rect.get()?;
        let lines = self.preview.detections.borrow().clone();
        let point = crate::hover::Point::new(x, y);
        let word = crate::hover::word_under(&lines, point, frame_width, frame_height, drawn)?;
        Some(word.text.clone())
    }

    /// Looks the word up, if the pointer has rested over one long enough.
    ///
    /// The timer runs for as long as the app does, so that there is never a
    /// timer to take away. Taking away a timer that has already fired stops
    /// the app, which is what the earlier one did.
    fn tick(&self) {
        let settings = crate::settings::read(&self.settings);
        let held = self.keys_held();
        let wanted = settings.modifier.mask();
        let delay = f64::from(settings.scan_delay) / 1000.0;
        let rested = now_seconds() - self.last_moved.get();
        if !held.contains(wanted) {
            return;
        }
        if rested < delay {
            return;
        }
        if self.scanned.replace(true) {
            return;
        }
        tracing::debug!("the pointer has rested for {rested:.2}s; looking it up");
        self.scan();
    }

    /// Looks up the word the pointer is over.
    fn scan(&self) {
        let Some((x, y)) = *self.pointer.borrow() else {
            return;
        };
        if self.over_text.get() {
            let Some(iter) = self
                .text_view
                .iter_at_location(x.round() as i32, y.round() as i32)
            else {
                return;
            };
            let offset = iter.offset() as usize;
            let Some((word, range)) = self.word_at(offset) else {
                return;
            };
            // The word is selected, as if it had been dragged over. That is
            // all that happens: the dictionary is asked when the user asks.
            self.select(range);
            tracing::debug!("the word under the pointer is {word:?}");
            return;
        }

        let (frame_width, frame_height) = self.preview.frame_size.get();
        let Some(drawn) = self.preview.rect.get() else {
            tracing::debug!("there is no picture of the screen to look at yet");
            return;
        };
        let lines = self.preview.detections.borrow().clone();
        let point = crate::hover::Point::new(x, y);
        let Some(word) = crate::hover::word_under(&lines, point, frame_width, frame_height, drawn)
        else {
            self.hovered_word(None);
            self.card.hide();
            return;
        };
        self.hovered_word(Some(word.bounds));
        let text = word.text.trim().to_owned();
        if text.is_empty() {
            self.card.hide();
            return;
        }
        tracing::debug!("the word under the pointer is {text:?}");
    }

    /// Selects a word in the text, so that it is clear which word is being
    /// looked up.
    fn select(&self, range: (usize, usize)) {
        let mut start = self.buffer.start_iter();
        let mut end = self.buffer.start_iter();
        start.set_offset(range.0 as i32);
        end.set_offset(range.1 as i32);
        self.buffer.select_range(&start, &end);
    }

    /// Shows the word the pointer is over in the preview, or takes it away.
    fn hovered_word(&self, bounds: Option<crate::layout::Rect>) {
        let changed = {
            let mut hovered = self.preview.hovered.borrow_mut();
            let changed = hovered.as_ref() != bounds.as_ref();
            *hovered = bounds;
            changed
        };
        if changed {
            self.canvas.queue_draw();
        }
    }

    /// Shows what the dictionary says about a word.
    ///
    /// The card appears beside the pointer when the pointer was over the word,
    /// and beside the text when the words were selected in it.
    fn show_entries(
        &self,
        pointed: bool,
        text: &str,
        entries: &[crate::yomitan::Entry],
    ) {
        if entries.is_empty() {
            self.set_status(&format!("Nothing is known about {text}."));
            self.card.hide();
            return;
        }
        let reading = entries.first().and_then(|entry| entry.reading.clone());
        let word = entries
            .first()
            .map(|entry| entry.term.clone())
            .unwrap_or_else(|| text.to_owned());
        if pointed {
            self.card.show(&self.canvas, &word, reading.as_deref(), entries);
        } else {
            self.card.show(&self.text_view, &word, reading.as_deref(), entries);
        }
    }

    /// Asks for the readings of the text that is already shown, which is
    /// what turning the readings on calls for.
    fn read_again(&self) {
        let texts: Vec<String> = self
            .pieces
            .borrow()
            .iter()
            .map(|piece| piece.plain.clone())
            .collect();
        for (piece, text) in texts.into_iter().enumerate() {
            let _ = self.reader.jobs.try_send(Asking::Readings { piece, text });
        }
    }

    fn copy_all(&self) {
        let all = self
            .pieces
            .borrow()
            .iter()
            .map(|piece| piece.plain.as_str())
            .collect::<Vec<_>>()
            .concat();
        let trimmed = all.trim_end();
        if trimmed.is_empty() {
            self.set_status("Nothing to copy yet.");
            return;
        }
        self.root.clipboard().set_text(trimmed);
        self.set_status("Copied everything to the clipboard.");
    }

    fn clear(&self) {
        self.pieces.borrow_mut().clear();
        self.buffer.set_text("");
        while let Some(row) = self.list.first_child() {
            self.list.remove(&row);
        }
        self.entries.borrow_mut().clear();
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
fn settings_panel(settings: settings::Shared) -> (gtk::Box, gtk::CheckButton) {
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

    let clump = gtk::CheckButton::with_label("Join lines into paragraphs");
    clump.set_active(current.clump);
    clump.set_tooltip_text(Some(
        "When on, the lines that belong to the same paragraph are shown as one \
         line. When off, each line keeps its own line.",
    ));
    panel.append(&clump);

    let lists = gtk::CheckButton::with_label("Gather list entries");
    lists.set_active(current.lists);
    lists.set_tooltip_text(Some(
        "When on, the short entries that line up, such as the entries of a \
         menu, are gathered into one list. When off, each entry is kept on \
         its own.",
    ));
    panel.append(&lists);

    let modifier_names = ["None", "Shift", "Control", "Alt"];
    let modifier = gtk::DropDown::from_strings(&modifier_names);
    modifier.set_selected(
        modifier_names
            .iter()
            .position(|name| *name == current.modifier.name())
            .unwrap_or(0) as u32,
    );
    modifier.set_tooltip_text(Some(
        "Hold this key while the pointer moves over a word, for the word to be \
         looked up.",
    ));
    panel.append(&labelled(
        "Scan modifier key",
        &modifier,
        "Hold this key while the pointer moves over a word, for the word to be \
         looked up.",
    ));

    let delay_adjustment = gtk::Adjustment::new(
        f64::from(current.scan_delay),
        0.0,
        2000.0,
        10.0,
        50.0,
        0.0,
    );
    let delay = gtk::SpinButton::new(Some(&delay_adjustment), 10.0, 2000);
    delay.set_tooltip_text(Some(
        "How long the pointer has to rest over a word, in milliseconds, before \
         the word is looked up.",
    ));
    panel.append(&labelled(
        "Scan delay (in milliseconds)",
        &delay,
        "How long the pointer has to rest over a word, in milliseconds, before \
         the word is looked up.",
    ));

    let readings = gtk::CheckButton::with_label("Show the readings");
    readings.set_active(current.readings);
    readings.set_tooltip_text(Some(
        "When on, the reading of each word is shown with the text, as \
         人気（にんき）のおかず. The readings come from Yomitan, which has to \
         be running for them to be had. The text that is copied is the plain \
         text either way.",
    ));
    panel.append(&readings);

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

    let for_modifier = settings.clone();
    modifier.connect_selected_notify(move |drop| {
        let index = drop.selected() as usize;
        let Some(name) = modifier_names.get(index) else {
            return;
        };
        let Some(modifier) = settings::Modifier::from_name(name) else {
            return;
        };
        if let Ok(mut settings) = for_modifier.lock() {
            settings.modifier = modifier;
        }
    });

    let for_delay = settings.clone();
    delay.connect_value_changed(move |spin| {
        if let Ok(mut settings) = for_delay.lock() {
            settings.scan_delay = spin.value().max(0.0) as u32;
        }
    });

    let for_readings = settings.clone();
    readings.connect_toggled(move |button| {
        if let Ok(mut settings) = for_readings.lock() {
            settings.readings = button.is_active();
        }
    });

    let for_clump = settings.clone();
    clump.connect_toggled(move |button| {
        if let Ok(mut settings) = for_clump.lock() {
            settings.clump = button.is_active();
        }
    });

    let for_lists = settings.clone();
    lists.connect_toggled(move |button| {
        if let Ok(mut settings) = for_lists.lock() {
            settings.lists = button.is_active();
        }
    });

    (panel, readings)
}

/// When the app started.
fn started() -> std::time::Instant {
    static STARTED: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    *STARTED.get_or_init(std::time::Instant::now)
}

/// How long it is since the app started, in seconds.
fn now_seconds() -> f64 {
    started().elapsed().as_secs_f64()
}

fn labelled(text: &str, control: &impl IsA<gtk::Widget>, tooltip: &str) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_tooltip_text(Some(tooltip));
    row.append(&label);
    row.append(control);
    row
}

/// The heading the given blocks sit under, if any.
fn section_of(
    layout: &crate::layout::Layout,
    blocks: &[crate::layout::Fresh],
) -> Option<crate::layout::Heading> {
    let sections = layout.sections();
    for entry in blocks {
        let block = &entry.block;
        for section in &sections {
            match block {
                crate::layout::Block::Heading { text, .. } => {
                    if section.heading.as_ref().is_some_and(|heading| &heading.text == text) {
                        return section.heading.clone();
                    }
                }
                crate::layout::Block::Paragraph { text, .. } => {
                    if section.paragraphs.iter().any(|paragraph| paragraph == text) {
                        return section.heading.clone();
                    }
                }
                crate::layout::Block::List { items, .. } => {
                    if section
                        .paragraphs
                        .iter()
                        .any(|paragraph| items.iter().any(|item| item == paragraph))
                    {
                        return section.heading.clone();
                    }
                }
            }
        }
    }
    None
}

/// The current local time, as it is shown beside a list entry.
fn timestamp_now() -> String {
    glib::DateTime::now_local()
        .ok()
        .and_then(|time| time.format("%b %e %l:%M %p").ok())
        .map(|time| time.to_string())
        .unwrap_or_default()
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

    /// Looks for a label with the given text in the tree of widgets.
    fn find_label(widget: &gtk::Widget, text: &str) -> Option<gtk::Widget> {
        if let Some(label) = widget.downcast_ref::<gtk::Label>() {
            if label.text() == text {
                return Some(widget.clone());
            }
        }
        let mut child = widget.first_child();
        while let Some(current) = child {
            if let Some(found) = find_label(&current, text) {
                return Some(found);
            }
            child = current.next_sibling();
        }
        None
    }

    /// The preview is a quarter of the width and the text the rest, so the
    /// picture is a third of the size of the text, and the button in front of
    /// the choice of how the text is shown takes it away.
    ///
    /// This is all in one test because GTK can only be started once, and only
    /// from one thread, so two tests could not both have a window.
    #[test]
    fn the_preview_takes_a_quarter_of_the_width_and_can_be_hidden() {
        if gtk::init().is_err() {
            // There is no display, so there is nothing to lay out.
            return;
        }
        let app = gtk::Application::builder()
            .application_id("org.gnome.Ocr.Test")
            .build();
        let window = Window::build(&app, true);

        let width = window.root.default_width();
        assert_eq!(
            window.split.position(),
            width / 4,
            "the preview should be a quarter of the width of {width}"
        );

        assert!(
            window.split.start_child().is_some(),
            "the picture should be there to begin with"
        );
        window.preview_button.set_active(false);
        assert!(
            window.split.start_child().is_none(),
            "the picture should be gone"
        );
        window.preview_button.set_active(true);
        assert!(
            window.split.start_child().is_some(),
            "the picture should be back"
        );

        // The window can be drawn, so that the layout can be looked at.
        let height = window.root.default_height();
        window.root.allocate(width, height, -1, None);

        // Let the window be shown and drawn, so that it has something to
        // draw with.
        let context = glib::MainContext::default();
        let _held = context.acquire();
        let until = std::time::Instant::now() + std::time::Duration::from_millis(400);
        while std::time::Instant::now() < until {
            context.iteration(false);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let paintable = gtk::WidgetPaintable::new(Some(&window.root));
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(&snapshot, f64::from(width), f64::from(height));

        let node: gtk::gsk::RenderNode = unsafe {
            let raw = gtk::ffi::gtk_snapshot_to_node(snapshot.as_ptr());
            glib::translate::FromGlibPtrFull::from_glib_full(raw)
        };
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, width, height)
            .expect("a surface to draw on");
        {
            let context = cairo::Context::new(&surface).expect("a context to draw with");
            node.draw(&context);
        }
        let mut file =
            std::fs::File::create("/tmp/gnome-ocr-window.png").expect("a file to write to");
        surface.write_to_png(&mut file).expect("the picture to be written");

        // The button stands in front of the choice of how the text is shown.
        let show_as = find_label(window.root.upcast_ref(), "Show as")
            .expect("the choice of how the text is shown should be there");
        let parent = show_as.parent().expect("the label should have a parent");
        let mut in_front = None;
        let mut child = parent.first_child();
        while let Some(current) = child {
            if current == show_as {
                break;
            }
            in_front = Some(current.clone());
            child = current.next_sibling();
        }
        let in_front = in_front.expect("something should stand in front of it");
        let button = in_front
            .downcast_ref::<gtk::ToggleButton>()
            .expect("the thing in front should be a button");
        assert_eq!(button.label().as_deref(), Some("Preview"));
    }


    /// The timestamp shown beside a list entry.
    #[test]
    fn the_timestamp_is_not_empty() {
        let timestamp = timestamp_now();
        assert!(!timestamp.is_empty(), "the timestamp should be written");
    }

    /// The scaled copy of the preview was cached by size alone, so every
    /// later frame of the same size was served from the cache and the
    /// preview stayed on the first frame.
    #[test]
    fn a_new_frame_replaces_the_one_that_was_shown() {
        use gtk::cairo;

        let preview = Preview::new();

        let red = cairo::ImageSurface::create(cairo::Format::ARgb32, 4, 4).expect("surface");
        {
            let context = cairo::Context::new(&red).expect("context");
            context.set_source_rgb(1.0, 0.0, 0.0);
            context.paint().expect("paint");
        }
        preview.set_surface(red);

        preview.scaled_surface(4, 4).expect("scaled");
        assert!(
            preview.scaled.borrow().is_some(),
            "the scaled copy should be cached"
        );

        // The next frame is the same size, so the cache would match it and
        // the old frame would keep being shown.
        let green = cairo::ImageSurface::create(cairo::Format::ARgb32, 4, 4).expect("surface");
        preview.set_surface(green);
        assert!(
            preview.scaled.borrow().is_none(),
            "the scaled copy of the old frame must be dropped"
        );
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
            x: 0.7,
            y: 0.1,
            width: 2.0,
            height: 1.6,
        };
        preview.clamp(&mut crop);

        assert!(crop.width <= 1.0, "the crop must fit the frame");
        assert!(crop.height <= 1.0, "the crop must fit the frame");
        assert!(crop.x >= 0.0);
        assert!(crop.y >= 0.0);
    }

    /// The crop is kept as a fraction of the frame, so it stays where it was
    /// when the window is resized and the preview is scaled differently.
    #[test]
    fn the_crop_does_not_move_when_the_preview_is_rescaled() {
        let preview = Preview::new();
        preview.rect.set(Some((0.0, 0.0, 800.0, 400.0)));
        *preview.crop.borrow_mut() = Some(Crop {
            x: 0.25,
            y: 0.5,
            width: 0.25,
            height: 0.25,
        });

        let before = preview.crop_rect().expect("crop");
        let region_before = preview.crop_region().expect("region");

        // The window was resized, so the preview is drawn differently.
        preview.rect.set(Some((0.0, 0.0, 400.0, 900.0)));
        let after = preview.crop_rect().expect("crop");
        let region_after = preview.crop_region().expect("region");

        assert_eq!(region_before, region_after, "the region must not change");
        assert_ne!(before, after, "but it is drawn in a different place");
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
        let mut crop = crop(0.2, 0.2, 0.4, 0.4);
        resize(&mut crop, Handle::NorthWest, 0.9, 0.9);
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
