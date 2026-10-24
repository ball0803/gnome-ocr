# GNOME OCR

Watch the screen, OCR what appears, and get new text on your clipboard.

Built as a qocr-style utility for GNOME on Wayland: capture comes from the
PipeWire ScreenCast portal, recognition comes from
[qocr](https://github.com/ssusurr/qocr)'s `qocrd`, and the result lands in
a small GTK4 window.

## What it does

- Captures through the xdg-desktop-portal ScreenCast interface (PipeWire).
  Both screens and windows are offered, so the portal dialog lets you pick
  exactly which screen or which application window to share. You must
  approve the first capture.
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

Press **Start OCR** and approve the capture dialog, then choose a screen or a
window in it. Recognised text starts filling the window.

Stopping is a real stop: the portal session is closed, the compositor stops
sharing, and the stream and the OCR daemon are released. Starting again opens
a new capture, which the portal may satisfy from the grant it already holds.

Stopping the share from the system menu ends the session from the
compositor's side. The app notices and says so, and the next start asks for
a new capture.

**Change window** captures something else: the current share is ended and the
portal asks again which screen or window to use, instead of restoring the
previous choice.

The preview shows what is being captured, scaled to fit the pane, along with
the kind of source and its size. It carries a crop rectangle:

- drag on the preview to draw one
- drag a handle, corner or edge, to resize it
- drag inside it to move it

Only the cropped part of the frame is recognised. The crop is kept as a
fraction of the frame, so it stays put when the captured window is resized.
**Whole capture** goes back to recognising everything.

**Show where text was found** draws a box around every line the recogniser
picked up, so you can see where the text on screen came from. The crop you
draw on the preview is kept as a part of the frame, so resizing the window
does not move it.

**Clear** empties the output and also forgets what has been seen, so text that
was already recognised is treated as new if it turns up again.

**Show as** chooses how the recognised text is presented. **Text** is the
plain box. **List** gives each screen its own entry, with the frame it was
seen in, the text read from it, and the time it was seen. More text from
the same screen is added to the entry that is already there rather than
making another one.

The portal gives every screen or window an identifier that stays the same
for as long as that thing is being captured, so entries are grouped by it
rather than by comparing the pictures, which would be slow and would
mistake two still frames for different ones.

Only the frame that carried new text is kept, and those pictures live in
memory only, so nothing is left behind on disk. They are shown at the size
they were made, rather than being scaled to fit.

**Settings** opens a panel with the two thresholds that decide whether two
detections are in the same place, and a choice of whether the lines that
belong to the same paragraph are joined into one line or each keeps its own.
They can all be tuned while the app runs.

## The order of the text

The recogniser reports where each line was found, but not the order they
should be read in. That is worked out from the geometry, in the way the
document analysis literature does it:

- lines are grouped into paragraphs, by their left edge and the gap between
  them
- the paragraphs are then cut up recursively at the widest run of
  whitespace, which recovers the reading order and the structure at once: a
  cut between columns is a level of the structure, and so is a cut between
  sections
- anything running across the whole width, such as a title, is taken out
  first so that it cannot break the cut
- a line that is much taller than the rest is a heading, and the size of the
  text tells the level
- lines that run downwards, which is how Japanese is often written, are kept
  apart and read from the right

The thresholds are all relative to the size of the ordinary text, so they
hold at any resolution.

## What counts as new text

Text that is still on screen gets recognised again on every frame, so it is
not inserted again. A detection counts as a repeat when its text has been
seen before and either

- the two boxes overlap, measured by Intersection over Union, or
- the two boxes are about the same size, even if they are in different
  places.

Otherwise it is new, and is inserted and copied.

Toggle **Auto-copy new text** to control whether new lines are copied to your
clipboard; **Copy all** copies everything shown; **Clear** empties the view
and resets the de-duplication set.

Compositors decide how a captured window behaves when it is moved, resized,
or minimised — some shrink the captured area to whatever is visible, so keep
the window on screen while capturing.

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

## Captured frames

Every frame handed to OCR is also written out as a PNG, so you can see
exactly what the recogniser was given:

```
${XDG_STATE_HOME:-~/.local/state}/gnome-ocr/frames/frame-0001.png
```

The first three frames are always saved. Set `GNOME_OCR_DUMP_ALL=1` to keep
every frame, and `GNOME_OCR_DUMP_DIR=/some/path` to write them elsewhere.
The file is a copy of the exact PNG that qocrd read, so it cannot differ
from what was recognised.

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