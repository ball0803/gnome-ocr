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

### One file

The app can be built into a single file that carries everything it needs:

```bash
bash scripts/package-appimage.sh
```

The result is `dist/gnome-ocr-x86_64.AppImage`. It carries the program, the
engine that reads the screen, a python of its own, the libraries that engine
needs, and the engine of Chrome that does the reading. It is checked as it is
built: given a picture, the engine inside it says how much it read from it.

The parts it is built from are found by looking, and can be named:

| What | Where it is looked for | Named by |
|---|---|---|
| the qocr checkout | `~/Downloads/qocr` | `QOCR_SRC` |
| the engine of Chrome | `~/.config/screen_ai` | `SCREEN_AI_DIR` |
| the python to put in it | one of its own, through `uv` | `PYTHON` |
| where the result goes | `./dist` | `OUT_DIR` |

The program still needs the GTK libraries and a way to reach the screen from
the machine it runs on, and both are there on any machine running GNOME.

The program is built against the C library of the machine it was built on, so
the bundle runs on that library and on anything newer.

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

**Preview** shows or hides the picture of what is captured, next to **Show as**.
The preview is a quarter of the width and the text the rest, and that is kept
when the window is resized.

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
detections are in the same place, and two choices of how the text is grouped:
whether the lines that belong to the same paragraph are joined into one line,
and whether the short entries that line up, such as the entries of a menu, are
gathered into one list. They can all be tuned while the app runs.

## The order of the text

The recogniser reports where each line was found, but not the order they
should be read in. That is worked out from the geometry, in the way the
document analysis literature does it:

- lines are grouped into paragraphs using the block the engine's layout
  model put them in, which knows things the geometry alone does not, such
  as a paragraph whose first line is indented. When the engine has grouped
  nothing, the left edge and the gap between the lines are used instead
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
- runs of short entries that share a left edge and are evenly spaced are
  gathered into one list, which is how the entries of a menu are kept
  together. An entry that is indented further along is at a different level
  and starts a list of its own

The lines of a paragraph are put back together the way the page had them.
The recogniser says whether a space follows each word it found, so where the
text is set with spaces between its words the lines are joined with a space,
and where it is not, as in Japanese, nothing goes between them.

The entries of a list are apart on the page, so the space that goes between
them is one of ours, and it is the ideographic space, which is how a gap is
marked in Japanese. The spaces that were on the page are left as they were.

Where the page has a large gap between two pieces of text, the break is kept
and the next piece is set out with an ideographic space, which is how a
paragraph is marked in Japanese.

No threshold used for this is an absolute size. The questions about the text
itself are answered against the size of the ordinary text, which comes from
the height that most of the lines share: how far the left edges of a column
may differ, how much space there may be between two lines of a paragraph,
and how much the spacing of the entries of a list may change are all
proportions of it, so they hold at any resolution.

The question of where one block ends and the next begins is answered against
the page itself, from the gaps between the lines, split into a lower and an
upper group. That one has to come from the page, because the gutters of a
page are its own.

The measure of the text, which says how wide a line of running text reaches,
is the width of the widest line on the page. An entry of a list is short
compared with it.

## Readings and lookups

The text that is recognised is Japanese, and Japanese is written without
spaces between its words, so the text is much harder to read than it looks.
Yomitan — the browser extension with the Japanese dictionaries — can say how
each word is read, and what it means.

**Show the readings**, in the settings, asks Yomitan for the readings and
shows them with the text:

```
人気（にんき）のおかず　ポトフ
```

The readings come from Yomitan, so it has to be running for them to be had;
the text that is copied is the plain text either way.

**Look up**, in the toolbar, asks the dictionary about the words that are
selected in the text, and shows what it says in a card.

### Looking up a word under the pointer

Hold the **scan modifier key** and move the pointer over a word in the
preview, and the word under it is looked up and the card appears beside the
pointer. This is how Yomitan works on a web page, and the app does the same
over the screen it is capturing.

Two settings control it, both in the settings:

| Setting | What it does |
|---|---|
| Scan modifier key | The key that has to be held down while the pointer moves. Without it, moving the pointer over the preview asks for nothing. |
| Scan delay | How long the pointer has to rest over a word, in milliseconds, before it is looked up. |

The word is found from where the recogniser said it was, so nothing is read
again for it, and the pointer has to rest before anything is asked, so that
sweeping the pointer across the screen does not fill the dictionary's
attention.

Yomitan has to be set up to answer requests from outside the browser. Its
API is served on `http://127.0.0.1:19633`, and the guide for setting it up
is in the [yomitan-api](https://github.com/Kuuuube/yomitan-api) project.
The app asks the API over the network, so it is worth knowing that the
requests go no further than the machine.

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

1. `GNOME_OCR_QOCRD`, if it is set
2. the copy next to the program, which is what a bundle carries
3. `~/.local/bin/qocrd`
4. `./qocr/.venv/bin/qocrd` inside a qocr checkout
5. `qocrd` on `PATH`

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