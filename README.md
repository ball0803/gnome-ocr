# GNOME OCR

Watch the screen, OCR what appears, and get new text on your clipboard.

Built as a qocr-style utility for GNOME on Wayland: capture comes from the
PipeWire ScreenCast portal, recognition comes from
[qocr](https://github.com/ssusurr/qocr)'s `qocrd`, and the result lands in
a small GTK4 window.

## What it does

- Captures your screen through the xdg-desktop-portal ScreenCast interface
  (PipeWire). You choose which screen and must approve the first capture.
- Runs every captured frame through `qocrd`, which recognises Japanese by
  default.
- Shows every newly recognised line in a scrollable text view.
- Optionally copies each new line to the clipboard as it appears.
- Keeps a de-duplication set, so text that stays on screen is not copied
  repeatedly — only genuinely new content is.

## Requirements

| Component | Version used here |
|---|---|
| Fedora | 42 |
| GNOME | 48 |
| GTK | 4.18 (`gtk4-rs` 0.11) |
| Rust | 1.85+ (edition 2024) |
| `qocrd` | from a local qocr checkout, see below |

Screen capture on Wayland requires the portal, so the app is built for GNOME.
It should also work on any compositor that implements
`xdg-desktop-portal` with the ScreenCast interface.

## Build

```bash
cargo build --release
```

The binary lands at `target/release/gnome-ocr`.

## Run

```bash
./target/release/gnome-ocr
```

Press **Start OCR**, then approve the screen-capture prompt. Recognised text
starts filling the window. Toggle **Auto-copy new text** to control whether new
lines are copied to your clipboard; **Copy all** copies everything shown;
**Clear** empties the view and resets the de-duplication set.

By default the engine only reports Japanese text. To accept any language:

```bash
./target/release/gnome-ocr --all-languages
```

## Where `qocrd` comes from

The app looks for the daemon in this order:

1. `~/.local/bin/qocrd`
2. `./qocr/.venv/bin/qocrd` inside a qocr checkout
3. `qocrd` on `PATH`

The qocr checkout needs the `scanfile` command, which accepts an existing
image path:

```
scanfile <japanese: true|false> <path-to-image>
```

If your qocr build does not have it, see `legacy/` for the earlier patch that
added it.

## Logs

Logs go to stderr and, in parallel, to:

```
${XDG_STATE_HOME:-~/.local/state}/gnome-ocr/gnome-ocr.log
```

Set `RUST_LOG` to change verbosity, e.g. `RUST_LOG=debug ./target/release/gnome-ocr`.

## Design

```
src/capture.rs   PipeWire ScreenCast; emits RGBA frames on a bounded channel
src/engine.rs    qocrd child process; PNG encode + scan, auto-restart on failure
src/pipeline.rs  capture -> OCR -> de-duplicate, on its own thread
src/ui.rs        GTK4 window, clipboard handling, event loop
```

Capture, PNG encoding, and OCR are all blocking work, so they live on a
dedicated OS thread rather than an async task. The UI thread only ever touches
widgets. The pipeline thread owns the capture session, so stopping the pipeline
also tears down the PipeWire stream.

Frames are dropped rather than queued when OCR falls behind; a queued backlog of
stale frames would only add latency.

## `legacy/`

Earlier work targeted a transparent overlay drawn by a GNOME Shell extension,
because an ordinary GTK window cannot sit above other applications on Wayland.
That approach is no longer used and none of it is compiled — see
`legacy/src/` and `legacy/examples/`. It is kept only for reference, including
a screenshot-capture experiment that must not be used.