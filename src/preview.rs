//! A small PNG of the captured frame, for the preview pane.

use std::io::Cursor;

use image::{ImageBuffer, ImageFormat, Rgba};

/// Builds a PNG of the frame, scaled down to at most `max_width`.
///
/// Returns None when the frame cannot be read.
pub fn preview_png(rgba: &[u8], width: u32, height: u32, max_width: u32) -> Option<Vec<u8>> {
    if width == 0 || height == 0 {
        return None;
    }
    let (preview_width, preview_height, pixels) = downscale(rgba, width, height, max_width);
    let buffer = ImageBuffer::<Rgba<u8>, _>::from_raw(preview_width, preview_height, pixels)?;
    let mut out = Vec::new();
    buffer
        .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .ok()?;
    Some(out)
}

/// Nearest-neighbour downscale, which is enough for a preview.
fn downscale(rgba: &[u8], width: u32, height: u32, max_width: u32) -> (u32, u32, Vec<u8>) {
    if width <= max_width {
        return (width, height, rgba.to_vec());
    }
    let new_width = max_width;
    let new_height = ((height as u64 * max_width as u64) / width as u64).max(1) as u32;

    let mut out = vec![0u8; new_width as usize * new_height as usize * 4];
    for y in 0..new_height {
        let source_row = (y as u64 * height as u64 / new_height as u64) as usize;
        for x in 0..new_width {
            let source_column = (x as u64 * width as u64 / new_width as u64) as usize;
            let source = (source_row * width as usize + source_column) * 4;
            let target = (y as usize * new_width as usize + x as usize) * 4;
            if let Some(pixel) = rgba.get(source..source + 4) {
                out[target..target + 4].copy_from_slice(pixel);
            }
        }
    }
    (new_width, new_height, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preview_is_smaller_than_the_frame() {
        let width = 100u32;
        let height = 50u32;
        let rgba = vec![128u8; (width * height * 4) as usize];
        let png = preview_png(&rgba, width, height, 40).expect("preview");
        assert!(!png.is_empty());
        assert_eq!(&png[1..4], b"PNG");
    }

    #[test]
    fn a_frame_that_is_already_small_is_kept() {
        let rgba = vec![7u8; (10 * 5 * 4) as usize];
        let png = preview_png(&rgba, 10, 5, 640).expect("preview");
        assert!(!png.is_empty());
    }

    #[test]
    fn an_empty_frame_has_no_preview() {
        assert!(preview_png(&[], 0, 0, 640).is_none());
    }
}
