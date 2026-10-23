//! examples/continuous_capture.rs
//! End-to-end pipeline smoke test:
//!   PipeWire ScreenCast (per-frame)
//!     -> write BGRx frame to a unique PNG in /tmp
//!     -> spawn a one-shot qocrd scanfile on that PNG
//!     -> parse the JSON result
//!     -> call dev.local.gnome_ocr.Overlay.Show with the bounding boxes
//!
//! Run:
//!   cargo run --release --example continuous_capture
//!
//! Prerequisites:
//!   - The GNOME Shell extension is installed and enabled (see
//!     overlay_dbus_smoke for the install+logout+enable sequence).
//!   - The portal's ScreenCast dialog will appear once and you click Allow.
//!
//! Expected: green boxes appear on the screen, updating as each frame is
//! OCR'd. If the extension is NOT enabled, you'll see a stream of
//! "overlay proxy unavailable" warnings but the pipeline still runs.

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use ashpd::desktop::{
    PersistMode,
    screencast::{
        CursorMode, Screencast, SelectSourcesOptions, SourceType, Stream as ScreencastStream,
    },
};
use ashpd::enumflags2::BitFlags;
use image::{ImageBuffer, Rgba};
use pipewire as pw;
use pw::{main_loop::MainLoopRc, properties::properties, spa};
use spa::param::video::VideoInfoRaw;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc as tmpsc;
use zbus::{proxy, Connection};

#[proxy(
    interface = "dev.local.gnome_ocr.Overlay",
    default_service = "dev.local.gnome_ocr.Overlay",
    default_path = "/dev/local/gnome_ocr/Overlay"
)]
trait Overlay {
    async fn show(&self, json: String) -> zbus::Result<()>;
}

#[derive(serde::Serialize)]
struct Box {
    x: f64, y: f64, w: f64, h: f64,
}
#[derive(serde::Serialize)]
struct ShowPayload {
    boxes: Vec<Box>,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // --- D-Bus connection (extension must be enabled for these to succeed) ---
    let conn = Connection::session().await.context("session D-Bus")?;
    eprintln!("[cc] D-Bus ready");

