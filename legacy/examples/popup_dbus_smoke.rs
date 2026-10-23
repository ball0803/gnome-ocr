//! Smoke test for the GNOME Shell extension's ShowPopup method.
//!
//! Usage:
//!   cargo run --release --example popup_dbus_smoke
//!
//! Prerequisites: extension is installed, user has logged out and back in,
//! extension is enabled (same as overlay_dbus_smoke).
//!
//! Expected: a dark popup with coloured text appears at (400, 300) on
//! the screen, on top of all application windows. ASCII text renders;
//! CJK glyphs do not render in this v1 cairo-only path (extension uses
//! Cairo Toy API; Pango integration is deferred).

use std::time::Duration;
use zbus::{proxy, Connection};

#[proxy(
    interface = "dev.local.gnome_ocr.Overlay",
    default_service = "dev.local.gnome_ocr.Overlay",
    default_path = "/dev/local/gnome_ocr/Overlay"
)]
trait Overlay {
    async fn show(&self, json: String) -> zbus::Result<()>;
    async fn show_popup(&self, json: String) -> zbus::Result<()>;
    async fn hide_popup(&self) -> zbus::Result<()>;
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    eprintln!("[popup-smoke] connecting to session D-Bus...");
    let conn = Connection::session().await?;
    let proxy = OverlayProxy::new(&conn).await?;
    eprintln!("[popup-smoke] proxy ready");

    let payload = r#"{
        "x": 400, "y": 300,
        "lines": ["hello world"],
        "readings": ["hEHloh wUrld"],
        "definitions": [
            "1. a greeting",
            "2. used as a test by gnome-ocr"
        ],
        "mine": true
    }"#;
    eprintln!("[popup-smoke] calling ShowPopup");
    proxy.show_popup(payload.to_string()).await?;
    eprintln!("[popup-smoke] ShowPopup OK — popup should be visible for 5s");
    tokio::time::sleep(Duration::from_secs(5)).await;
    eprintln!("[popup-smoke] calling HidePopup");
    proxy.hide_popup().await?;
    eprintln!("[popup-smoke] done");

    Ok(())
}
