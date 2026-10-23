//! Screenshot capture via `org.freedesktop.portal.Screenshot`.
//!
//! Flow:
//! 1. Portal invokes `org.gnome.Shell.Screenshot.InteractiveScreenshot` — gnome-shell
//!    shows the same region selector as the Print Screen button.
//! 2. User drags-selects; portal returns a `file://` URI to a PNG.
//! 3. We feed the PNG to qocrd via the `scanfile` stdin command (added in main.py).
//!
//! This replaces the broken transparent-overlay path. Loss: no live overlay.
//! Gain: works on Fedora 42 + GNOME 48 without fighting Mutter or PipeWire.

use anyhow::{Context, Result};
use ashpd::desktop::screenshot::{Screenshot as ScreenshotResult, ScreenshotOptions, ScreenshotProxy};
use std::path::PathBuf;
use tokio::process::Command;

use crate::daemon::qocrd_path;

/// URI returned by the portal.
pub struct CaptureResult {
    pub png_path: PathBuf,
}

/// Trigger the portal's interactive screenshot. The user drags a region in the
/// gnome-shell selection UI. Returns the local file path of the captured PNG.
pub async fn capture_interactive() -> Result<CaptureResult> {
    let proxy = ScreenshotProxy::new()
        .await
        .context("connect to org.freedesktop.portal.Screenshot")?;
    let options = ScreenshotOptions::default()
        .set_modal(true)
        .set_interactive(true);
    let request = proxy
        .screenshot(None, options)
        .await
        .context("portal Screenshot")?;
    let result: ScreenshotResult = request.response().context("screenshot response")?;
    let uri = result.uri().as_str().to_string();
    let path = file_uri_to_path(&uri)
        .ok_or_else(|| anyhow::anyhow!("portal URI not a local file path: {uri}"))?;
    Ok(CaptureResult { png_path: path })
}

fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let decoded = percent_decode(rest);
    Some(PathBuf::from(decoded))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Spawn qocrd, feed it a `scanfile` command, return the parsed JSON.
pub async fn ocr_file(png_path: &PathBuf) -> Result<String> {
    let mut child = Command::new(qocrd_path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("spawn qocrd")?;
    let mut stdin = child.stdin.take().context("qocrd stdin")?;
    use tokio::io::AsyncWriteExt;
    let cmd = format!("scanfile true {}\n", png_path.display());
    stdin.write_all(cmd.as_bytes()).await?;
    stdin.shutdown().await?;
    drop(stdin);
    let mut out = String::new();
    tokio::io::AsyncReadExt::read_to_string(
        &mut child.stdout.take().context("qocrd stdout")?,
        &mut out,
    )
    .await?;
    let _ = child.wait().await;
    Ok(out)
}
