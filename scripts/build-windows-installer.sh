#!/bin/sh
# Builds dist/BibiCord-Setup-<version>.exe: one-file Windows 10/11 installer
# with bibicord.exe and a bundled FFmpeg (no download on first run).
#
# Needs: rustup target x86_64-pc-windows-gnu, mingw-w64, makensis
#   (macOS: brew install mingw-w64 makensis; Linux: apt install mingw-w64 nsis).
# Usage: scripts/build-windows-installer.sh [path/to/ffmpeg.exe]
#   (own ffmpeg.exe only for local tests: licenses/FFmpeg-SOURCE.txt describes
#   the pinned build from packaging/ffmpeg.env)
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
  # Pinned static GPL build (packaging/ffmpeg.env): ddagrab capture,
  # NVENC / AMF / QSV / MF / x264.
  . packaging/ffmpeg.env
  FF="$CACHE/ffmpeg-$FFMPEG_WIN_VERSION.exe"
  if [ ! -f "$FF" ]; then
    echo "Pobieranie FFmpeg $FFMPEG_WIN_VERSION (BtbN FFmpeg-Builds)…"
    scripts/fetch.sh "$FFMPEG_WIN_URL" "$FFMPEG_WIN_SHA256" "$CACHE/ffmpeg-win-$FFMPEG_WIN_VERSION.zip"
    rm -rf "$CACHE/ffmpeg-win"
    unzip -q -o "$CACHE/ffmpeg-win-$FFMPEG_WIN_VERSION.zip" -d "$CACHE/ffmpeg-win"
    cp "$CACHE"/ffmpeg-win/*/bin/ffmpeg.exe "$FF"
  fi
fi

rm -rf "$STAGE"
mkdir -p "$STAGE"
cp "target/$TARGET/release/bibicord.exe" "$STAGE/"
cp "$FF" "$STAGE/ffmpeg.exe"
cp packaging/icon/icon.ico "$STAGE/"
scripts/licenses.sh "$STAGE/licenses" "$TARGET"
scripts/ffmpeg-notice.sh "$STAGE/licenses" win "$VERSION"

OUT="dist/BibiCord-Setup-$VERSION.exe"
ROOT=$(pwd -W 2>/dev/null || pwd)  # Git Bash on Windows needs a native path
makensis -V2 -INPUTCHARSET UTF8 \
  -DVERSION="$VERSION" -DSRC="$ROOT/$STAGE" -DOUTFILE="$ROOT/$OUT" \
  packaging/windows/installer.nsi
ls -lh "$OUT"
