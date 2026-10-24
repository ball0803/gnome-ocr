//! OCR engine: wraps the `qocrd` daemon over stdio and turns frames into text.
//!
//! Frame -> PNG -> scratch file -> `qocrd scanfile` -> JSON -> text lines.
//! The engine keeps the qocrd child alive between scans and replaces it
//! automatically if it dies, so callers never deal with daemon lifecycle.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{Context, Result};
use image::{ImageBuffer, ImageFormat, Rgba};
use serde::Deserialize;

/// Where the qocrd wrapper lives. First candidate that resolves wins.
const QOCRD_CANDIDATES: &[&str] = &[
    "/home/camel/.local/bin/qocrd",
    "/home/camel/Downloads/qocr/.venv/bin/qocrd",
    "qocrd",
];

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
}

/// Recognised text from a single frame.
#[derive(Debug, Default, Clone)]
pub struct ScanResult {
    pub lines: Vec<String>,
}

/// The qocrd child process. Owns its own teardown so it can be swapped out.
struct Process {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
}

impl Process {
    fn spawn() -> Result<Self> {
        let program = QOCRD_CANDIDATES
            .iter()
            .copied()
            .find(|candidate| resolve(candidate))
            .context(
                "could not find qocrd; tried ~/.local/bin/qocrd, \
                 the qocr virtualenv, and PATH",
            )?;

        tracing::info!("starting qocrd from {program}");
        let mut child = Command::new(program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("spawn qocrd at {program}"))?;

        let stdin = child.stdin.take().context("qocrd stdin")?;
        let stdout = child.stdout.take().context("qocrd stdout")?;
        Ok(Self {
            child,
            stdin,
            stdout,
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
    japanese_only: bool,
    scratch: PathBuf,
    dump_dir: Option<PathBuf>,
    dump_every: bool,
    frames: u64,
    dumped: u32,
}

impl Engine {
    pub fn spawn(japanese_only: bool) -> Result<Self> {
        Ok(Self {
            process: Process::spawn()?,
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
        let outcome = match write_png(&self.scratch, rgba, width, height) {
            Ok(()) => {
                self.report_frame(width, height);
                self.exchange()
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
                self.process = Process::spawn()?;
                let _ = std::fs::remove_file(&self.scratch);
                Err(err)
            }
        }
    }

    fn exchange(&mut self) -> Result<ScanResult> {
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

        // qocrd terminates each reply with a NUL byte, so read exactly one
        // JSON value. Trying to read a second value lands on that NUL and
        // fails to parse.
        let response = serde_json::Deserializer::from_reader(&mut self.process.stdout)
            .into_iter::<ScanFileResponse>()
            .next()
            .context("qocrd closed stdout without replying")?
            .context("parse qocrd response")?;

        let mut lines: Vec<String> = Vec::new();
        if !response.text.trim().is_empty() {
            lines.extend(
                response
                    .text
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(ToOwned::to_owned),
            );
        }
        lines.extend(
            response
                .lines
                .into_iter()
                .map(|line| line.text.trim().to_owned())
                .filter(|line| !line.is_empty()),
        );

        // `text` and `lines` frequently repeat the same content.
        let mut seen = std::collections::HashSet::new();
        lines.retain(|line| seen.insert(line.clone()));

        Ok(ScanResult { lines })
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.scratch);
    }
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

fn resolve(candidate: &str) -> bool {
    if candidate == "qocrd" {
        which(candidate).is_some()
    } else {
        Path::new(candidate).is_file()
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
    use super::*;

    /// Renders `text` to an opaque white image and returns it as RGBA8.
    ///
    /// ARgb32 is premultiplied BGRA in native byte order. Everything drawn
    /// here is fully opaque, so alpha needs no unpremultiplying.
    fn render_text(text: &str) -> (u32, u32, Vec<u8>) {
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
            context.move_to(40.0, 150.0);
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
        let mut engine = Engine::spawn(false).expect("spawn qocrd");
        let result = engine.scan(&rgba, width, height).expect("scan should succeed");

        assert!(!result.lines.is_empty(), "qocrd returned no text");
        let recognised = result.lines.join(" ");
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
        let mut engine = Engine::spawn(false).expect("spawn qocrd");
        let result = engine.scan(&rgba, width, height).expect("blank frame must not error");
        assert!(result.lines.is_empty(), "expected no lines, got {:?}", result.lines);
    }

    #[test]
    fn rejects_a_frame_whose_size_does_not_match_its_pixels() {
        let mut engine = Engine::spawn(false).expect("spawn qocrd");
        let too_small = vec![0u8; 16];
        assert!(engine.scan(&too_small, 100, 100).is_err());
    }
}
