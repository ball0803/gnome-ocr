//! OCR engine: wraps the `qocrd` daemon over stdio and turns frames into text.
//!
//! Frame -> PNG -> scratch file -> `qocrd scanfile` -> JSON -> text lines.
//! The engine keeps the qocrd child alive between scans and replaces it
//! automatically if it dies, so callers never deal with daemon lifecycle.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::process::{Child, ChildStdin, Command, Stdio};

use anyhow::{Context, Result};
use image::{ImageBuffer, ImageFormat, Rgba};
use serde::Deserialize;

/// Where the qocrd wrapper lives when it is installed on the machine.
const QOCRD_CANDIDATES: &[&str] = &[
    "/home/camel/.local/bin/qocrd",
    "/home/camel/Downloads/qocr/.venv/bin/qocrd",
    "qocrd",
];

/// Where the qocrd wrapper may be, first candidate that resolves wins.
///
/// A bundle of the app carries its own copy of the engine next to the
/// program, so that is looked for first, and the place can be named outright
/// with GNOME_OCR_QOCRD.
fn qocrd_candidates() -> Vec<PathBuf> {
    qocrd_candidates_from(std::env::current_exe().ok().as_deref())
}

/// The same, for a program at a known place.
fn qocrd_candidates_from(program: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(program) = std::env::var_os("GNOME_OCR_QOCRD") {
        candidates.push(PathBuf::from(program));
    }
    if let Some(directory) = program.and_then(Path::parent) {
        candidates.push(directory.join("qocrd"));
    }
    candidates.extend(QOCRD_CANDIDATES.iter().map(PathBuf::from));
    candidates
}

#[derive(Debug, Deserialize)]
struct ScanFileResponse {
    #[serde(default)]
    text: String,
    #[serde(default)]
    lines: Vec<ScanLine>,
}

#[derive(Debug, Deserialize)]
struct ScanLine {
    #[serde(default)]
    text: String,
    #[serde(default)]
    aabb: Option<AxisAlignedBox>,
    #[serde(default)]
    words: Vec<ScanWord>,
    #[serde(default)]
    is_vertical: bool,
    #[serde(default)]
    content_type: String,
    #[serde(default)]
    block_id: i64,
    #[serde(default)]
    direction: String,
    #[serde(default)]
    confidence: f64,
}

#[derive(Debug, Deserialize)]
struct ScanWord {
    #[serde(default)]
    text: String,
    #[serde(default)]
    aabb: Option<AxisAlignedBox>,
    #[serde(default)]
    has_space_after: bool,
}

/// The axis-aligned bounding box qocrd reports for a line of text.
#[derive(Debug, Deserialize)]
struct AxisAlignedBox {
    #[serde(default)]
    x: f64,
    #[serde(default)]
    y: f64,
    #[serde(default)]
    width: f64,
    #[serde(default)]
    height: f64,
}

/// Recognised text from a single frame, with where it was found.
#[derive(Debug, Default, Clone)]
pub struct ScanResult {
    /// The lines that were recognised, with everything needed to work out
    /// their order.
    pub lines: Vec<crate::layout::Line>,
    /// The part of the frame that was scanned, so it can be kept alongside
    /// the text that came from it.
    pub scanned: Option<ScannedImage>,
}

/// The pixels that were read, in RGBA8.
#[derive(Debug, Clone)]
pub struct ScannedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: std::sync::Arc<Vec<u8>>,
}



/// How sure the engine has to be before a line is taken as text.
const MIN_CONFIDENCE: f64 = 0.5;

/// Whether the line is text, as opposed to a rule, a picture or a formula.
fn is_text_content(content_type: &str) -> bool {
    matches!(
        content_type,
        "PRINTED_TEXT" | "HANDWRITTEN_TEXT" | "" | "UNREADABLE_TEXT"
    )
}

/// Reads replies from qocrd and hands them over a channel.
///
/// qocrd ends every reply with a NUL byte, so each reply is read up to its
/// NUL and parsed on its own. Reading the output as a stream of JSON values
/// instead would run into that byte after the first reply.
fn read_replies(
    mut reader: impl std::io::BufRead,
    sender: std::sync::mpsc::Sender<Result<ScanFileResponse, serde_json::Error>>,
) {
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\0', &mut buffer) {
            Ok(0) => break,
            Ok(_) => {
                if buffer.last() == Some(&0) {
                    buffer.pop();
                }
                if buffer.is_empty() {
                    continue;
                }
                let reply = serde_json::from_slice::<ScanFileResponse>(&buffer);
                if sender.send(reply).is_err() {
                    break;
                }
            }
            Err(err) => {
                tracing::warn!("reading qocrd failed: {err}");
                break;
            }
        }
    }
}

