//! Cairo/Pango renderer for OCR boxes + region rectangles + Yomitan popups.
//!
//! Each function takes a Cairo `Context` and draws directly onto it. The GTK4 overlay
//! hooks `GtkDrawingArea::set_draw_func` and calls into here.
//!
//! Why not WebKit2GTK for the popup: Yomitan's HTML popup requires JS execution + CSS that
//! we don't need for v1. Rendering the JSON fields directly with Pango is faster, lighter,
//! and easier to style consistently with the rest of the overlay.

use cairo::{Context, Format, ImageSurface};
use pango::{FontDescription, Layout};
use std::f64::consts::PI;

use crate::config::{Config, YomitanConfig};
use crate::types::{Aabb, Line, OcrResult, Point};

pub struct RenderState {
    pub regions: Vec<RegionState>,
    pub drag: Option<Aabb>,
    pub popup: Option<PopupState>,
}

pub struct RegionState {
    pub monitor: String,
    pub result: Option<OcrResult>,
    pub flash_until: Option<std::time::Instant>,
}

#[derive(Debug, Clone)]
pub struct PopupState {
    pub anchor: Point,
    pub lines: Vec<String>,
    pub readings: Vec<String>,
    pub definitions: Vec<String>,
}

impl RenderState {
    pub fn new() -> Self {
        Self {
            regions: Vec::new(),
            drag: None,
            popup: None,
        }
    }
}

fn parse_hex(s: &str) -> Option<(f64, f64, f64, f64)> {
    let s = s.trim_start_matches('#');
    let (r, g, b, a) = match s.len() {
        6 => (
            u8::from_str_radix(&s[0..2], 16).ok()?,
            u8::from_str_radix(&s[2..4], 16).ok()?,
            u8::from_str_radix(&s[4..6], 16).ok()?,
            255u8,
        ),
        8 => (
            u8::from_str_radix(&s[0..2], 16).ok()?,
            u8::from_str_radix(&s[2..4], 16).ok()?,
            u8::from_str_radix(&s[4..6], 16).ok()?,
            u8::from_str_radix(&s[6..8], 16).ok()?,
        ),
        _ => return None,
    };
    Some((r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0, a as f64 / 255.0))
}

fn fill_rgba(cr: &Context, hex: &str) {
    if let Some((r, g, b, a)) = parse_hex(hex) {
        cr.set_source_rgba(r, g, b, a);
    } else {
        cr.set_source_rgba(1.0, 0.0, 0.0, 0.5);
    }
}

/// Top-level draw function: walks all regions and renders them. Used by overlay.rs's
/// `GtkDrawingArea::set_draw_func`.
pub fn draw_state(cr: &Context, width: i32, height: i32, state: &RenderState, config: &Config) {
    // Clear to fully transparent.
    cr.set_operator(cairo::Operator::Source);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
    cr.paint().ok();
    cr.set_operator(cairo::Operator::Over);

    for region_state in &state.regions {
        if let Some(sel) = &state.drag {
            draw_region_rect(cr, sel, &config.region_border, &config.region_background);
        } else if let Some(result) = &region_state.result {
            draw_ocr_result(cr, result, config);
        }
    }
    if let Some(sel) = &state.drag {
        draw_region_rect(cr, sel, &config.region_border, &config.region_background);
    }
    if let Some(popup) = &state.popup {
        draw_popup(cr, popup, &config.yomitan);
    }

    // Idle indicator: small green dot top-right so the user can confirm the overlay is
    // running (without it, a transparent overlay over a dark desktop looks like a black screen).
    if state.popup.is_none() {
        draw_idle_badge(cr, width, height);
    }
    let _ = config;
}

fn draw_idle_badge(cr: &Context, width: i32, height: i32) {
    let pad = 12.0;
    let dot_r = 6.0;
    let cx = width as f64 - pad - dot_r;
    let cy = pad + dot_r;
    cr.set_source_rgba(0.3, 0.85, 0.55, 0.85);
    cr.arc(cx, cy, dot_r, 0.0, 2.0 * std::f64::consts::PI);
    cr.fill().ok();
    let _ = height;
}

pub fn draw_region_rect(cr: &Context, rect: &Aabb, border_hex: &str, bg_hex: &str) {
    fill_rgba(cr, bg_hex);
    cr.rectangle(rect.x, rect.y, rect.width, rect.height);
    cr.fill().ok();
    fill_rgba(cr, border_hex);
    cr.set_line_width(2.0);
    cr.rectangle(rect.x, rect.y, rect.width, rect.height);
    cr.stroke().ok();
}

