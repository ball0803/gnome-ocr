//! Unix socket IPC server + CLI client.
//!
//! Server listens on `$XDG_RUNTIME_DIR/gnome-ocr.sock` and accepts JSON-encoded `Command`s.
//! Client (`gnome-ocr-cli <command>`) connects, writes one command, reads back a JSON
//! reply, and exits. This is the same shape as the Quickshell IPC the original qocr used.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc as tmpsc;

use crate::types::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
}

impl Reply {
    pub fn ok() -> Self {
        Self {
            ok: true,
            message: None,
            value: None,
        }
    }
    pub fn ok_msg(msg: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: Some(msg.into()),
            value: None,
        }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: Some(msg.into()),
            value: None,
        }
    }
    pub fn with_value(mut self, v: serde_json::Value) -> Self {
        self.value = Some(v);
        self
    }
}

pub fn socket_path() -> Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::runtime_dir())
        .context("no XDG_RUNTIME_DIR")?;
    Ok(runtime.join("gnome-ocr.sock"))
}

/// Spawn the listener. Each accepted connection reads one JSON-encoded command, hands it
/// to the channel, and exits when the connection closes.
pub async fn serve(
    path: PathBuf,
    tx: tmpsc::Sender<Command>,
) -> Result<()> {
    // Remove stale socket file from a previous crash.
    let _ = std::fs::remove_file(&path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let listener = UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?;
    // Tighten perms: server is a per-user socket.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    tracing::info!("IPC listening on {}", path.display());
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(handle_conn(stream, tx.clone()));
            }
            Err(e) => {
                tracing::warn!("IPC accept: {e}");
            }
        }
    }
}

async fn handle_conn(stream: UnixStream, tx: tmpsc::Sender<Command>) {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    if reader.read_line(&mut line).await.is_err() || line.trim().is_empty() {
        return;
    }
    match serde_json::from_str::<Command>(line.trim()) {
        Ok(cmd) => {
            if tx.send(cmd).await.is_err() {
                tracing::warn!("command channel closed; daemon likely shut down");
            }
            // Ack — the long-running reply would need a richer protocol, but the original
       // qocr `ipc call ocr <cmd>` returns immediately for most commands. Clients that
            // care about the result should query state via get_config / wait for events.
            let reply = Reply::ok();
            let payload = serde_json::to_string(&reply).unwrap_or_else(|_| "{}".into());
            let _ = write.write_all(payload.as_bytes()).await;
            let _ = write.write_all(b"\n").await;
            let _ = write.shutdown().await;
        }
        Err(e) => {
            let reply = Reply::err(format!("invalid command: {e}"));
            let payload = serde_json::to_string(&reply).unwrap_or_else(|_| "{}".into());
            let _ = write.write_all(payload.as_bytes()).await;
            let _ = write.write_all(b"\n").await;
            let _ = write.shutdown().await;
        }
    }
}

/// Client: connect, write one JSON command, read reply, print it, exit.
pub async fn send(path: PathBuf, cmd: &Command) -> Result<Reply> {
    let stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("connect {}", path.display()))?;
    let (read, mut write) = stream.into_split();
    let payload = serde_json::to_string(cmd)?;
    write.write_all(payload.as_bytes()).await?;
    write.write_all(b"\n").await?;
    write.shutdown().await?;
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let reply: Reply = serde_json::from_str(line.trim()).context("parse reply")?;
    Ok(reply)
}