/// How long to wait for qocrd to answer a scan.
const SCAN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// How often the wait checks whether the scan should be abandoned.
const SCAN_POLL: std::time::Duration = std::time::Duration::from_millis(150);

/// The qocrd child process. Owns its own teardown so it can be swapped out.
struct Process {
    child: Child,
    stdin: ChildStdin,
    responses: std::sync::mpsc::Receiver<Result<ScanFileResponse, serde_json::Error>>,
}

impl Process {
    fn spawn() -> Result<Self> {
        let program = qocrd_candidates()
            .into_iter()
            .find(|candidate| resolve(candidate))
            .context(
                "could not find qocrd; tried GNOME_OCR_QOCRD, the copy \
                 next to this program, ~/.local/bin/qocrd, the qocr \
                 virtualenv, and PATH",
            )?;

        tracing::info!("starting qocrd from {}", program.display());
        let mut child = Command::new(&program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("spawn qocrd at {}", program.display()))?;

        let stdin = child.stdin.take().context("qocrd stdin")?;
        let stdout = child.stdout.take().context("qocrd stdout")?;

        // A reader thread owns the output stream and hands parsed replies
        // over a channel, so a scan can wait with a deadline instead of
        // blocking forever on a daemon that stopped answering.
        //
        // qocrd ends every reply with a NUL byte. Reading the output as a
        // stream of JSON values would run into that byte after the first
        // reply and fail to parse, so each reply is read up to its NUL.
        let (sender, responses) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("qocrd-reader".into())
            .spawn(move || {
                read_replies(std::io::BufReader::new(stdout), sender);
            })
            .context("spawn qocrd reader thread")?;

        Ok(Self {
            child,
            stdin,
            responses,
        })
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// OCR engine backed by a long-lived qocrd process.
pub struct Engine {
    process: Process,
    cancel: Arc<AtomicBool>,
    region: Arc<Mutex<Option<crate::region::Region>>>,
    japanese_only: bool,
    scratch: PathBuf,
    dump_dir: Option<PathBuf>,
    dump_every: bool,
    frames: u64,
    dumped: u32,
}

impl Engine {
    /// Starts the engine.
    ///
    /// `cancel` is shared with the pipeline so that stopping abandons a scan
    /// that is already in flight instead of waiting for it to finish.
    pub fn spawn(
        japanese_only: bool,
        cancel: Arc<AtomicBool>,
        region: Arc<Mutex<Option<crate::region::Region>>>,
    ) -> Result<Self> {
        Ok(Self {
            process: Process::spawn()?,
            cancel,
            region,
            japanese_only,
            scratch: scratch_path(),
            dump_dir: dump_dir(),
            dump_every: std::env::var_os("GNOME_OCR_DUMP_ALL").is_some(),
            frames: 0,
            dumped: 0,
        })
    }

    /// Scan one RGBA frame and return the recognised lines.
    ///
    /// A protocol failure replaces the qocrd child so the next scan starts
    /// from a clean daemon, then reports the error to the caller.
    pub fn scan(&mut self, rgba: &[u8], width: u32, height: u32) -> Result<ScanResult> {
        self.frames += 1;
        let selected = self.region.lock().ok().and_then(|region| *region);
        let (pixels, width, height, origin_x, origin_y) = match selected {
            Some(region) => crop(rgba, width, height, region)
                .unwrap_or_else(|| (rgba.to_vec(), width, height, 0, 0)),
            None => (rgba.to_vec(), width, height, 0, 0),
        };
        let pixels = std::sync::Arc::new(pixels);
        let outcome = match write_png(&self.scratch, &pixels, width, height) {
            Ok(()) => {
                self.report_frame(width, height);
                self.exchange(
                    origin_x,
                    origin_y,
                    std::sync::Arc::clone(&pixels),
                    width,
                    height,
                )
            }
            Err(err) => Err(err),
        };

        match outcome {
            Ok(scan) => {
                tracing::info!(
                    "scan ok: {width}x{height}, {} line(s) recognised",
                    scan.lines.len()
                );
                let _ = std::fs::remove_file(&self.scratch);
                Ok(scan)
            }
            Err(err) => {
                tracing::warn!(
                    "scan failed at {width}x{height} ({} bytes, mean luma {:.1}): {err}",
                    rgba.len(),
                    mean_luma(rgba)
                );
                let _ = std::fs::remove_file(&self.scratch);
                // A scan that was abandoned because the capture stopped does
                // not need a fresh daemon; the engine is about to be dropped.
                if !self.cancel.load(Ordering::Relaxed) {
                    self.process = Process::spawn()?;
                }
                Err(err)
            }
        }
    }

    /// Asks qocrd to read the frame and returns what it found.
    ///
    /// The boxes come back relative to the image that was scanned, so the
    /// origin of the crop is added back to put them where they are in the
    /// frame.
    fn exchange(
        &mut self,
        origin_x: u32,
        origin_y: u32,
        scanned: std::sync::Arc<Vec<u8>>,
        scanned_width: u32,
        scanned_height: u32,
    ) -> Result<ScanResult> {
        let command = format!(
            "scanfile {} {}\n",
            if self.japanese_only { "true" } else { "false" },
            self.scratch.display()
        );
        self.process
            .stdin
            .write_all(command.as_bytes())
            .context("write command to qocrd")?;
        self.process.stdin.flush().context("flush qocrd stdin")?;

        // qocrd terminates each reply with a NUL byte, so exactly one JSON
        // value is read per command. Trying to read a second lands on that
        // NUL and fails to parse.
        // Wait in short steps so a stop is picked up promptly rather than
        // after the whole scan timeout.
        let deadline = std::time::Instant::now() + SCAN_TIMEOUT;
        let response = loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(anyhow::anyhow!(
                    "qocrd did not answer within {}s",
                    SCAN_TIMEOUT.as_secs()
                ));
            }
            match self.process.responses.recv_timeout(remaining.min(SCAN_POLL)) {
                Ok(response) => break response,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(anyhow::anyhow!("qocrd closed its output"));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if self.cancel.load(Ordering::Relaxed) {
                        return Err(anyhow::anyhow!("scan abandoned because the capture stopped"));
                    }
                }
            }
        }
        .context("parse qocrd response")?;

