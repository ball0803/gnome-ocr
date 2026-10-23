//! examples/screencast_smoke.rs
//! Pop the xdg-desktop-portal ScreenCast permission dialog, capture ONE frame,
//! and write /tmp/screencast-test.png. Exits 0 on success.

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

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let proxy = Screencast::new().await?;
    let session = proxy.create_session(Default::default()).await?;
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Hidden)
                .set_sources(SourceType::Monitor | SourceType::Window)
                .set_multiple(false)
                .set_persist_mode(PersistMode::DoNot),
        )
        .await?;
    let response = proxy.start(&session, None, Default::default()).await?
        .response()?;
    let stream: ScreencastStream = response.streams().first().context("no streams")?.to_owned();
    eprintln!("[smoke] portal version = {}", proxy.version());
    eprintln!(
        "[smoke] pipe_wire_node_id = {}, size = {:?}, position = {:?}",
        stream.pipe_wire_node_id(),
        stream.size(),
        stream.position()
    );
    let node_id: u32 = stream.pipe_wire_node_id();
    let fd: OwnedFd = proxy.open_pipe_wire_remote(&session, Default::default()).await?;

    let out = PathBuf::from("/tmp/screencast-test.png");
    let join = std::thread::Builder::new()
        .name("pw-capture".into())
        .spawn(move || -> Result<(u32, u32, Vec<u8>)> {
            pw::init();
            let mainloop = MainLoopRc::new(None).context("MainLoopRc::new")?;
            let context = pw::context::ContextBox::new(mainloop.loop_(), None)
                .context("ContextBox::new")?;
            let core = context.connect_fd(fd, None).context("Context::connect_fd")?;

            let frame: Arc<Mutex<Option<(u32, u32, Vec<u8>)>>> = Arc::new(Mutex::new(None));

            let stream = pw::stream::StreamBox::new(
                &core,
                "gnome-ocr-smoke",
                properties! {
                    *pw::keys::MEDIA_TYPE     => "Video",
                    *pw::keys::MEDIA_CATEGORY => "Capture",
                    *pw::keys::MEDIA_ROLE     => "Screen",
                },
            )?;

            let info = Arc::new(Mutex::new(VideoInfoRaw::new()));
            let info_p = info.clone();
            let info_p2 = info.clone();
            let frame_p = frame.clone();
            let mainloop_for_cb = mainloop.clone();
            let _listener = stream
                .add_local_listener_with_user_data(())
                .state_changed(move |_, _, old, new| {
                    eprintln!("[smoke] state: {:?} -> {:?}", old, new);
                })
                .param_changed(move |_, _, id, param| {
                    let Some(param) = param else { return; };
                    if id != pw::spa::param::ParamType::Format.as_raw() { return; }
                    let (mt, st) = match pw::spa::param::format_utils::parse_format(param) {
                        Ok(v) => v,
                        Err(_) => return,
                    };
                    if mt != spa::param::format::MediaType::Video
                        || st != spa::param::format::MediaSubtype::Raw
                    {
                        return;
                    }
                    let mut g = info_p.lock().unwrap();
                    if let Err(e) = g.parse(param) {
                        eprintln!("[smoke] VideoInfoRaw parse err: {e:?}");
                    } else {
                        eprintln!(
                            "[smoke] format: {:?} {}x{} @ {}/{}",
                            g.format(),
                            g.size().width,
                            g.size().height,
                            g.framerate().num,
                            g.framerate().denom
                        );
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
                    if width == 0 || height == 0 {
                        eprintln!("[smoke] process before format; skipping");
                        return;
                    }
                    let Some(src) = d.data() else { return };
                    let row_bytes = (width as usize * 4).min(stride);
                    let mut bytes = vec![0u8; width as usize * height as usize * 4];
                    for row in 0..height as usize {
                        let s = row * stride;
                        let dst = row * width as usize * 4;
                        if s + row_bytes > src.len() { break; }
                        bytes[dst..dst + row_bytes].copy_from_slice(&src[s..s + row_bytes]);
                    }
                    eprintln!("[smoke] frame: {}x{} stride={}", width, height, stride);
                    frame_p.lock().unwrap().replace((width, height, bytes));
                    mainloop_for_cb.quit();
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
                    pw::spa::param::video::VideoFormat::RGB,
                    pw::spa::param::video::VideoFormat::RGB,
                    pw::spa::param::video::VideoFormat::RGBA,
                    pw::spa::param::video::VideoFormat::RGBx,
                    pw::spa::param::video::VideoFormat::BGRx,
                    pw::spa::param::video::VideoFormat::YUY2,
                    pw::spa::param::video::VideoFormat::I420,
                ),
                pw::spa::pod::property!(
                    pw::spa::param::format::FormatProperties::VideoSize,
                    Choice, Range, Rectangle,
                    pw::spa::utils::Rectangle { width: 320, height: 240 },
                    pw::spa::utils::Rectangle { width: 1, height: 1 },
                    pw::spa::utils::Rectangle { width: 4096, height: 4096 }),
                pw::spa::pod::property!(
                    pw::spa::param::format::FormatProperties::VideoFramerate,
                    Choice, Range, Fraction,
                    pw::spa::utils::Fraction { num: 25, denom: 1 },
                    pw::spa::utils::Fraction { num: 0,  denom: 1 },
                    pw::spa::utils::Fraction { num: 1000, denom: 1 }),
            );
            let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
                std::io::Cursor::new(Vec::new()),
                &pw::spa::pod::Value::Object(obj),
            )?.0.into_inner();
            let mut params = [pw::spa::pod::Pod::from_bytes(&values).unwrap()];

            eprintln!("[smoke] connecting (target.object unset, AUTOCONNECT)...");
            stream.connect(
                spa::utils::Direction::Input,
                Some(node_id),
                pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                &mut params,
            )?;
            eprintln!("[smoke] connected, running mainloop (process callback quits on first frame)...");

            mainloop.run();

            let g = frame.lock().unwrap().take().context("no frame received")?;
            Ok(g)
        })?;

    let (w, h, mut px) = join.join().unwrap()?;
    for p in px.chunks_exact_mut(4) { p.swap(0, 2); }
    let img = ImageBuffer::<Rgba<u8>, _>::from_raw(w, h, px).context("from_raw")?;
    img.save(&out).context("save png")?;
    println!("{}", out.display());
    Ok(())
}
