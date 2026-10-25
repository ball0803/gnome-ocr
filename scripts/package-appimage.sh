#!/usr/bin/env bash
#
# Builds the app into a single file.
#
# The result is an AppImage that carries the program, the engine that reads
# the screen, and everything the engine needs, so that it can be handed to
# someone else and run without installing anything.
#
# What goes in:
#
#   usr/bin/gnome-ocr              the program
#   usr/bin/qocrd                  a launcher for the engine
#   usr/lib/gnome-ocr/qocrd/       the engine, as it is written in python
#   usr/lib/gnome-ocr/python/      a python of its own, and the two
#                                  libraries the engine needs
#   usr/lib/gnome-ocr/screen-ai/   the engine of Chrome, which reads the
#                                  screen and gives back the text
#
# What the program needs from the machine it runs on: the GTK libraries, and
# a way to reach the screen. Both are there on any machine running GNOME.
#
# The pieces it is built from are found by looking, and can be named:
#
#   QOCR_SRC           the qocr checkout          (default ~/Downloads/qocr)
#   SCREEN_AI_DIR      the Chrome engine          (default ~/.config/screen_ai)
#   PYTHON             a python to put in the bundle
#   OUT_DIR            where to put the result    (default ./dist)
#
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(dirname "$here")"

QOCR_SRC="${QOCR_SRC:-$HOME/Downloads/qocr}"
SCREEN_AI_DIR="${SCREEN_AI_DIR:-$HOME/.config/screen_ai}"
OUT_DIR="${OUT_DIR:-$root/dist}"
PYTHON_VERSION="${PYTHON_VERSION:-3.13}"

APP_NAME=gnome-ocr
APP_ID=dev.local.gnome_ocr
APPDIR="$OUT_DIR/$APP_NAME.AppDir"
APPIMAGE="$OUT_DIR/$APP_NAME-x86_64.AppImage"

say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------- the parts

say "building the program"
cargo build --release --manifest-path "$root/Cargo.toml"
[ -x "$root/target/release/$APP_NAME" ] || die "the program was not built"

[ -d "$QOCR_SRC/src/qocrd" ] || die "no qocr checkout at $QOCR_SRC"
[ -f "$SCREEN_AI_DIR/resources/libchromescreenai.so" ] ||
    die "no Chrome engine at $SCREEN_AI_DIR/resources"

# A python of its own, so that the bundle does not lean on the machine.
if [ -z "${PYTHON:-}" ]; then
    if command -v uv >/dev/null 2>&1; then
        say "asking uv for a python $PYTHON_VERSION"
        uv python install "$PYTHON_VERSION" >&2
        PYTHON="$(uv python find "$PYTHON_VERSION")"
    else
        die "no python to bundle: install uv, or name one with PYTHON="
    fi
fi
[ -x "$PYTHON" ] || die "the python at $PYTHON is not there"

python_home="$(cd "$(dirname "$PYTHON")/.." && pwd)"
stdlib="$(ls -d "$python_home"/lib/python3.* 2>/dev/null | head -1)"
[ -n "$stdlib" ] || die "the python at $PYTHON has no library"
stdlib_name="$(basename "$stdlib")"
say "python: $PYTHON ($stdlib_name)"

# The two libraries the engine needs, put together for this python alone.
say "collecting the libraries the engine needs"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
uv venv --python "$PYTHON" "$work/venv" >&2
uv pip install --python "$work/venv/bin/python" pillow protobuf >&2

# ------------------------------------------------------------- the assembly

say "assembling the bundle"
rm -rf "$APPDIR"
python_root="$APPDIR/usr/lib/$APP_NAME/python"
mkdir -p "$APPDIR/usr/bin" \
         "$APPDIR/usr/lib/$APP_NAME/qocrd" \
         "$python_root" \
         "$APPDIR/usr/lib/$APP_NAME/screen-ai" \
         "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/icons/hicolor/scalable/apps"

install -m 0755 "$root/target/release/$APP_NAME" "$APPDIR/usr/bin/$APP_NAME"

# The python, with the libraries the engine needs inside it.
cp -r "$python_home/lib" "$python_root/lib"
cp -r "$python_home/bin" "$python_root/bin"
rm -f "$python_root/bin"/pip "$python_root/bin"/pip3 "$python_root/bin"/pip3.* \
      "$python_root/bin"/idle3 "$python_root/bin"/idle3.* \
      "$python_root/bin"/pydoc3 "$python_root/bin"/pydoc3.*
site="$python_root/lib/$stdlib_name/site-packages"
mkdir -p "$site"
cp -r "$work/venv/lib/$stdlib_name/site-packages/." "$site/"