        let mut lines: Vec<crate::layout::Line> = Vec::new();

        // The lines carry their own geometry, which is what the preview
        // draws, what tells repeats apart, and what the order is worked out
        // from.
        for line in response.lines {
            let text = line.text.trim();
            if text.is_empty() {
                continue;
            }
            let Some(aabb) = line.aabb else { continue };
            let bounds = crate::layout::Rect {
                x: aabb.x + origin_x as f64,
                y: aabb.y + origin_y as f64,
                width: aabb.width,
                height: aabb.height,
            };

            let words = line
                .words
                .iter()
                .filter_map(|word| {
                    let word_box = word.aabb.as_ref()?;
                    Some(crate::layout::Word {
                        text: word.text.clone(),
                        bounds: crate::layout::Rect {
                            x: word_box.x + origin_x as f64,
                            y: word_box.y + origin_y as f64,
                            width: word_box.width,
                            height: word_box.height,
                        },
                        has_space_after: word.has_space_after,
                    })
                })
                .collect();

            lines.push(crate::layout::Line {
                text: text.to_owned(),
                bounds,
                words,
                is_vertical: line.is_vertical || line.direction == "TOP_TO_BOTTOM",
                is_right_to_left: line.direction == "RIGHT_TO_LEFT",
                is_text: is_text_content(&line.content_type) && line.confidence >= MIN_CONFIDENCE,
                block_id: line.block_id,
            });
        }

        // Some builds only fill in the flat text.
        if lines.is_empty() && !response.text.trim().is_empty() {
            for text in response.text.lines().map(str::trim).filter(|l| !l.is_empty()) {
                lines.push(crate::layout::Line {
                    text: text.to_owned(),
                    bounds: crate::layout::Rect {
                        x: 0.0,
                        y: 0.0,
                        width: 0.0,
                        height: 0.0,
                    },
                    words: Vec::new(),
                    is_vertical: false,
                    is_right_to_left: false,
                    is_text: true,
                    block_id: 0,
                });
            }
        }

        Ok(ScanResult {
            lines,
            scanned: Some(ScannedImage {
                width: scanned_width,
                height: scanned_height,
                rgba: scanned,
            }),
        })
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.scratch);
    }
}


