//! Capture, recognition and the start/stop commands that drive them.
//!
//! The capture is created once and kept for the lifetime of the app. Stopping
//! only pauses the recognition; the portal session, the PipeWire node and the
//! frame stream stay up. Starting again therefore needs no portal request and
//! cannot fail the way a fresh permission request can.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_channel::Sender;

use crate::capture::Capture;
use crate::dedupe::History;
use crate::engine::Engine;

/// One update for the UI.
#[derive(Debug)]
pub enum Event {
    Started,
    /// Text that was recognised, and where it was found.
    Scanned {
        /// The detections that had not been seen before.
        fresh: Vec<crate::dedupe::Detection>,
        /// Everything found in this frame.
        found: Vec<crate::dedupe::Detection>,
        /// How many new detections there have been in total.
        total: usize,
    },
    Failed(String),
    Stopped,
    /// A small PNG of the captured frame, for the preview pane.
    Preview {
        png: Vec<u8>,
        /// The size of the frame the PNG was taken from.
        frame_width: u32,
        frame_height: u32,
    },
    /// What the portal is sharing.
    Source(String),
}

/// What the UI asks the pipeline to do.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Start,
    Stop,
    /// Release the current capture and open a new one, asking the portal for
    /// the source again instead of restoring the previous choice.
    ChangeSource,
}

/// A running capture and recognition pipeline.
pub struct Pipeline {
    commands: mpsc::Sender<Command>,
    shutdown: Arc<AtomicBool>,
}

impl Pipeline {
    /// Start the pipeline thread.
    pub fn start(
        events: Sender<Event>,
        japanese_only: bool,
        restore_token: Arc<Mutex<Option<String>>>,
        region: Arc<Mutex<Option<crate::region::Region>>>,
        settings: Arc<Mutex<crate::dedupe::Settings>>,
    ) -> Self {
        let (commands, inbox) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);

        std::thread::Builder::new()
            .name("gnome-ocr-pipeline".into())
            .spawn(move || {
                run(
                    events,
                    inbox,
                    thread_shutdown,
                    japanese_only,
                    restore_token,
                    region,
                    settings,
                )
            })
            .expect("spawn pipeline thread");

        Self {
            commands,
            shutdown,
        }
    }

    /// Begin recognising text. The first call opens the capture.
    pub fn start_capture(&self) {
        let _ = self.commands.send(Command::Start);
    }

    /// Stop recognising text and release the capture.
    pub fn stop_capture(&self) {
        let _ = self.commands.send(Command::Stop);
    }

    /// Capture something else. The portal asks which screen or window to
    /// share instead of restoring the previous choice.
    pub fn change_source(&self) {
        let _ = self.commands.send(Command::ChangeSource);
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = self.commands.send(Command::Stop);
    }
}

