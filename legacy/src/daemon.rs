//! `qocrd` subprocess wrapper.
//!
//! Spawns the Python OCR daemon we already set up at `/home/camel/Downloads/qocr/.venv/bin/qocrd`
//! (via the wrapper at `~/.local/bin/qocrd`) and pumps commands to it on stdin, parsing
//! null-byte-delimited JSON results from stdout.
//!
//! The daemon is the only thing that needs to be reused from upstream qocr; the GTK4
//! overlay here replaces Quickshell entirely.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc as tmpsc;

use crate::types::OcrResult;

/// Resolved path to the qocrd wrapper we created during install.
pub fn qocrd_path() -> PathBuf {
    // Prefer the wrapper in ~/.local/bin (works around upstream pyproject.toml bug),
    // then the venv entry-point script (broken entry, but keep as fallback).
    if let Ok(home) = std::env::var("HOME") {
        let wrapper = PathBuf::from(home).join(".local/bin/qocrd");
        if wrapper.exists() {
            return wrapper;
        }
    }
    PathBuf::from("/home/camel/Downloads/qocr/.venv/bin/qocrd")
}

/// Handle to the running qocrd subprocess.
pub struct OcrDaemon {
    child: Child,
    stdin: tokio::process::ChildStdin,
    /// Pending "rescan" replies, one per scan region. qocrd returns `{"unchanged":true}` if
    /// the captured frame matches the previous one for that monitor.
    pending_rescans: u32,
}

impl OcrDaemon {
    pub async fn spawn() -> Result<Self> {
        let mut cmd = Command::new(qocrd_path());
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().context("spawn qocrd")?;
        let stdin = child.stdin.take().context("qocrd stdin")?;
        let stdout = child.stdout.take().context("qocrd stdout")?;

        // Drain stderr in a background task — qocrd's logs go here but Chrome Screen AI
        // spams at C level (suppressed internally). We just forward to tracing.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt as _;
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    tracing::debug!("qocrd stderr: {line}");
                }
            });
        }

        // Spawn the read loop. It will send parsed results via the channel passed in `run`.
        // We just need to keep stdout alive; the read loop lives in `run`.
        // (Stash stdout back so callers can use it.)
        child.stdout = Some(stdout);

        Ok(Self {
            child,
            stdin,
            pending_rescans: 0,
        })
    }

    /// Ask qocrd to scan. `japanese_only` filters to Japanese-only lines. If `fullscreen`
    /// is true, qocrd picks the active monitor instead of asking slurp.
    pub async fn scan(&mut self, japanese_only: bool, fullscreen: bool) -> Result<()> {
        self.write_line(format!("scan {japanese_only} {fullscreen}\n")).await
    }

    /// Re-OCR a previously captured region. Coordinates are in absolute screen space.
    /// `monitor` is the output name (e.g. "DP-1", "HDMI-A-1").
    pub async fn rescan(
        &mut self,
        japanese_only: bool,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        x_in_monitor: i32,
        y_in_monitor: i32,
        monitor: &str,
    ) -> Result<()> {
        let line = format!(
            "rescan {japanese_only} {x} {y} {w} {h} {x_in_monitor} {y_in_monitor} {monitor}\n"
        );
        self.write_line(line).await
    }

    async fn write_line(&mut self, line: String) -> Result<()> {
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Take the stdout handle out so the caller can drive the read loop.
    pub fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.child.stdout.take()
    }

    pub fn kill(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// Read loop: splits stdout on `\0` records, parses each as JSON, and forwards to the channel.
/// One record = either an `OcrResult` or an `UnchangedResult` marker.
pub async fn read_loop(
    stdout: tokio::process::ChildStdout,
    tx: tmpsc::Sender<DaemonEvent>,
) -> Result<()> {
    use tokio::io::AsyncBufReadExt as _;
    let mut reader = BufReader::new(stdout);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = reader.read_until(b'\0', &mut buf).await?;
        if n == 0 {
            tracing::warn!("qocrd stdout EOF");
            break;
        }
        let chunk = std::str::from_utf8(&buf).context("non-utf8 from qocrd")?.trim();
        if chunk.is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(chunk) {
            Ok(v) if v.get("unchanged").is_some() => {
                let _ = tx.send(DaemonEvent::Unchanged).await;
            }
            Ok(_) => match serde_json::from_str::<OcrResult>(chunk) {
                Ok(result) => {
                    let _ = tx.send(DaemonEvent::Result(result)).await;
                }
                Err(e) => {
                    tracing::warn!("parse OcrResult: {e} -- raw: {chunk}");
                }
            },
            Err(e) => {
                tracing::warn!("parse JSON: {e} -- raw: {chunk}");
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
pub enum DaemonEvent {
    Result(OcrResult),
    Unchanged,
}