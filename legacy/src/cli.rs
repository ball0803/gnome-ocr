//! CLI client. Separate binary `gnome-ocr-cli` that sends IPC commands to the daemon.
//! Shares modules with the daemon binary (single crate).

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::json;
use tokio::sync::mpsc as tmpsc;

mod ipc;
mod types;

use crate::ipc::socket_path;
use crate::types::Command;

#[derive(Debug, Parser)]
#[command(name = "gnome-ocr-cli", about = "Send IPC commands to gnome-ocr daemon")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Begin a region-select drag, then OCR the picked region.
    Scan {
        #[arg(long, default_value_t = true)]
        japanese: bool,
        #[arg(long, default_value_t = false)]
        fullscreen: bool,
    },
    Rescan,
    ClearOverlay,
    ClearAll,
    ShowRegion,
    TriggerPopup { x: i32, y: i32, monitor: String },
    HoverOn,
    HoverOff,
    ToggleConfig { key: String },
    GetConfig { key: String },
}

impl Cmd {
    fn into_command(self) -> Command {
        match self {
            Cmd::Scan { japanese, fullscreen } => Command::Scan { japanese, fullscreen },
            Cmd::Rescan => Command::Rescan,
            Cmd::ClearOverlay => Command::ClearOverlay,
            Cmd::ClearAll => Command::ClearAll,
            Cmd::ShowRegion => Command::ShowRegion,
            Cmd::TriggerPopup { x, y, monitor } => Command::TriggerPopup { x, y, monitor },
            Cmd::HoverOn => Command::HoverOn,
            Cmd::HoverOff => Command::HoverOff,
            Cmd::ToggleConfig { key } => Command::ToggleConfig { key },
            Cmd::GetConfig { key } => Command::GetConfig { key },
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cmd = cli.cmd.into_command();
    let path = socket_path().context("socket path")?;
    let (_tx, _rx) = tmpsc::channel::<()>(1);
    let reply = ipc::send(path, &cmd).await?;
    println!("{}", json!({ "ok": reply.ok, "message": reply.message }));
    Ok(())
}