//! D-Bus client that talks to the GNOME Shell extension that owns the
//! overlay surface. The extension registers itself on the session bus as
//! `dev.local.gnome_ocr.Overlay` at `/dev/local/gnome_ocr/Overlay`.
//!
//! Protocol (JSON payload to `Show`):
//! ```json
//! {"boxes":[{"x":100,"y":200,"w":400,"h":50}, ...]}
//! ```
//!
//! The extension emits a `Clicked(x, y)` signal when the user clicks on a
//! highlight box. Coordinates are global screen coords.

use anyhow::{Context, Result};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc as tmpsc;
use tracing::{info, warn};
use zbus::{proxy, Connection};

pub const DBUS_IFACE: &str = "dev.local.gnome_ocr.Overlay";
pub const DBUS_PATH: &str = "/dev/local/gnome_ocr/Overlay";

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct HighlightBox {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ShowPayload {
    boxes: Vec<HighlightBox>,
}

#[proxy(
    interface = "dev.local.gnome_ocr.Overlay",
    default_service = "dev.local.gnome_ocr.Overlay",
    default_path = "/dev/local/gnome_ocr/Overlay"
)]
trait Overlay {
    async fn show(&self, json: String) -> zbus::Result<()>;
    async fn clear(&self) -> zbus::Result<()>;
    async fn show_popup(&self, json: String) -> zbus::Result<()>;
    async fn hide_popup(&self) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn clicked(&self, x: i32, y: i32) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn mine_anki(&self, term: String) -> zbus::Result<()>;
}

#[derive(Clone)]
pub struct OverlayClient {
    conn: Connection,
}

impl OverlayClient {
    /// Connect to the session bus and prepare a proxy. The actual D-Bus
    /// calls happen later — the extension must be enabled in GNOME Shell for
    /// them to succeed. `Ok(())` here only means we reached the bus.
    pub async fn new() -> Result<Self> {
        let conn = Connection::session().await.context("session D-Bus")?;
        Ok(Self { conn })
    }

    /// Send a list of boxes to the extension. Returns Err if the call fails
    /// (e.g. extension not enabled) — the caller should log and keep going.
    pub async fn show(&self, boxes: Vec<HighlightBox>) -> Result<()> {
        let proxy = OverlayProxy::new(&self.conn).await
            .context("create overlay proxy (is the gnome-shell extension enabled?)")?;
        let payload = serde_json::to_string(&ShowPayload { boxes })?;
        proxy.show(payload).await.context("Show D-Bus call")?;
        Ok(())
    }

    /// Clear all boxes.
    pub async fn clear(&self) -> Result<()> {
        let proxy = OverlayProxy::new(&self.conn).await
            .context("create overlay proxy")?;
        proxy.clear().await.context("Clear D-Bus call")?;
        Ok(())
    }