/// Cuts the frame down to the selected part.
///
/// The region is in fractions of the frame, so it stays correct when the
/// captured window is resized. Returns None when the region covers nothing.
fn crop(
    rgba: &[u8],
    width: u32,
    height: u32,
    region: crate::region::Region,
) -> Option<(Vec<u8>, u32, u32, u32, u32)> {
    if region.is_empty() {
        return None;
    }
    let limit_x = width as f64;
    let limit_y = height as f64;
    let x0 = (region.x * limit_x).round().clamp(0.0, limit_x) as u32;
    let y0 = (region.y * limit_y).round().clamp(0.0, limit_y) as u32;
    let x1 = ((region.x + region.width) * limit_x)
        .round()
        .clamp(0.0, limit_x) as u32;
    let y1 = ((region.y + region.height) * limit_y)
        .round()
        .clamp(0.0, limit_y) as u32;
    let cropped_width = x1.saturating_sub(x0);
    let cropped_height = y1.saturating_sub(y0);
    if cropped_width == 0 || cropped_height == 0 {
        return None;
    }

    let mut out = Vec::with_capacity((cropped_width * cropped_height * 4) as usize);
    for y in y0..y1 {
        let start = ((y as usize * width as usize) + x0 as usize) * 4;
        let end = start + cropped_width as usize * 4;
        out.extend_from_slice(rgba.get(start..end)?);
    }
    // The origin is returned as well, because the boxes the recogniser
    // reports are relative to the cropped image and have to be moved back
    // to where they are in the frame.
    Some((out, cropped_width, cropped_height, x0, y0))
}

/// Each engine needs its own scratch file: two engines sharing one path race
/// and read each other's partially written image.
fn scratch_path() -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("gnome-ocr-frame-{}-{id}.png", std::process::id()))
}

fn write_png(path: &Path, rgba: &[u8], width: u32, height: u32) -> Result<()> {
    let buffer = ImageBuffer::<Rgba<u8>, _>::from_raw(width, height, rgba.to_vec())
        .context("build image buffer")?;
    let file = std::fs::File::create(path).context("create scratch png")?;
    let mut writer = std::io::BufWriter::new(file);
    buffer
        .write_to(&mut writer, ImageFormat::Png)
        .context("encode png")?;
    writer.flush().context("flush scratch png")?;
    Ok(())
}

fn resolve(candidate: &Path) -> bool {
    if candidate == Path::new("qocrd") {
        which("qocrd").is_some()
    } else {
        candidate.is_file()
    }
}

