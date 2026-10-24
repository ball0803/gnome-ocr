//! PipeWire ScreenCast capture.
//!
//! Opens an xdg-desktop-portal ScreenCast session (the path proven working on
//! this machine by the original smoke test), negotiates a BGRx video format,
//! and pushes each dequeued buffer into a channel as RGBA8 bytes.
//!
//! Capture runs on its own OS thread driving the PipeWire mainloop, because
//! the PipeWire C API is not async. Frames reach the async side over a bounded
//! channel; if the consumer is slow, frames are dropped rather than stalling
//! the mainloop and desynchronising the stream.

use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::PersistMode;
use ashpd::enumflags2::BitFlags;
use pipewire as pw;
use pw::{main_loop::MainLoopRc, properties::properties, spa};
use spa::param::video::VideoInfoRaw;
use std::sync::mpsc::SyncSender;

/// A single captured frame as RGBA8, ready to be encoded as PNG.
#[derive(Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Handle for a running capture session.
///
/// Dropping the handle stops capture; the PipeWire thread unwinds on its next
/// frame and releases the socket.
pub struct Session {
    cancel: Arc<AtomicBool>,
}

impl Session {
    /// Ask the capture thread to unwind and close the stream.
    pub fn stop(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What the portal should offer as a capture target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// A whole monitor.
    Screen,
    /// A single application window, chosen in the portal dialog.
    Window,
}

impl SourceKind {
    fn source_type(self) -> SourceType {
        match self {
            SourceKind::Screen => SourceType::Monitor,
            SourceKind::Window => SourceType::Window,
        }
    }
}

/// Request a ScreenCast session and begin pumping frames into `tx`.
///
/// The portal shows a permission dialog once per session; the user must accept
/// it, and pick which screen or window to share. Returns after the portal
/// handshake, before any frame has arrived.
pub async fn start(tx: SyncSender<Frame>, source: SourceKind) -> Result<Session> {
    let proxy = Screencast::new().await?;
    let session = proxy.create_session(Default::default()).await?;
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Hidden)
                .set_sources(BitFlags::from(source.source_type()))
                .set_multiple(false)
                .set_persist_mode(PersistMode::DoNot),
        )
        .await?;
    let response = proxy
        .start(&session, None, Default::default())
        .await?
        .response()?;
    let stream = response
        .streams()
        .first()
        .context("portal returned no streams")?
        .to_owned();
    tracing::info!(
        "portal granted: node_id={} size={:?}",
        stream.pipe_wire_node_id(),
        stream.size()
    );
    let node_id: u32 = stream.pipe_wire_node_id();
    let fd: OwnedFd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await?;

    let cancel = Arc::new(AtomicBool::new(false));
    let tx = Arc::new(Mutex::new(Some(tx)));
    let thread_cancel = Arc::clone(&cancel);

    std::thread::Builder::new()
        .name("gnome-ocr-capture".into())
        .spawn(move || {
            if let Err(err) = run_pipewire(fd, node_id, &tx, &thread_cancel) {
                tracing::error!("capture thread stopped: {err}");
            }
        })
        .context("spawn capture thread")?;

    Ok(Session { cancel })
}

