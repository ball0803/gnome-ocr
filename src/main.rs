//! GNOME OCR — screen capture to clipboard.
//!
//! A single-window GTK4 app that watches the screen through the PipeWire
//! ScreenCast portal, sends each frame to qocrd, and shows the recognised
//! text. Newly recognised lines are optionally copied to the clipboard.

mod capture;
mod dedupe;
mod engine;
mod paths;
mod pipeline;
mod preview;
mod region;
mod ui;

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gtk::glib;

fn main() -> glib::ExitCode {
    init_logging();
    tracing::info!(
        "gnome-ocr {} starting (japanese_only={})",
        env!("CARGO_PKG_VERSION"),
        japanese_only()
    );
    ui::run(japanese_only())
}

fn japanese_only() -> bool {
    !std::env::args().any(|arg| arg == "--all-languages")
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    match log_file().and_then(open_log_file) {
        Some(file) => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(Tee {
                    file: Arc::new(Mutex::new(file)),
                })
                .try_init();
        }
        None => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(true)
                .with_writer(std::io::stderr)
                .try_init();
        }
    }
}

fn open_log_file(path: PathBuf) -> Option<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

/// Mirrors every log line to stderr and to the on-disk log at once.
#[derive(Clone)]
struct Tee {
    file: Arc<Mutex<std::fs::File>>,
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        std::io::stderr().write_all(buf)?;
        self.file.lock().expect("log file lock").write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()?;
        self.file.lock().expect("log file lock").flush()
    }
}

impl<'a> tracing_subscriber::fmt::writer::MakeWriter<'a> for Tee {
    type Writer = Tee;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn log_file() -> Option<PathBuf> {
    Some(paths::state_dir().join("gnome-ocr.log"))
}