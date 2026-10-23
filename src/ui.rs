//! GTK4 user interface.
//!
//! One window: a toolbar with Start/Stop, an auto-copy switch, Copy all and
//! Clear; a status line; and a scrollable monospace text view that fills with
//! recognised text as the pipeline reports it.

use std::cell::RefCell;
use std::rc::Rc;

use async_channel::Receiver;
use gtk::gdk::prelude::*;
use gtk::prelude::*;
use gtk::{gio, glib};

use crate::capture::SourceKind;
use crate::pipeline::{Event, Pipeline};

const APP_ID: &str = "dev.local.gnome_ocr";

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
    copy_button: gtk::Button,
    clear_button: gtk::Button,
    auto_copy: gtk::Switch,
    source_kind: gtk::DropDown,
    status: gtk::Label,
    events: async_channel::Sender<Event>,
    pipeline: RefCell<Option<Pipeline>>,
    japanese_only: bool,
    lines: RefCell<Vec<String>>,
}

impl Window {
    fn build(app: &gtk::Application, japanese_only: bool) -> Rc<Self> {
        let root = gtk::ApplicationWindow::builder()
            .application(app)
            .title("GNOME OCR")
            .default_width(760)
            .default_height(620)
            .build();

        let start_button = pill("Start OCR", Some("suggested-action"));
        let stop_button = pill("Stop", Some("destructive-action"));
        stop_button.set_sensitive(false);
        let copy_button = pill("Copy all", None);
        let clear_button = pill("Clear", None);

        let auto_copy = gtk::Switch::builder()
            .valign(gtk::Align::Center)
            .active(true)
            .build();

        let source_kind = gtk::DropDown::from_strings(&["Screen", "Window"]);
        source_kind.set_selected(0);
        source_kind.set_valign(gtk::Align::Center);
        source_kind.set_tooltip_text(Some(
            "What to capture. You pick the exact screen or window in the dialog that opens.",
        ));

        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        toolbar.set_margin_top(12);
        toolbar.set_margin_bottom(8);
        toolbar.set_margin_start(12);
        toolbar.set_margin_end(12);
        toolbar.append(&start_button);
        toolbar.append(&stop_button);
        toolbar.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        toolbar.append(&gtk::Label::new(Some("Capture")));
        toolbar.append(&source_kind);
        toolbar.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        toolbar.append(&gtk::Label::new(Some("Auto-copy new text")));
        toolbar.append(&auto_copy);
        toolbar.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        toolbar.append(&copy_button);
        toolbar.append(&clear_button);

        let status = gtk::Label::new(Some("Idle — press Start OCR to begin."));
        status.set_xalign(0.0);
        status.add_css_class("dim-label");
        status.set_margin_start(14);
        status.set_margin_end(14);
        status.set_margin_bottom(8);

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
            copy_button,
            clear_button,
            auto_copy,
            source_kind,
            status,
            events,
            pipeline: RefCell::new(None),
            japanese_only,
            lines: RefCell::new(Vec::new()),
        });

        window.wire();
        window.consume(inbox);
        window.root.present();
        window
    }

    fn wire(self: &Rc<Self>) {
        let owner = Rc::clone(self);
        self.start_button
            .connect_clicked(move |_| owner.start_pipeline());

        let owner = Rc::clone(self);
        self.stop_button.connect_clicked(move |_| {
            if let Some(pipeline) = owner.pipeline.borrow().as_ref() {
                pipeline.stop();
            }
            owner.stop_button.set_sensitive(false);
            owner.start_button.set_sensitive(true);
            owner.set_status("Stopping…");
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

    fn start_pipeline(self: &Rc<Self>) {
        if self.pipeline.borrow().is_some() {
            return;
        }

        self.clear();
        self.start_button.set_sensitive(false);
        self.stop_button.set_sensitive(true);
        self.source_kind.set_sensitive(false);

        let source = if self.source_kind.selected() == 1 {
            SourceKind::Window
        } else {
            SourceKind::Screen
        };
        self.set_status(match source {
            SourceKind::Window => "Pick a window in the dialog that opens…",
            SourceKind::Screen => "Pick a screen in the dialog that opens…",
        });

        let pipeline = Pipeline::start(self.events.clone(), self.japanese_only, source);
        *self.pipeline.borrow_mut() = Some(pipeline);
    }

    fn handle(&self, event: Event) {
        match event {
            Event::Started => {
                self.start_button.set_sensitive(false);
                self.stop_button.set_sensitive(true);
                self.set_status("Capturing — new text appears below.");
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
                self.source_kind.set_sensitive(true);
            }
            Event::Stopped => {
                *self.pipeline.borrow_mut() = None;
                self.start_button.set_sensitive(true);
                self.stop_button.set_sensitive(false);
                self.source_kind.set_sensitive(true);
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