    // --- qocrd subprocess: long-lived, line-based protocol ---
    let qocrd_path = "/home/camel/Downloads/qocr/.venv/bin/qocrd";
    let mut qocrd = Command::new(qocrd_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn qocrd")?;
    let mut qocrd_stdin = qocrd.stdin.take().context("qocrd stdin")?;
    let qocrd_stdout = qocrd.stdout.take().context("qocrd stdout")?;
    let mut stdout_reader = BufReader::new(qocrd_stdout);
    eprintln!("[cc] qocrd spawned at {qocrd_path}");

    // --- Frame channel: pw thread -> tokio task ---
    let (frame_tx, mut frame_rx) = tmpsc::channel::<(u32, u32, Vec<u8>, u64)>(2);
    let frame_tx_clone = frame_tx.clone();

    // --- Portal ScreenCast setup ---
    let proxy_handle = Screencast::new().await?;
    let session = proxy_handle.create_session(Default::default()).await?;
    proxy_handle
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Hidden)
                .set_sources(BitFlags::from(SourceType::Monitor))
                .set_multiple(false)
                .set_persist_mode(PersistMode::DoNot),
        )
        .await?;
    let response = proxy_handle.start(&session, None, Default::default()).await?
        .response()?;
    let stream: ScreencastStream = response.streams().first().context("no streams")?
        .to_owned();
    eprintln!("[cc] portal: node_id={} size={:?}",
        stream.pipe_wire_node_id(), stream.size());
    let node_id: u32 = stream.pipe_wire_node_id();
    let fd: OwnedFd = proxy_handle.open_pipe_wire_remote(&session, Default::default()).await?;

    // --- PipeWire capture thread ---
    let _pw_thread = std::thread::Builder::new()
        .name("pw-capture".into())
        .spawn(move || -> Result<()> {
            pw::init();
            let mainloop = MainLoopRc::new(None).context("MainLoopRc::new")?;
            let context = pw::context::ContextBox::new(mainloop.loop_(), None)
                .context("ContextBox::new")?;
            let core = context.connect_fd(fd, None).context("Context::connect_fd")?;

            let stream = pw::stream::StreamBox::new(
                &core,
                "gnome-ocr-continuous",
                properties! {
                    *pw::keys::MEDIA_TYPE     => "Video",
                    *pw::keys::MEDIA_CATEGORY => "Capture",
                    *pw::keys::MEDIA_ROLE     => "Screen",
                },
            )?;

            let info: Arc<Mutex<VideoInfoRaw>> = Arc::new(Mutex::new(VideoInfoRaw::new()));
            let info_p = info.clone();
            let info_p2 = info.clone();

            let _listener = stream
                .add_local_listener_with_user_data(())
                .state_changed(move |_, _, old, new| {
                    if matches!(new, pw::stream::StreamState::Error(_)) {
                        eprintln!("[cc] pw state: {:?} -> {:?}", old, new);
                    }
                })
                .param_changed(move |_, _, id, param| {
                    let Some(param) = param else { return; };
                    if id != pw::spa::param::ParamType::Format.as_raw() { return; }
                    let (mt, st) = match pw::spa::param::format_utils::parse_format(param) {
                        Ok(v) => v,
                        Err(_) => return,
                    };
                    if mt != spa::param::format::MediaType::Video
                        || st != spa::param::format::MediaSubtype::Raw { return; }
                    if let Err(e) = info_p.lock().unwrap().parse(param) {
                        eprintln!("[cc] VideoInfoRaw parse err: {e:?}");
                    } else {
                        let g = info_p.lock().unwrap();
                        eprintln!("[cc] format negotiated: {}x{} ({:?})",
                            g.size().width, g.size().height, g.format());
                    }
                })
                .process(move |stream, _| {
                    let mut buf = match stream.dequeue_buffer() {
                        None => return,
                        Some(b) => b,
                    };
                    let Some(d) = buf.datas_mut().first_mut() else { return };
                    let stride = d.chunk().stride().unsigned_abs() as usize;
                    let (width, height) = {
                        let g = info_p2.lock().unwrap();
                        (g.size().width as u32, g.size().height as u32)
                    };
                    if width == 0 || height == 0 { return; }
                    let Some(src) = d.data() else { return };
                    let row_bytes = (width as usize * 4).min(stride);
                    let mut bytes = vec![0u8; width as usize * height as usize * 4];
                    for row in 0..height as usize {
                        let s = row * stride;
                        let dst = row * width as usize * 4;
                        if s + row_bytes > src.len() { break; }
                        bytes[dst..dst + row_bytes].copy_from_slice(&src[s..s + row_bytes]);
                    }
                    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let _ = frame_tx_clone.try_send((width, height, bytes, seq));
                })
                .register()?;

            let obj = pw::spa::pod::object!(
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
                    pw::spa::param::video::VideoFormat::RGBx,
                    pw::spa::param::video::VideoFormat::RGBA,
                    pw::spa::param::video::VideoFormat::RGB,
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
                    pw::spa::utils::Fraction { num: 5, denom: 1 },
                    pw::spa::utils::Fraction { num: 0,  denom: 1 },
                    pw::spa::utils::Fraction { num: 30, denom: 1 }),
            );
            let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
                std::io::Cursor::new(Vec::new()),
                &pw::spa::pod::Value::Object(obj),
            )?.0.into_inner();
            let mut params = [pw::spa::pod::Pod::from_bytes(&values).unwrap()];

            stream.connect(
                spa::utils::Direction::Input,
                Some(node_id),
                pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                &mut params,
            )?;
            eprintln!("[cc] pw connected, running mainloop forever");
            mainloop.run();
            Ok(())
        })?;

    // --- Process frames: save PNG -> qocrd scanfile -> D-Bus Show ---
    let mut frame_seq = 0u64;
    let mut png_seq = 0u64;
    eprintln!("[cc] entering frame-processing loop");
    while let Some((w, h, mut px, seq)) = frame_rx.recv().await {
        // Drop stale frames: if more than 4 are queued, only keep the latest.
        if seq < frame_seq { continue; }
        frame_seq = seq;
        png_seq += 1;
        let path = PathBuf::from(format!("/tmp/gnome-ocr-{png_seq:04}.png"));
        for p in px.chunks_exact_mut(4) { p.swap(0, 2); }
        let img = match ImageBuffer::<Rgba<u8>, _>::from_raw(w, h, px) {
            Some(i) => i,
            None => { eprintln!("[cc] from_raw failed (size mismatch?)"); continue; }
        };
        if let Err(e) = img.save(&path) {
            eprintln!("[cc] save {}: {e}", path.display());
            continue;
        }
        eprintln!("[cc] frame {seq} -> {}", path.display());

        // OCR the frame
        let cmd = format!("scanfile true {}\n", path.display());
        if let Err(e) = qocrd_stdin.write_all(cmd.as_bytes()).await {
            eprintln!("[cc] qocrd write: {e}");
            continue;
        }
        if let Err(e) = qocrd_stdin.flush().await {
            eprintln!("[cc] qocrd flush: {e}");
            continue;
        }
        // Read until null byte
        let mut buf = Vec::new();
        match stdout_reader.read_until(b'\0', &mut buf).await {
            Ok(0) => { eprintln!("[cc] qocrd EOF"); break; }
            Ok(_) => {}
            Err(e) => { eprintln!("[cc] qocrd read: {e}"); continue; }
        }
        let chunk = match std::str::from_utf8(&buf) {
            Ok(s) => s.trim(),
            Err(_) => continue,
        };
        if chunk.is_empty() { continue; }
        if chunk.contains("\"unchanged\"") {
            eprintln!("[cc] frame {seq}: unchanged");
            continue;
        }
        // Parse the result
        let v: serde_json::Value = match serde_json::from_str(chunk) {
            Ok(v) => v,
            Err(e) => { eprintln!("[cc] json parse: {e}"); continue; }
        };
        let Some(lines) = v.get("lines").and_then(|x| x.as_array()) else {
            eprintln!("[cc] no lines in result");
            continue;
        };
        let boxes: Vec<Box> = lines.iter().filter_map(|l| {
            let aabb = l.get("aabb")?;
            Some(Box {
                x: aabb.get("x")?.as_f64()? - 4.0,
                y: aabb.get("y")?.as_f64()? - 4.0,
                w: aabb.get("width")?.as_f64()? + 8.0,
                h: aabb.get("height")?.as_f64()? + 8.0,
            })
        }).collect();
        eprintln!("[cc] frame {seq}: {} boxes", boxes.len());
        // Send to extension
        let payload = serde_json::to_string(&ShowPayload { boxes })?;
        if let Ok(proxy) = OverlayProxy::new(&conn).await {
            if let Err(e) = proxy.show(payload).await {
                eprintln!("[cc] overlay show: {e}");
            }
        } else {
            eprintln!("[cc] overlay extension not available (not enabled?)");
        }
    }
    Ok(())
}