fn run(
    events: Sender<Event>,
    inbox: mpsc::Receiver<Command>,
    shutdown: Arc<AtomicBool>,
    japanese_only: bool,
    restore_token: Arc<Mutex<Option<String>>>,
    region: Arc<Mutex<Option<crate::region::Region>>>,
    settings: Arc<Mutex<crate::dedupe::Settings>>,
) {
    guarded(&events, || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(err) => {
                emit_terminal(&events, Event::Failed(format!("{err}")));
                return;
            }
        };

        let mut capture: Option<Capture> = None;
        let mut engine: Option<Engine> = None;
        let mut history = History::new(settings);
        let mut total: usize = 0;
        let mut recognising = false;
        let mut last_preview = std::time::Instant::now() - Duration::from_millis(400);

        while !shutdown.load(Ordering::Relaxed) {
            let mut begin = false;
            match inbox.recv_timeout(Duration::from_millis(50)) {
                Ok(Command::Start) => begin = true,
                Ok(Command::ChangeSource) => {
                    // Release the current capture and forget the grant, so
                    // the portal asks which screen or window to share
                    // instead of restoring the previous choice.
                    release_capture(&runtime, &mut capture, &mut engine);
                    if let Ok(mut stored) = restore_token.lock() {
                        *stored = None;
                    }
                    recognising = false;
                    begin = true;
                }
                Ok(Command::Stop) => {
                    if recognising {
                        recognising = false;
                        emit_terminal(&events, Event::Stopped);
                    }
                    release_capture(&runtime, &mut capture, &mut engine);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }

            if begin {
                if capture.is_none() {
                    tracing::info!("opening the screen capture, this asks for permission");
                    let token = restore_token.lock().ok().and_then(|token| token.clone());
                    match runtime.block_on(crate::capture::start(token)) {
                        Ok((opened, info)) => {
                            capture = Some(opened);
                            // The portal consumes the token that was sent,
                            // so the stored one is replaced even when the
                            // reply carries none.
                            if let Ok(mut stored) = restore_token.lock() {
                                *stored = info.restore_token.clone();
                            }
                            emit(&events, Event::Source(info.description));
                        }
                        Err(err) => {
                            emit_terminal(&events, Event::Failed(format!("{err}")));
                            continue;
                        }
                    }
                }
                if engine.is_none() {
                    engine = match Engine::spawn(
                        japanese_only,
                        Arc::clone(&shutdown),
                        Arc::clone(&region),
                    ) {
                        Ok(engine) => Some(engine),
                        Err(err) => {
                            emit_terminal(&events, Event::Failed(format!("{err}")));
                            continue;
                        }
                    };
                }
                recognising = true;
                tracing::info!("capture running");
                emit(&events, Event::Started);
            }

            // The compositor can end the share on its own, from the system
            // menu. The stream then dies, and the capture has to be replaced
            // before the next start.
            if let Some(current) = capture.as_ref()
                && !current.is_alive()
            {
                tracing::info!("the capture was stopped outside the app");
                capture = None;
                engine = None;
                recognising = false;
                emit_terminal(
                    &events,
                    Event::Failed(
                        "The screen share was stopped from the system menu. \
                         Press Start to share again."
                            .into(),
                    ),
                );
                emit_terminal(&events, Event::Stopped);
                continue;
            }

            if !recognising {
                continue;
            }
            let (Some(capture), Some(engine)) = (capture.as_ref(), engine.as_mut()) else {
                continue;
            };
            let Some(frame) = capture.try_next_frame() else {
                continue;
            };

            if last_preview.elapsed() >= Duration::from_millis(400) {
                last_preview = std::time::Instant::now();
                if let Some(png) =
                    crate::preview::preview_png(&frame.rgba, frame.width, frame.height, 1024)
                {
                    emit(
                        &events,
                        Event::Preview {
                            png,
                            frame_width: frame.width,
                            frame_height: frame.height,
                        },
                    );
                }
            }

            match engine.scan(&frame.rgba, frame.width, frame.height) {
                Ok(scan) => {
                    let mut fresh = Vec::new();
                    let mut found = Vec::new();
                    for detection in scan.detections {
                        if history.is_new(&detection) {
                            fresh.push(detection.clone());
                        }
                        found.push(detection);
                    }
                    total += fresh.len();
                    emit(
                        &events,
                        Event::Scanned {
                            fresh,
                            found,
                            total,
                        },
                    );
                }
                Err(err) => {
                    tracing::warn!("scan failed: {err}");
                    if shutdown.load(Ordering::Relaxed) {
                        recognising = false;
                    }
                }
            }
        }

        tracing::info!("pipeline finished after {total} line(s)");
    });
}


/// Closes the portal session and releases the stream and the daemon.
fn release_capture(
    runtime: &tokio::runtime::Runtime,
    capture: &mut Option<Capture>,
    engine: &mut Option<Engine>,
) {
    if let Some(closed) = capture.take() {
        runtime.block_on(closed.close_session());
    }
    *engine = None;
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
