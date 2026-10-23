//! End-to-end smoke test: portal Screenshot → qocrd scanfile → print OCR text.
//!
//! Run: cargo run --release --example snap_test
//!
//! 1. gnome-shell's region-selector appears (same UI as Print Screen).
//! 2. Drag-select a region. The captured PNG is saved.
//! 3. qocrd OCRs the PNG via its `scanfile` stdin command.
//! 4. The OCR JSON is printed to stdout.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result};
use ashpd::desktop::screenshot::{Screenshot as ScreenshotResult, ScreenshotOptions, ScreenshotProxy};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

const QOCRD: &str = "/home/camel/.local/bin/qocrd";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let proxy = ScreenshotProxy::new()
        .await
        .context("connect to org.freedesktop.portal.Screenshot")?;
    let options = ScreenshotOptions::default()
        .set_modal(true)
        .set_interactive(true);
    eprintln!("[snap] invoking portal Screenshot (interactive=true)...");
    let request = proxy
        .screenshot(None, options)
        .await
        .context("portal Screenshot")?;
    let result: ScreenshotResult = request
        .response()
        .context("screenshot response (did you accept the dialog?)")?;
    let uri = result.uri().as_str().to_string();
    let png_path = file_uri_to_path(&uri)
        .ok_or_else(|| anyhow::anyhow!("portal URI not a local file path: {uri}"))?;
    eprintln!("[snap] captured PNG: {}", png_path.display());

    let mut child = Command::new(QOCRD)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("spawn qocrd")?;
    let mut stdin = child.stdin.take().context("qocrd stdin")?;
    let cmd = format!("scanfile true {}\n", png_path.display());
    eprintln!("[snap] feeding qocrd: {cmd}");
    stdin.write_all(cmd.as_bytes()).await?;
    stdin.shutdown().await?;
    drop(stdin);
    let mut out = String::new();
    child
        .stdout
        .take()
        .context("qocrd stdout")?
        .read_to_string(&mut out)
        .await?;
    let _ = child.wait().await;
    println!("{out}");
    Ok(())
}

fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let mut out = Vec::with_capacity(rest.len());
    let bytes = rest.as_bytes();
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
    Some(PathBuf::from(String::from_utf8_lossy(&out).into_owned()))
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
