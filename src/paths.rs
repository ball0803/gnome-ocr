use std::path::PathBuf;

/// Directory where gnome-ocr keeps its log and captured frames.
pub fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
        })
        .unwrap_or_else(std::env::temp_dir)
        .join("gnome-ocr")
}