    /// Show a popup at (x, y) with the given text. The JSON shape matches
    /// what the extension's ShowPopup accepts. Set `mine=true` to render
    /// the "Mine to Anki" button at the bottom of the popup.
    pub async fn show_popup(&self, x: f64, y: f64,
        lines: Vec<String>, readings: Vec<String>, definitions: Vec<String>,
        mine: bool) -> Result<()> {
        let proxy = OverlayProxy::new(&self.conn).await
            .context("create overlay proxy")?;
        #[derive(Serialize)]
        struct Payload<'a> {
            x: f64, y: f64,
            lines: &'a [String], readings: &'a [String], definitions: &'a [String],
            mine: bool,
        }
        let payload = serde_json::to_string(&Payload {
            x, y, lines: &lines, readings: &readings, definitions: &definitions, mine,
        })?;
        proxy.show_popup(payload).await.context("ShowPopup D-Bus call")?;
        Ok(())
    }

    /// Hide any visible popup.
    pub async fn hide_popup(&self) -> Result<()> {
        let proxy = OverlayProxy::new(&self.conn).await
            .context("create overlay proxy")?;
        proxy.hide_popup().await.context("HidePopup D-Bus call")?;
        Ok(())
    }

    /// Spawn a task that forwards `Clicked(x, y)` signals to the given
    /// channel. Returns the JoinHandle so the caller can cancel it.
    pub fn spawn_click_listener(&self, tx: tmpsc::Sender<(i32, i32)>) -> tokio::task::JoinHandle<()> {
        let conn = self.conn.clone();
        tokio::spawn(async move {
            // Try to build the proxy; if the extension isn't enabled, retry
            // every 2s for 30s, then give up (it'll be re-tried on the next
            // show() call if the user enables the extension later).
            let mut proxy = None;
            for _ in 0..15 {
                match OverlayProxy::new(&conn).await {
                    Ok(p) => { proxy = Some(p); break; }
                    Err(e) => {
                        warn!("overlay proxy unavailable ({e}); retrying in 2s");
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                }
            }
            let Some(proxy) = proxy else {
                warn!("gave up waiting for overlay extension");
                return;
            };
            let mut stream = match proxy.receive_clicked().await {
                Ok(s) => s,
                Err(e) => { warn!("receive_clicked: {e}"); return; }
            };
            info!("overlay Clicked listener up");
            while let Some(sig) = stream.next().await {
                let args = match sig.args() {
                    Ok(a) => a,
                    Err(e) => { warn!("clicked args: {e}"); continue; }
                };
                if tx.send((args.x, args.y)).await.is_err() {
                    info!("clicked channel closed; stopping listener");
                    break;
                }
            }
        })
    }

    /// Spawn a task that forwards `MineAnki(term)` signals to the given
    /// channel.
    pub fn spawn_mine_listener(&self, tx: tmpsc::Sender<String>) -> tokio::task::JoinHandle<()> {
        let conn = self.conn.clone();
        tokio::spawn(async move {
            let mut proxy = None;
            for _ in 0..15 {
                match OverlayProxy::new(&conn).await {
                    Ok(p) => { proxy = Some(p); break; }
                    Err(e) => {
                        warn!("overlay proxy unavailable ({e}); retrying in 2s");
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                }
            }
            let Some(proxy) = proxy else {
                warn!("gave up waiting for overlay extension (mine listener)");
                return;
            };
            let mut stream = match proxy.receive_mine_anki().await {
                Ok(s) => s,
                Err(e) => { warn!("receive_mine_anki: {e}"); return; }
            };
            info!("overlay MineAnki listener up");
            while let Some(sig) = stream.next().await {
                let args = match sig.args() {
                    Ok(a) => a,
                    Err(e) => { warn!("mine_anki args: {e}"); continue; }
                };
                if tx.send(args.term).await.is_err() {
                    info!("mine channel closed; stopping listener");
                    break;
                }
            }
        })
    }
}

/// Convert one OCR result's lines into highlight boxes with a configurable
/// margin. Coordinates are absolute screen pixels.
pub fn boxes_from_ocr(
    result: &crate::types::OcrResult,
    margin: f64,
) -> Vec<HighlightBox> {
    result.lines.iter().map(|l| HighlightBox {
        x: l.aabb.x - margin,
        y: l.aabb.y - margin,
        w: l.aabb.width + margin * 2.0,
        h: l.aabb.height + margin * 2.0,
    }).collect()
}

/// Helper for callers that already hold a connection via Arc.
pub async fn try_show(conn: &Arc<Connection>, boxes: Vec<HighlightBox>) {
    if let Ok(proxy) = OverlayProxy::new(conn.as_ref()).await {
        let payload = serde_json::to_string(&ShowPayload { boxes }).unwrap_or_default();
        if let Err(e) = proxy.show(payload).await {
            warn!("overlay Show failed: {e}");
        }
    } else {
        warn!("overlay extension not available (not enabled?)");
    }
}
