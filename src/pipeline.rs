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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use async_channel::Sender;

use crate::capture::{self, Frame};
use crate::engine::Engine;

/// One update for the UI.
#[derive(Debug)]
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
    pub fn start(
        events: Sender<Event>,
        japanese_only: bool,
        restore_token: Arc<Mutex<Option<String>>>,
    ) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);

        std::thread::Builder::new()
            .name("gnome-ocr-pipeline".into())
            .spawn(move || run(events, thread_shutdown, japanese_only, restore_token))
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
    restore_token: Arc<Mutex<Option<String>>>,
) {
    guarded(&events, || {
        if let Err(err) = execute(&events, &shutdown, japanese_only, &restore_token) {
            emit_terminal(&events, Event::Failed(format!("{err}")));
        }
    });
}


/// Resolves once the capture has been asked to stop.
///
/// Used to abandon the portal handshake, which otherwise waits for the
/// permission dialog and would leave the UI waiting forever.
async fn wait_for_cancel(cancel: Arc<AtomicBool>) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(150));
    loop {
        ticker.tick().await;
        if cancel.load(Ordering::Relaxed) {
            return;
        }
    }
}

/// Runs the pipeline body and always reports that it has stopped.
fn guarded(events: &Sender<Event>, body: impl FnOnce()) {
    let _guard = StopGuard {
        events: events.clone(),
    };
    body();
}

/// Reports that the pipeline has ended, even if the thread unwound.
///
/// Without this a panic would leave the UI holding a dead pipeline and the
/// capture could never be started again.
struct StopGuard {
    events: Sender<Event>,
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        emit_terminal(&self.events, Event::Stopped);
    }
}

fn execute(
    events: &Sender<Event>,
    shutdown: &Arc<AtomicBool>,
    japanese_only: bool,
    restore_token: &Arc<Mutex<Option<String>>>,
) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime for the portal handshake")?;

    let (frame_tx, frame_rx) = mpsc::sync_channel::<Frame>(2);
    let token = restore_token.lock().ok().and_then(|token| token.clone());
    let (session, token) = runtime.block_on(async {
        tokio::select! {
            result = capture::start(frame_tx, token) => result.context("start screen capture"),
            _ = wait_for_cancel(Arc::clone(shutdown)) => {
                anyhow::bail!("capture cancelled before it started")
            }
        }
    })?;
    if let Some(token) = token
        && let Ok(mut stored) = restore_token.lock()
    {
        *stored = Some(token);
    }

    let mut engine = Engine::spawn(japanese_only, Arc::clone(shutdown))?;
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
                    emit(
                        events,
                        Event::Failed(
                            "qocrd failed on 3 consecutive frames. See the log.".into(),
                        ),
                    );
                }
            }
        }
    }

    drop(session);
    tracing::info!("pipeline stopped after {total} lines from {frames_seen} frames");
    Ok(())
}

/// Sends an event that may be dropped when the UI is behind.
fn emit(events: &Sender<Event>, event: Event) {
    if let Err(err) = events.try_send(event) {
        tracing::warn!("UI channel full, dropping event: {err}");
    }
}

/// Sends an event the UI must not miss.
///
/// Terminal events decide whether the capture can be started again, so
/// they wait for room in the channel rather than being dropped.
fn emit_terminal(events: &Sender<Event>, event: Event) {
    if let Err(err) = events.send_blocking(event) {
        tracing::warn!("UI channel closed, dropping event: {err}");
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// The capture can only be started again once the pipeline reports that
    /// it stopped, so that report has to survive a thread that unwinds.
    #[test]
    fn stopping_is_reported_even_when_the_thread_unwinds() {
        let (sender, receiver) = async_channel::bounded(4);
        let events = sender.clone();

        let outcome = std::thread::spawn(move || {
            guarded(&events, || panic!("simulated failure inside the pipeline thread"));
        })
        .join();

        assert!(outcome.is_err(), "the thread should have panicked");
        match recv_with_deadline(&receiver) {
            Event::Stopped => {}
            other => panic!("expected Stopped, got {other:?}"),
        }
    }

    /// Terminal events must not be dropped when the UI is behind, because
    /// they are what re-enables the start button.
    #[test]
    fn a_terminal_event_survives_a_full_channel() {
        let (sender, receiver) = async_channel::bounded(1);
        sender.try_send(Event::Started).expect("first event fits");

        let sender_for_thread = sender.clone();
        let sender = std::thread::spawn(move || {
            emit_terminal(&sender_for_thread, Event::Stopped);
        });

        // The channel is full, so the terminal event has to wait for room.
        let first = receiver.recv_blocking().expect("first event");
        assert!(matches!(first, Event::Started));

        sender.join().expect("emit thread");
        let second = recv_with_deadline(&receiver);
        assert!(matches!(second, Event::Stopped));
    }

    /// The portal handshake waits for a permission dialog, so the stop has
    /// to be able to abandon it.
    #[test]
    fn the_cancel_watcher_notices_a_stop() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let flag = Arc::new(AtomicBool::new(false));
        let watcher = Arc::clone(&flag);
        runtime.block_on(async move {
            let handle = tokio::spawn(wait_for_cancel(watcher));
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            flag.store(true, Ordering::Relaxed);
            tokio::time::timeout(std::time::Duration::from_secs(2), handle)
                .await
                .expect("the watcher must notice the stop")
                .expect("the watcher must not panic");
        });
    }

    /// Waits for an event, failing rather than hanging when none arrives.
    fn recv_with_deadline(receiver: &async_channel::Receiver<Event>) -> Event {
        let (sender, done) = std::sync::mpsc::channel();
        let receiver = receiver.clone();
        std::thread::spawn(move || {
            let _ = sender.send(receiver.recv_blocking());
        });
        done.recv_timeout(std::time::Duration::from_secs(5))
            .expect("a terminal event must arrive")
            .expect("the channel must stay open")
    }
}