pub fn draw_ocr_result(cr: &Context, result: &OcrResult, config: &Config) {
    let margin = config.box_margin as f64;
    let region_x = result.region.x as f64;
    let region_y = result.region.y as f64;

    for line in &result.lines {
        let padded = Aabb {
            x: line.aabb.x - margin,
            y: line.aabb.y - margin,
            width: line.aabb.width + margin * 2.0,
            height: line.aabb.height + margin * 2.0,
        };
        fill_rgba(cr, &config.background);
        cr.rectangle(padded.x, padded.y, padded.width, padded.height);
        cr.fill().ok();

        fill_rgba(cr, &config.border_color);
        cr.set_line_width(config.border as f64);
        cr.rectangle(padded.x, padded.y, padded.width, padded.height);
        cr.stroke().ok();

        // Per-symbol faint boxes (debug-visible).
        for word in &line.words {
            for sym in &word.symbols {
                let sb = sym.aabb;
                fill_rgba(cr, "#20d0d0d0");
                cr.rectangle(sb.x, sb.y, sb.width, sb.height);
                cr.fill().ok();
            }
        }

        if config.show_overlay {
            draw_text(
                cr,
                &line.text,
                line.aabb.x,
                line.aabb.y + line.aabb.height * 0.85,
                line.aabb.width,
                &config.border_color,
            );
        }
        let _ = line;
    }

    let region_rect = Aabb {
        x: region_x,
        y: region_y,
        width: result.region.w as f64,
        height: result.region.h as f64,
    };
    draw_region_rect(cr, &region_rect, &config.region_border, &config.region_background);
}

fn draw_text(cr: &Context, text: &str, x: f64, y: f64, width: f64, color: &str) {
    if text.is_empty() {
        return;
    }
    let pango_ctx = pangocairo::functions::create_context(cr);
    let layout = Layout::new(&pango_ctx);
    let mut font = FontDescription::from_string("Sans 11");
    font.set_weight(pango::Weight::Bold);
    layout.set_font_description(Some(&font));
    layout.set_width((width.max(1.0) * pango::SCALE as f64) as i32);
    layout.set_text(text);
    fill_rgba(cr, color);
    cr.move_to(x, y);
    pangocairo::functions::show_layout(cr, &layout);
}

pub fn draw_popup(cr: &Context, popup: &PopupState, cfg: &YomitanConfig) {
    let pad = 12.0;
    let line_h = 22.0;
    let min_width = 280.0;
    let total_lines = popup.lines.len() + popup.readings.len() + popup.definitions.len();
    let width = min_width;
    let height = (total_lines.max(1) as f64) * line_h + pad * 2.0;
    let x = popup.anchor.x.max(pad);
    let y = popup.anchor.y.max(pad);

    fill_rgba(cr, &cfg.background_color);
    rounded_rect(cr, x, y, width, height, 6.0);
    cr.fill().ok();

    fill_rgba(cr, &cfg.border_color);
    rounded_rect(cr, x, y, width, height, 6.0);
    cr.set_line_width(1.0);
    cr.stroke().ok();

    let pango_ctx = pangocairo::functions::create_context(cr);
    let mut yy = y + pad;
    for chunk in [&popup.lines, &popup.readings, &popup.definitions] {
        for line in chunk.iter() {
            let layout = Layout::new(&pango_ctx);
            let mut font = FontDescription::from_string("Sans 11");
            font.set_weight(pango::Weight::Bold);
            layout.set_font_description(Some(&font));
            layout.set_width(((width - pad * 2.0) * pango::SCALE as f64) as i32);
            layout.set_text(line);
            fill_rgba(cr, &cfg.foreground_color);
            cr.move_to(x + pad, yy);
            pangocairo::functions::show_layout(cr, &layout);
            yy += line_h;
        }
    }
}

fn rounded_rect(cr: &Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
}

/// Convenience: render the whole state onto an ImageSurface the given size, return it.
pub fn render_to_surface(
    width: i32,
    height: i32,
    state: &RenderState,
    config: &Config,
) -> ImageSurface {
    let surface = ImageSurface::create(Format::ARgb32, width, height).expect("ImageSurface");
    let cr = Context::new(&surface).expect("cairo Context");
    draw_state(&cr, width, height, state, config);
    surface
}

/// Helper for callers that need a hex-string -> (r,g,b,a) tuple.
pub fn rgba(hex: &str) -> (f64, f64, f64, f64) {
    parse_hex(hex).unwrap_or((1.0, 0.0, 1.0, 0.5))
}

#[allow(dead_code)]
pub fn _unused_line(_line: &Line) {}