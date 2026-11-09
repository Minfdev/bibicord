#!/bin/sh
# Builds dist/BibiCord-Setup-<version>.exe: one-file Windows 10/11 installer
# with bibicord.exe and a bundled FFmpeg (no download on first run).
#
# Needs: rustup target x86_64-pc-windows-gnu, mingw-w64, makensis
#   (macOS: brew install mingw-w64 makensis; Linux: apt install mingw-w64 nsis).
# Usage: scripts/build-windows-installer.sh [path/to/ffmpeg.exe]
set -eu
cd "$(dirname "$0")/.."
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
TARGET=${TARGET:-x86_64-pc-windows-gnu}  # CI on Windows: x86_64-pc-windows-msvc
STAGE=target/pkg/windows
CACHE=target/pkg/cache
mkdir -p "$CACHE" dist

cargo build --release --target "$TARGET"

FF="${1:-}"
if [ -z "$FF" ]; then
  FF="$CACHE/ffmpeg.exe"
  if [ ! -f "$FF" ]; then
    # Static GPL build: ddagrab capture, NVENC / AMF / QSV / MF / x264.
    echo "Pobieranie FFmpeg (BtbN FFmpeg-Builds)…"
    curl -fL --retry 3 -o "$CACHE/ffmpeg-win.zip" \
      https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-n9.0-latest-win64-gpl-9.0.zip
    rm -rf "$CACHE/ffmpeg-win"
    unzip -q -o "$CACHE/ffmpeg-win.zip" -d "$CACHE/ffmpeg-win"
    cp "$CACHE"/ffmpeg-win/*/bin/ffmpeg.exe "$FF"
    cp "$CACHE"/ffmpeg-win/*/LICENSE.txt "$CACHE/FFmpeg-LICENSE.txt" 2>/dev/null || true
  fi
fi

rm -rf "$STAGE"
mkdir -p "$STAGE"
cp "target/$TARGET/release/bibicord.exe" "$STAGE/"
cp "$FF" "$STAGE/ffmpeg.exe"
cp packaging/icon/icon.ico "$STAGE/"
scripts/licenses.sh "$STAGE/licenses" "$TARGET"
if [ -f "$CACHE/FFmpeg-LICENSE.txt" ]; then
  cp "$CACHE/FFmpeg-LICENSE.txt" "$STAGE/licenses/FFmpeg-GPL.txt"
else
  echo "FFmpeg: GPL v3, https://ffmpeg.org/legal.html" > "$STAGE/licenses/FFmpeg-GPL.txt"
fi

OUT="dist/BibiCord-Setup-$VERSION.exe"
ROOT=$(pwd -W 2>/dev/null || pwd)  # Git Bash on Windows needs a native path
makensis -V2 -INPUTCHARSET UTF8 \
  -DVERSION="$VERSION" -DSRC="$ROOT/$STAGE" -DOUTFILE="$ROOT/$OUT" \
  packaging/windows/installer.nsi
ls -lh "$OUT"
