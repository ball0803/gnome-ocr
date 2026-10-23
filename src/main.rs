//! GNOME OCR — screen capture to clipboard.
//!
//! A single-window GTK4 app that watches the screen through the PipeWire
//! ScreenCast portal, sends each frame to qocrd, and shows the recognised
//! text. Newly recognised lines are optionally copied to the clipboard.

mod capture;
mod engine;
mod pipeline;
mod ui;

use std::path::PathBuf;

use gtk::glib;

fn main() -> glib::ExitCode {
    init_logging();
    ui::run(japanese_only())
}

fn japanese_only() -> bool {
    !std::env::args().any(|arg| arg == "--all-languages")
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    if let Some(file) = log_file().and_then(|path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
    }) {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter.clone())
            .with_writer(file)
            .with_ansi(false)
            .try_init();
        tracing::info!("gnome-ocr starting; log at {}", log_file().unwrap_or_default().display());
    }

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

fn log_file() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .map(|base| base.join("gnome-ocr/gnome-ocr.log"))
}