fn which(binary: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(binary))
            .find(|candidate| candidate.is_file())
    })
}
/// Where captured frames are written for inspection.
fn dump_dir() -> Option<PathBuf> {
    let dir = match std::env::var_os("GNOME_OCR_DUMP_DIR") {
        Some(path) => PathBuf::from(path),
        None => crate::paths::state_dir().join("frames"),
    };
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

impl Engine {
    fn report_frame(&mut self, width: u32, height: u32) {
        let n = self.frames;
        if n <= 3 || n.is_multiple_of(30) {
            tracing::info!("frame {n} captured: {width}x{height}");
        }

        let Some(dir) = self.dump_dir.clone() else { return };
        if !self.dump_every && self.dumped >= 3 {
            return;
        }
        self.dumped += 1;
        let target = dir.join(format!("frame-{n:04}.png"));
        match std::fs::copy(&self.scratch, &target) {
            Ok(_) => tracing::info!("frame {n} written to {}", target.display()),
            Err(err) => tracing::warn!("could not write frame {n}: {err}"),
        }
    }
}

fn mean_luma(rgba: &[u8]) -> f32 {
    let mut total = 0u64;
    for [r, g, b, _] in rgba.as_chunks::<4>().0 {
        total += u64::from(*r) + u64::from(*g) + u64::from(*b);
    }
    let samples = rgba.len() / 4 * 3;
    if samples == 0 {
        return 0.0;
    }
    total as f32 / samples as f32
}

#[cfg(test)]
mod tests {

    /// The engine that comes with a bundle of the app sits next to the
    /// program, and is looked for before the ones on the machine.
    #[test]
    fn the_engine_next_to_the_program_is_looked_for_first() {
        let program = Path::new("/somewhere/usr/bin/gnome-ocr");
        let candidates = qocrd_candidates_from(Some(program));
        assert_eq!(
            candidates[0],
            PathBuf::from("/somewhere/usr/bin/qocrd"),
            "the copy next to the program comes first, got {candidates:?}"
        );
        assert!(
            candidates.len() > 1,
            "the ones on the machine are looked at as well"
        );
    }

    use super::*;

    /// Renders `text` to an opaque white image and returns it as RGBA8.
    ///
    /// ARgb32 is premultiplied BGRA in native byte order. Everything drawn
    /// here is fully opaque, so alpha needs no unpremultiplying.
    fn render_text(text: &str) -> (u32, u32, Vec<u8>) {
        render_text_at(text, 40.0)
    }

    /// Draws the text at a given distance from the left of the frame.
    fn render_text_at(text: &str, start_x: f64) -> (u32, u32, Vec<u8>) {
        use gtk::cairo;

        let width = 1000i32;
        let height = 220i32;
        let mut surface =
            cairo::ImageSurface::create(cairo::Format::ARgb32, width, height).expect("surface");
        {
            let context = cairo::Context::new(&surface).expect("context");
            context.set_source_rgb(1.0, 1.0, 1.0);
            context.paint().expect("paint background");
            context.set_source_rgb(0.0, 0.0, 0.0);
            context.select_font_face("sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
            context.set_font_size(64.0);
            context.move_to(start_x, 150.0);
            context.show_text(text).expect("draw text");
        }

        surface.flush();
        let stride = surface.stride() as usize;
        let data = surface.data().expect("surface data");

        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for row in 0..height as usize {
            for column in 0..width as usize {
                let offset = row * stride + column * 4;
                rgba.extend_from_slice(&[
                    data[offset + 2],
                    data[offset + 1],
                    data[offset],
                    data[offset + 3],
                ]);
            }
        }
        (width as u32, height as u32, rgba)
    }

    #[test]
    fn recognises_text_rendered_into_a_frame() {
        let (width, height, rgba) = render_text("HELLO WORLD 12345");
        let mut engine = Engine::spawn(
            false,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None)),
        ).expect("spawn qocrd");
        let result = engine.scan(&rgba, width, height).expect("scan should succeed");

        assert!(!result.lines.is_empty(), "qocrd returned no text");
        let recognised = result
            .lines
            .iter()
            .map(|detection| detection.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            recognised.contains("HELLO"),
            "expected HELLO in recognised text, got {recognised:?}"
        );
    }

    /// A frame with no text used to fail: the reader looked for a second JSON
    /// value and hit the NUL byte qocrd terminates replies with.
    #[test]
    fn returns_no_text_for_a_blank_frame_instead_of_erroring() {
        let (width, height, rgba) = render_text("");
        let mut engine = Engine::spawn(
            false,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None)),
        ).expect("spawn qocrd");
        let result = engine.scan(&rgba, width, height).expect("blank frame must not error");
        assert!(result.lines.is_empty(), "expected no lines, got {:?}", result.lines);
    }

    /// qocrd ends every reply with a NUL byte, so the reader has to split
    /// on it. Reading the output as a stream of JSON values would fail on
    /// the byte after the first reply.
    #[test]
    fn replies_are_split_on_the_nul_that_ends_them() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let data = "{\"lines\":[{\"text\":\"one\"}]}\0{\"lines\":[{\"text\":\"two\"}]}\0";

        read_replies(std::io::BufReader::new(std::io::Cursor::new(data)), sender);

        let first = receiver.recv().expect("first reply").expect("first parses");
        let second = receiver.recv().expect("second reply").expect("second parses");
        assert_eq!(first.lines[0].text, "one");
        assert_eq!(second.lines[0].text, "two");
    }

    /// A reply that is not followed by a NUL is still read at the end of
    /// the stream.
    #[test]
    fn a_last_reply_without_a_nul_is_still_read() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let data = "{\"lines\":[{\"text\":\"only\"}]}";

        read_replies(std::io::BufReader::new(std::io::Cursor::new(data)), sender);

        let reply = receiver.recv().expect("reply").expect("parses");
        assert_eq!(reply.lines[0].text, "only");
    }

    /// The boxes the recogniser reports are relative to the image it was
    /// given, so they have to be moved back to where they are in the frame.
    #[test]
    fn a_crop_moves_the_boxes_back_to_where_they_are_in_the_frame() {
        let (width, height, rgba) = render_text_at("HELLO WORLD", 600.0);
        let region = Arc::new(Mutex::new(Some(crate::region::Region {
            x: 0.5,
            y: 0.0,
            width: 0.5,
            height: 1.0,
        })));
        let mut engine = Engine::spawn(
            false,
            Arc::new(AtomicBool::new(false)),
            region,
        )
        .expect("spawn qocrd");

        let result = engine.scan(&rgba, width, height).expect("scan");
        let detection = result
            .lines
            .first()
            .expect("the text in the right half should be found");
        assert!(
            detection.bounds.x >= 500.0,
            "the box should sit where the text is in the frame, not where it is in the crop: {:?}",
            detection.bounds
        );
    }

    #[test]
    fn rejects_a_frame_whose_size_does_not_match_its_pixels() {
        let mut engine = Engine::spawn(
            false,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None)),
        ).expect("spawn qocrd");
        let too_small = vec![0u8; 16];
        assert!(engine.scan(&too_small, 100, 100).is_err());
    }
}