# The engine, as it is written.
cp "$QOCR_SRC/src/qocrd/main.py" \
   "$QOCR_SRC/src/qocrd/screenai.py" \
   "$QOCR_SRC/src/qocrd/chrome_screen_ai_pb2.py" \
   "$APPDIR/usr/lib/$APP_NAME/qocrd/"

# The engine of Chrome, which reads the screen and gives back the text.
cp -r "$SCREEN_AI_DIR/resources" "$APPDIR/usr/lib/$APP_NAME/screen-ai/resources"

# The engine is run by the launcher below, which says where everything is.
cat > "$APPDIR/usr/bin/qocrd" <<LAUNCHER
#!/bin/sh
here="\$(cd "\$(dirname "\$0")" && pwd)"
root="\$here/../lib/$APP_NAME"
export PYTHONPATH="\$root:\$root/python/lib/$stdlib_name/site-packages"
export SCREEN_AI_RESOURCES="\$root/screen-ai/resources"
exec "\$root/python/bin/python3" -m qocrd.main "\$@"
LAUNCHER
chmod 0755 "$APPDIR/usr/bin/qocrd"

# The program is run through the launcher, so that the bundle can be moved
# about and still find everything it carries.
cat > "$APPDIR/AppRun" <<'LAUNCHER'
#!/bin/sh
here="$(cd "$(dirname "$0")" && pwd)"
exec "$here/usr/bin/gnome-ocr" "$@"
LAUNCHER
chmod 0755 "$APPDIR/AppRun"

cat > "$APPDIR/usr/share/applications/$APP_ID.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=GNOME OCR
Comment=Read the text on the screen and copy it
Exec=$APP_NAME
Icon=$APP_NAME
Terminal=false
Categories=Utility;Graphics;TextTools;
Keywords=ocr;scan;text;recognise;
DESKTOP
cp "$APPDIR/usr/share/applications/$APP_ID.desktop" "$APPDIR/$APP_ID.desktop"

cat > "$APPDIR/usr/share/icons/hicolor/scalable/apps/$APP_NAME.svg" <<'ICON'
<svg xmlns="http://www.w3.org/2000/svg" width="128" height="128" viewBox="0 0 128 128">
  <rect x="12" y="16" width="82" height="96" rx="10" fill="#f6f5f4" stroke="#3d3846" stroke-width="4"/>
  <g stroke="#3d3846" stroke-width="6" stroke-linecap="round">
    <path d="M28 40h50M28 58h50M28 76h32"/>
  </g>
  <circle cx="86" cy="84" r="22" fill="none" stroke="#3584e4" stroke-width="7"/>
  <path d="M103 101l13 13" stroke="#3584e4" stroke-width="9" stroke-linecap="round"/>
</svg>
ICON
cp "$APPDIR/usr/share/icons/hicolor/scalable/apps/$APP_NAME.svg" "$APPDIR/$APP_NAME.svg"

# ------------------------------------------------------------------ the file

say "getting the tool that builds the image"
tool="$OUT_DIR/appimagetool-x86_64.AppImage"
if [ ! -f "$tool" ]; then
    curl -fL --progress-bar \
        -o "$tool" \
        https://github.com/AppImage/AppImageKit/releases/download/continuous/appimagetool-x86_64.AppImage
fi
chmod +x "$tool"

say "building the image"
ARCH=x86_64 "$tool" --no-appstream "$APPDIR" "$APPIMAGE"

# ---------------------------------------------------------------- the checks

say "checking the bundle"
[ -x "$APPDIR/usr/bin/$APP_NAME" ] || die "the program is not in the bundle"
[ -x "$APPDIR/usr/bin/qocrd" ] || die "the engine is not in the bundle"
# The program looks for the engine next to itself, so they must be together.
[ "$(dirname "$APPDIR/usr/bin/$APP_NAME")" = "$(dirname "$APPDIR/usr/bin/qocrd")" ] ||
    die "the program and the engine are not side by side"
[ -f "$APPDIR/usr/lib/$APP_NAME/screen-ai/resources/libchromescreenai.so" ] ||
    die "the engine of Chrome is not in the bundle"

# The engine inside the bundle must read a screen. Give it a picture and it
# says how much it found.
if [ -n "${1:-}" ] && [ -f "$1" ]; then
    lines=$(printf 'scanfile true %s\n' "$1" |
        "$APPDIR/usr/bin/qocrd" 2>/dev/null |
        tr '\0' '\n' |
        python3 -c 'import json,sys; print(len(json.load(sys.stdin).get("lines", [])))')
    [ "${lines:-0}" -gt 0 ] || die "the engine inside the bundle read nothing"
    say "the engine inside the bundle read $lines lines"
else
    say "no picture given, so the engine was not read from"
fi

say "done: $APPIMAGE"
du -h "$APPIMAGE"
