//! OCR engine: wraps the `qocrd` daemon over stdio and turns frames into text.
//!
//! Frame -> PNG -> scratch file -> `qocrd scanfile` -> JSON -> text lines.
//! The engine keeps the qocrd child alive between scans and replaces it
//! automatically if it dies, so callers never deal with daemon lifecycle.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{anyhow, Context, Result};
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
}

impl Engine {
    pub fn spawn(japanese_only: bool) -> Result<Self> {
        Ok(Self {
            process: Process::spawn()?,
            japanese_only,
        })
    }

    /// Scan one RGBA frame and return the recognised lines.
    ///
    /// A protocol failure replaces the qocrd child so the next scan starts
    /// from a clean daemon, then reports the error to the caller.
    pub fn scan(&mut self, rgba: &[u8], width: u32, height: u32) -> Result<ScanResult> {
        let scratch = scratch_path();
        let outcome = write_png(&scratch, rgba, width, height)
            .and_then(|()| self.exchange(&scratch));

        match outcome {
            Ok(lines) => {
                let _ = std::fs::remove_file(&scratch);
                Ok(ScanResult { lines })
            }
            Err(err) => {
                tracing::warn!("qocrd scan failed ({err}); replacing the daemon");
                self.process = Process::spawn()?;
                let _ = std::fs::remove_file(&scratch);
                Err(err)
            }
        }
    }

    fn exchange(&mut self, scratch: &Path) -> Result<Vec<String>> {
        let command = format!(
            "scanfile {} {}\n",
            if self.japanese_only { "true" } else { "false" },
            scratch.display()
        );
        self.process
            .stdin
            .write_all(command.as_bytes())
            .context("write command to qocrd")?;
        self.process.stdin.flush().context("flush qocrd stdin")?;

        let responses =
            serde_json::Deserializer::from_reader(&mut self.process.stdout).into_iter::<ScanFileResponse>();

        let mut lines: Vec<String> = Vec::new();
        // qocrd may emit a bare status envelope before the real payload, so
        // keep reading values until one carries text (bounded, so a silent
        // daemon cannot spin here forever).
        for response in responses.take(16) {
            let response = response.context("parse qocrd response")?;
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
            if !lines.is_empty() {
                break;
            }
        }

        if lines.is_empty() {
            return Err(anyhow!("qocrd returned no text"));
        }

        // `text` and `lines` frequently repeat the same content.
        let mut seen = std::collections::HashSet::new();
        lines.retain(|line| seen.insert(line.clone()));
        Ok(lines)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(scratch_path());
    }
}

fn scratch_path() -> PathBuf {
    std::env::temp_dir().join("gnome-ocr-frame.png")
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

    #[test]
    fn rejects_a_frame_whose_size_does_not_match_its_pixels() {
        let mut engine = Engine::spawn(false).expect("spawn qocrd");
        let too_small = vec![0u8; 16];
        assert!(engine.scan(&too_small, 100, 100).is_err());
    }
}
