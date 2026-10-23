//! Smoke test for the GNOME Shell extension D-Bus interface.
//!
//! Usage:
//!   cargo run --release --example overlay_dbus_smoke
//!
//! What it does:
//!   1. Connects to the session D-Bus.
//!   2. Calls dev.local.gnome_ocr.Overlay.Show with a test box (centered
//!      on the primary monitor at 400x300).
//!   3. Waits 4 seconds for you to see the green box.
//!   4. Calls Clear.
//!
//! Prerequisites:
//!   - The extension is installed at
//!     ~/.local/share/gnome-shell/extensions/gnome-ocr-overlay@gnome-ocr.local/
//!   - You have logged out and back in (Wayland: GNOME Shell needs a session
//!     restart to pick up newly installed extensions).
//!   - The extension is enabled: `gnome-extensions enable
//!     gnome-ocr-overlay@gnome-ocr.local`
//!
//! Expected: a green rectangle appears on your screen, on top of all
//! application windows. The green rect disappears after 4 seconds.

use std::time::Duration;
use zbus::{proxy, Connection};

#[proxy(
    interface = "dev.local.gnome_ocr.Overlay",
    default_service = "dev.local.gnome_ocr.Overlay",
    default_path = "/dev/local/gnome_ocr/Overlay"
)]
trait Overlay {
    async fn show(&self, json: String) -> zbus::Result<()>;
    async fn clear(&self) -> zbus::Result<()>;
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    eprintln!("[smoke] connecting to session D-Bus...");
    let conn = Connection::session().await?;
    let proxy = OverlayProxy::new(&conn).await?;
    eprintln!("[smoke] D-Bus proxy created (extension is registered)");

    // A single green box in the middle of the screen, 400x300.
    // The extension logs screen dimensions; we just put it at (200, 200).
    let payload = r#"{"boxes":[{"x":200,"y":200,"w":400,"h":300}]}"#;
    eprintln!("[smoke] calling Show with payload: {payload}");
    proxy.show(payload.to_string()).await?;
    eprintln!("[smoke] Show OK — you should see a green rect on your screen");
    eprintln!("[smoke] waiting 4s before clearing...");
    tokio::time::sleep(Duration::from_secs(4)).await;

    eprintln!("[smoke] calling Clear");
    proxy.clear().await?;
    eprintln!("[smoke] Clear OK — green rect should be gone");

    Ok(())
}
