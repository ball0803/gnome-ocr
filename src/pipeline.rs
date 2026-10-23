//! Capture -> OCR -> text pipeline, running off the GTK main loop.
//!
//! Everything expensive happens on a dedicated OS thread: the PipeWire capture
//! thread, the blocking qocrd child process, and PNG encoding. The thread owns
//! the capture session, so when the pipeline stops the capture socket is
//! released too. Events are pushed to the UI over a channel the GTK main
//! context drains.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_channel::Sender;

use crate::capture::{self, Frame, SourceKind};
use crate::engine::Engine;

/// One update for the UI.
pub enum Event {
    Started,
    Fresh { lines: Vec<String>, total: usize },
    Failed(String),
    Stopped,
}

/// A running capture+OCR pipeline.
pub struct Pipeline {
    shutdown: Arc<AtomicBool>,
}

impl Pipeline {
    /// Start capturing and OCRing in the background.
    pub fn start(events: Sender<Event>, japanese_only: bool, source: SourceKind) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);

        std::thread::Builder::new()
            .name("gnome-ocr-pipeline".into())
            .spawn(move || run(events, thread_shutdown, japanese_only, source))
            .expect("spawn pipeline thread");

        Self { shutdown }
    }

    /// Stop after the in-flight scan completes.
    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

fn run(
    events: Sender<Event>,
    shutdown: Arc<AtomicBool>,
    japanese_only: bool,
    source: SourceKind,
) {
    if let Err(err) = execute(&events, &shutdown, japanese_only, source) {
        emit(&events, Event::Failed(format!("{err}")));
    }
    emit(&events, Event::Stopped);
}

fn execute(
    events: &Sender<Event>,
    shutdown: &Arc<AtomicBool>,
    japanese_only: bool,
    source: SourceKind,
) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime for the portal handshake")?;

    let (frame_tx, frame_rx) = mpsc::sync_channel::<Frame>(2);
    let session = runtime
        .block_on(capture::start(frame_tx, source))
        .context("start screen capture")?;

    let mut engine = Engine::spawn(japanese_only)?;
    emit(events, Event::Started);
    tracing::info!("pipeline running");

    let mut seen: HashSet<String> = HashSet::new();
    let mut total: usize = 0;
    let mut frames_seen: usize = 0;
    let mut consecutive_failures: usize = 0;
    let mut stall_reported = false;
    let started_at = std::time::Instant::now();

    while !shutdown.load(Ordering::Relaxed) {
        let frame = match frame_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(frame) => frame,
            Err(RecvTimeoutError::Timeout) => {
                if !stall_reported
                    && frames_seen == 0
                    && started_at.elapsed() > Duration::from_secs(8)
                {
                    stall_reported = true;
                    emit(
                        events,
                        Event::Failed(
                            "Capture connected but no frames arrived within 8s. \
                             See the log for the PipeWire stream state."
                                .into(),
                        ),
                    );
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };

        frames_seen += 1;

        match engine.scan(&frame.rgba, frame.width, frame.height) {
            Ok(scan) => {
                consecutive_failures = 0;
                let fresh: Vec<String> = scan
                    .lines
                    .into_iter()
                    .filter(|line| seen.insert(line.clone()))
                    .collect();
                if !fresh.is_empty() {
                    total += fresh.len();
                    emit(events, Event::Fresh { lines: fresh, total });
                }
            }
            Err(err) => {
                consecutive_failures += 1;
                tracing::warn!("scan failed: {err}");
                if consecutive_failures == 3 {
                    let hint = if japanese_only {
                        " — qocrd found no Japanese text on screen. \
                         Try --all-languages."
                    } else {
                        ""
                    };
                    emit(events, Event::Failed(format!("OCR returned nothing{hint}")));
                }
            }
        }
    }

    drop(session);
    tracing::info!("pipeline stopped after {total} lines from {frames_seen} frames");
    Ok(())
}

fn emit(events: &Sender<Event>, event: Event) {
    if let Err(err) = events.try_send(event) {
        tracing::warn!("UI channel closed, dropping event: {err}");
    }
}