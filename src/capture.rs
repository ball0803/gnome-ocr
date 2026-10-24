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
use pipewire as pw;
use pw::{main_loop::MainLoopRc, properties::properties, spa};
use spa::param::video::{VideoFormat, VideoInfoRaw};
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

/// Request a ScreenCast session and begin pumping frames into `tx`.
///
/// The portal shows a permission dialog once per session; the user must accept
/// it and pick which screen or window to share. Both kinds are requested, so
/// the dialog lists screens and windows alike. Returns after the portal
/// handshake, before any frame has arrived.
pub async fn start(tx: SyncSender<Frame>) -> Result<Session> {
    let proxy = Screencast::new().await?;
    let session = proxy.create_session(Default::default()).await?;
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                .set_sources(SourceType::Monitor | SourceType::Window)
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
        "portal v{} granted: node_id={} size={:?} position={:?}",
        proxy.version(),
        stream.pipe_wire_node_id(),
        stream.size(),
        stream.position()
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
            let Some(param) = param else {
                tracing::info!("pipewire param {id} changed to null");
                return;
            };
            tracing::info!("pipewire param {id} changed");
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            match pw::spa::param::format_utils::parse_format(param) {
                Ok((media, subtype)) => {
                    tracing::info!("  format pod: media={media:?} subtype={subtype:?}");
                }
                Err(err) => tracing::info!("  parse_format failed: {err:?}"),
            }
            match apply_negotiated_format(&info_format, param) {
                Ok((width, height, format)) => {
                    tracing::info!("  negotiated {width}x{height} as {format:?}");
                }
                Err(err) => tracing::error!("  VideoInfoRaw parse failed: {err:?}"),
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
            let (width, height, format) = {
                let g = info_size.lock().expect("info lock");
                (g.size().width, g.size().height, g.format())
            };
            if width == 0 || height == 0 {
                return;
            }
            let Some(pixel_size) = pixel_size(format) else {
                tracing::warn!("unsupported negotiated format {format:?}");
                return;
            };

            let chunk = datas.chunk();
            let offset = chunk.offset() as usize;
            let chunk_size = chunk.size() as usize;
            let raw_stride = chunk.stride();
            let Some(source) = datas.data() else { return };

            // PipeWire occasionally leaves the stride at zero when the
            // producer did not fill it in.
            let stride = if raw_stride > 0 {
                raw_stride as usize
            } else {
                width as usize * pixel_size
            };
            let row_bytes = width as usize * pixel_size;
            let end = offset.saturating_add(chunk_size).min(source.len());
            let Some(source) = source.get(offset..end) else {
                return;
            };
            if source.len() < stride.saturating_mul(height as usize) {
                return;
            }

            let mut rgba = vec![0u8; width as usize * height as usize * 4];
            for y in 0..height as usize {
                let row_start = y * stride;
                if row_start + row_bytes > source.len() {
                    break;
                }
                let row = &source[row_start..row_start + row_bytes];
                for x in 0..width as usize {
                    let input = &row[x * pixel_size..];
                    let output = &mut rgba[(y * width as usize + x) * 4..];
                    // The fourth byte of an "x" format is padding, not alpha.
                    // Reading it as alpha yields a transparent image when the
                    // compositor leaves it zero.
                    match format {
                        VideoFormat::RGB | VideoFormat::RGBx => {
                            output.copy_from_slice(&[input[0], input[1], input[2], 0xFF]);
                        }
                        VideoFormat::RGBA => {
                            output.copy_from_slice(&[input[0], input[1], input[2], input[3]]);
                        }
                        VideoFormat::BGR | VideoFormat::BGRx => {
                            output.copy_from_slice(&[input[2], input[1], input[0], 0xFF]);
                        }
                        VideoFormat::BGRA => {
                            output.copy_from_slice(&[input[2], input[1], input[0], input[3]]);
                        }
                        _ => return,
                    }
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
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRA,
            pw::spa::param::video::VideoFormat::RGBx,
            pw::spa::param::video::VideoFormat::RGBA,
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoSize,
            Choice, Range, Rectangle,
            pw::spa::utils::Rectangle { width: 1920, height: 1080 },
            pw::spa::utils::Rectangle { width: 1, height: 1 },
            pw::spa::utils::Rectangle { width: 7680, height: 4320 }),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFramerate,
            Choice, Range, Fraction,
            pw::spa::utils::Fraction { num: 30, denom: 1 },
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

    mainloop.run();
    tracing::info!("pipewire mainloop exited");
    Ok(())
}
/// Applies a negotiated format pod and reports the geometry it carries.
///
/// The lock is taken once and held across both the parse and the read.
/// Taking a second lock here deadlocks: the mutex is not reentrant and this
/// runs on the PipeWire mainloop thread, so the whole stream stops.
fn apply_negotiated_format(
    info: &Mutex<VideoInfoRaw>,
    param: &spa::pod::Pod,
) -> Result<(u32, u32, VideoFormat), spa::utils::result::Error> {
    let mut guard = info.lock().expect("format lock");
    guard.parse(param)?;
    Ok((guard.size().width, guard.size().height, guard.format()))
}

/// Bytes per pixel for a negotiated format, or None if it cannot be read.
fn pixel_size(format: VideoFormat) -> Option<usize> {
    match format {
        VideoFormat::RGB | VideoFormat::BGR => Some(3),
        VideoFormat::RGBA
        | VideoFormat::RGBx
        | VideoFormat::BGRA
        | VideoFormat::BGRx => Some(4),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pw::spa::param::format::FormatProperties;
    use pw::spa::param::format::MediaSubtype;
    use pw::spa::param::format::MediaType;
    use pw::spa::param::video::VideoFormat;

    /// A second lock on the same mutex would wedge the PipeWire mainloop
    /// thread forever. The work runs on a helper thread with a deadline so
    /// a regression fails the test instead of hanging the suite.
    #[test]
    fn applying_a_negotiated_format_completes_without_deadlocking() {
        let object = pw::spa::pod::object!(
            pw::spa::utils::SpaTypes::ObjectParamFormat,
            pw::spa::param::ParamType::Format,
            pw::spa::pod::property!(
                FormatProperties::MediaType,
                Id, MediaType::Video),
            pw::spa::pod::property!(
                FormatProperties::MediaSubtype,
                Id, MediaSubtype::Raw),
            pw::spa::pod::property!(
                FormatProperties::VideoFormat,
                Id, VideoFormat::BGRx),
            pw::spa::pod::property!(
                FormatProperties::VideoSize,
                Rectangle, pw::spa::utils::Rectangle { width: 1920, height: 1080 }),
        );
        let bytes = pw::spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &pw::spa::pod::Value::Object(object),
        )
        .expect("serialize")
        .0
        .into_inner();
        let bytes = std::sync::Arc::new(bytes);

        let (tx, rx) = std::sync::mpsc::channel();
        let worker = bytes.clone();
        std::thread::spawn(move || {
            let pod = pw::spa::pod::Pod::from_bytes(&worker).expect("pod");
            let info = std::sync::Mutex::new(VideoInfoRaw::new());
            let result = apply_negotiated_format(&info, pod);
            let _ = tx.send(result.map(|(w, h, f)| (w, h, f)));
        });

        let outcome = rx.recv_timeout(std::time::Duration::from_secs(5));
        let (width, height, format) = match outcome {
            Ok(Ok(value)) => value,
            Ok(Err(err)) => panic!("applying the format failed: {err:?}"),
            Err(err) => panic!("applying the format deadlocked: {err}"),
        };
        assert_eq!(width, 1920);
        assert_eq!(height, 1080);
        assert_eq!(format, VideoFormat::BGRx);
    }

    /// The gate that used to discard the negotiated format: parse_format has
    /// to accept it, and VideoInfoRaw::parse has to yield the geometry.
    #[test]
    fn negotiated_format_pod_is_accepted_by_both_parsers() {
        let object = pw::spa::pod::object!(
            pw::spa::utils::SpaTypes::ObjectParamFormat,
            pw::spa::param::ParamType::Format,
            pw::spa::pod::property!(
                FormatProperties::MediaType,
                Id, MediaType::Video),
            pw::spa::pod::property!(
                FormatProperties::MediaSubtype,
                Id, MediaSubtype::Raw),
            pw::spa::pod::property!(
                FormatProperties::VideoFormat,
                Id, VideoFormat::BGRx),
            pw::spa::pod::property!(
                FormatProperties::VideoSize,
                Rectangle, pw::spa::utils::Rectangle { width: 1920, height: 1080 }),
            pw::spa::pod::property!(
                FormatProperties::VideoFramerate,
                Fraction, pw::spa::utils::Fraction { num: 30, denom: 1 }),
        );
        let bytes = pw::spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &pw::spa::pod::Value::Object(object),
        )
        .expect("serialize")
        .0
        .into_inner();
        let pod = pw::spa::pod::Pod::from_bytes(&bytes).expect("pod");

        let parsed = pw::spa::param::format_utils::parse_format(pod);
        assert!(
            parsed.is_ok(),
            "parse_format rejected the negotiated format: {parsed:?}"
        );
        let (media, subtype) = parsed.expect("parsed");
        assert_eq!(media, MediaType::Video);
        assert_eq!(subtype, MediaSubtype::Raw);

        let mut info = VideoInfoRaw::new();
        info.parse(pod)
            .expect("VideoInfoRaw::parse must accept the negotiated format");
        assert_eq!(info.size().width, 1920);
        assert_eq!(info.size().height, 1080);
        assert_eq!(info.format(), VideoFormat::BGRx);
    }
}