fn run_pipewire(
    fd: OwnedFd,
    node_id: u32,
    tx: &Arc<Mutex<Option<SyncSender<Frame>>>>,
    cancel: &Arc<AtomicBool>,
) -> Result<()> {
    pw::init();
    let mainloop = MainLoopRc::new(None).context("MainLoopRc::new")?;
    let context = pw::context::ContextBox::new(mainloop.loop_(), None)
        .context("ContextBox::new")?;
    let core = context.connect_fd(fd, None).context("connect_fd")?;

    let stream = pw::stream::StreamBox::new(
        &core,
        "gnome-ocr",
        properties! {
            *pw::keys::MEDIA_TYPE     => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE     => "Screen",
        },
    )?;

    let info: Arc<Mutex<VideoInfoRaw>> = Arc::new(Mutex::new(VideoInfoRaw::new()));
    let info_format = Arc::clone(&info);
    let info_size = Arc::clone(&info);
    let tx_frame = Arc::clone(tx);
    let cancel_cb = Arc::clone(cancel);
    let mainloop_cb = mainloop.clone();
    let announced_frame = Arc::new(AtomicBool::new(false));

    let _listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(move |_, _, old, new| {
            if matches!(new, pw::stream::StreamState::Error(_)) {
                tracing::error!("pipewire stream entered error state: {old:?} -> {new:?}");
            } else {
                tracing::info!("pipewire state: {old:?} -> {new:?}");
            }
        })
        .param_changed(move |_, _, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media, sub)) = pw::spa::param::format_utils::parse_format(param) else {
                return;
            };
            if media != spa::param::format::MediaType::Video
                || sub != spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            match info_format.lock().expect("format lock").parse(param) {
                Ok(_) => {
                    let g = info_format.lock().expect("format lock");
                    tracing::info!(
                        "negotiated {}x{} as {:?}",
                        g.size().width,
                        g.size().height,
                        g.format()
                    );
                }
                Err(err) => tracing::error!("VideoInfoRaw parse failed: {err:?}"),
            }
        })
        .process(move |stream, _| {
            // This callback fires once per frame, so it doubles as the
            // cancellation poll: unwind the mainloop when asked to stop.
            if cancel_cb.load(Ordering::Relaxed) {
                mainloop_cb.quit();
                return;
            }
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(datas) = buffer.datas_mut().first_mut() else {
                return;
            };
            let stride = datas.chunk().stride().unsigned_abs() as usize;
            let (width, height, bgr_order) = {
                let g = info_size.lock().expect("info lock");
                let bgr = is_bgr_order(g.format());
                (g.size().width, g.size().height, bgr)
            };
            if width == 0 || height == 0 {
                return;
            }
            let Some(source) = datas.data() else { return };

            let row_bytes = (width as usize * 4).min(stride);
            let row_len = width as usize * 4;
            let mut rgba = vec![0u8; row_len * height as usize];
            for (row, chunk) in rgba.chunks_mut(row_len).enumerate() {
                let src_offset = row * stride;
                let available = source.len().saturating_sub(src_offset);
                let copy = row_bytes.min(available).min(chunk.len());
                if copy == 0 {
                    break;
                }
                chunk[..copy].copy_from_slice(&source[src_offset..src_offset + copy]);
            }
            // Only BGR-ordered formats need the red/blue swap. Swapping on an
            // already-RGB frame would hand OCR a colour-inverted image.
            if bgr_order {
                for [r, _g, b, _] in rgba.as_chunks_mut::<4>().0 {
                    std::mem::swap(r, b);
                }
            }

            let guard = tx_frame.lock().expect("tx lock");
            let Some(sender) = guard.as_ref() else { return };
            // A full queue means the OCR consumer is behind; dropping this
            // frame is the right call, the next one carries newer pixels.
            let _ = sender.try_send(Frame {
                width,
                height,
                rgba,
            });
            if !announced_frame.swap(true, Ordering::Relaxed) {
                tracing::info!("first frame delivered: {width}x{height}");
            }
        })
        .register()?;

    let format_param = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id, spa::param::format::MediaType::Video),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id, spa::param::format::MediaSubtype::Raw),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice, Enum, Id,
            pw::spa::param::video::VideoFormat::RGB,
            pw::spa::param::video::VideoFormat::RGB,
            pw::spa::param::video::VideoFormat::RGBA,
            pw::spa::param::video::VideoFormat::RGBx,
            pw::spa::param::video::VideoFormat::BGRx,
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoSize,
            Choice, Range, Rectangle,
            pw::spa::utils::Rectangle { width: 1920, height: 1080 },
            pw::spa::utils::Rectangle { width: 1, height: 1 },
            pw::spa::utils::Rectangle { width: 4096, height: 4096 }),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFramerate,
            Choice, Range, Fraction,
            pw::spa::utils::Fraction { num: 25, denom: 1 },
            pw::spa::utils::Fraction { num: 0,  denom: 1 },
            pw::spa::utils::Fraction { num: 1000, denom: 1 }),
    );
    let encoded = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(format_param),
    )?
    .0
    .into_inner();
    let mut params =
        [pw::spa::pod::Pod::from_bytes(&encoded).context("re-parse format params")?];

    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("connect stream")?;
    tracing::info!("pipewire connected; entering mainloop");

    // Without this the stream sits in Paused forever and never negotiates a
    // format, so no buffers are ever delivered.
    stream.set_active(true).context("activate stream")?;
    tracing::info!("stream activated");

    mainloop.run();
    tracing::info!("pipewire mainloop exited");
    Ok(())
}
/// True when the negotiated format stores bytes as blue, green, red.
fn is_bgr_order(format: pw::spa::param::video::VideoFormat) -> bool {
    use pw::spa::param::video::VideoFormat as F;
    matches!(
        format,
        F::BGR | F::BGRx | F::BGRA | F::ARGB | F::xBGR | F::ABGR
    )
}
