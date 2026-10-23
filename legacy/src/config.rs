//! Configuration loader.
//!
//! Reads `~/.config/qocr/config.json` (same path the original qocr uses) so users can carry
//! over their config without rewriting. All fields are optional with sensible defaults.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[allow(unused_imports)]
use notify::Watcher;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub box_margin: i32,
    pub border: i32,
    pub japanese_only: bool,
    pub auto_rescan: bool,
    pub auto_rescan_delay: f64,
    pub auto_rescan_delay_unchanged: f64,
    pub overlay_on_hover: bool,
    pub show_overlay: bool,
    pub hide_overlay_on_rescan: bool,
    pub background: String,
    pub selected_border: String,
    pub selected_background: String,
    pub border_color: String,
    pub region_border: String,
    pub region_background: String,
    pub yomitan: YomitanConfig,
    pub anki: AnkiConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            box_margin: 15,
            border: 1,
            japanese_only: true,
            auto_rescan: false,
            auto_rescan_delay: 0.0,
            auto_rescan_delay_unchanged: 0.1,
            overlay_on_hover: true,
            show_overlay: true,
            hide_overlay_on_rescan: false,
            background: "#50000000".into(),
            selected_border: "#cc56b7a5".into(),
            selected_background: "#6656b7a5".into(),
            border_color: "#50d0d0d0".into(),
            region_border: "#cccc6633".into(),
            region_background: "#26cc6633".into(),
            yomitan: YomitanConfig::default(),
            anki: AnkiConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct YomitanConfig {
    pub background_color: String,
    pub foreground_color: String,
    pub border_color: String,
    pub separator_color: String,
    pub foreground_secondary_color: String,
    pub background_secondary_color: String,
    pub extra_css: String,
    pub api_url: String,
    pub text_scan_length: i32,
    pub lookup_max_distance: i32,
    pub fetch_audio: bool,
    pub auto_play_first_audio: bool,
}

impl Default for YomitanConfig {
    fn default() -> Self {
        Self {
            background_color: "#121212".into(),
            foreground_color: "#d0d0d0".into(),
            border_color: "#56b7a5".into(),
            separator_color: "#505050".into(),
            foreground_secondary_color: "#909090".into(),
            background_secondary_color: "#303030".into(),
            extra_css: "".into(),
            api_url: "http://127.0.0.1:19633".into(),
            text_scan_length: 16,
            lookup_max_distance: 10,
            fetch_audio: false,
            auto_play_first_audio: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct AnkiConfig {
    pub enable: bool,
    pub anki_connect_url: String,
    pub deck: String,
    pub model: String,
    pub tags: Vec<String>,
    pub allow_duplicate: bool,
    pub fields: std::collections::HashMap<String, String>,
}

impl Default for AnkiConfig {
    fn default() -> Self {
        let mut fields = std::collections::HashMap::new();
        fields.insert("Expression".into(), "{expression}".into());
        fields.insert(
            "ExpressionFurigana".into(),
            "{furigana-plain}".into(),
        );
        fields.insert("ExpressionReading".into(), "{reading}".into());
        fields.insert("ExpressionAudio".into(), "{audio}".into());
        fields.insert(
            "MainDefinition".into(),
            "{single-glossary-jitendexorg-2025-06-01}".into(),
        );
        fields.insert(
            "Sentence".into(),
            "{cloze-prefix}<b>{cloze-body}</b>{cloze-suffix}".into(),
        );
        fields.insert("Glossary".into(), "{glossary}".into());
        fields.insert("IsWordAndSentenceCard".into(), "x".into());
        fields.insert("PitchPosition".into(), "{pitch-accent-positions}".into());
        fields.insert(
            "PitchCategories".into(),
            "{pitch-accent-categories}".into(),
        );
        fields.insert("Frequency".into(), "{frequency-harmonic-rank}".into());
        fields.insert("FreqSort".into(), "{frequency-harmonic-rank}".into());
        Self {
            enable: true,
            anki_connect_url: "http://127.0.0.1:8765".into(),
            deck: "Mining".into(),
            model: "Lapis".into(),
            tags: vec!["qocr".into()],
            allow_duplicate: true,
            fields,
        }
    }
}

impl Config {
    pub fn config_path() -> Result<PathBuf> {
        let xdg = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
            .context("no XDG_CONFIG_HOME or HOME")?;
        Ok(xdg.join("qocr").join("config.json"))
    }

    pub fn load() -> Result<Self> {
        Self::load_from(&Self::config_path()?)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            tracing::info!("config {} not found, using defaults", path.display());
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        let cfg: Self = serde_json::from_str(&raw)
            .with_context(|| format!("parse config {}", path.display()))?;
        tracing::info!("loaded config from {}", path.display());
        Ok(cfg)
    }

    /// Watch the config file and call `on_change` whenever it changes. Returns the JoinHandle.
    pub fn watch<F>(path: PathBuf, on_change: F) -> std::io::Result<std::thread::JoinHandle<()>>
    where
        F: Fn() + Send + 'static,
    {
        use std::sync::mpsc;
        use std::time::Duration;

        std::thread::Builder::new()
            .name("config-watcher".into())
            .spawn(move || {
                let (tx, rx) = mpsc::channel();
                let mut watcher = match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                    let _ = tx.send(res);
                }) {
                    Ok(w) => w,
                    Err(e) => {
                        tracing::error!("config watcher init failed: {e}");
                        return;
                    }
                };
                if let Err(e) = watcher.watch(&path, notify::RecursiveMode::NonRecursive) {
                    tracing::error!("config watch failed: {e}");
                    return;
                }
                // Debounce: collapse bursts of modify events.
                loop {
                    let _ = rx.recv();
                    let debounce = Duration::from_millis(150);
                    let cutoff = std::time::Instant::now() + debounce;
                    while let Ok(Ok(_)) = rx.recv_timeout(cutoff.saturating_duration_since(std::time::Instant::now())) {
                        // drain
                    }
                    on_change();
                }
            })
    }
}