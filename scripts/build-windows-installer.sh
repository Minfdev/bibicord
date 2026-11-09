#!/bin/sh
# Builds dist/BibiCord-Setup-<version>.exe: one-file Windows 10/11 installer
# with bibicord.exe and a bundled FFmpeg (no download on first run).
#
# Needs: rustup target x86_64-pc-windows-gnu, mingw-w64, makensis, and for
#   FFmpeg nasm, cmake, pkg-config (macOS: brew install mingw-w64 makensis
#   nasm cmake pkgconf; Linux: apt install mingw-w64 nsis nasm cmake pkg-config).
# Usage: scripts/build-windows-installer.sh [path/to/ffmpeg.exe]
#   Without an argument FFmpeg is built by scripts/build-ffmpeg.sh windows.
#   The argument must be such a build (CI passes it from the ffmpeg job).
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
  scripts/build-ffmpeg.sh windows
  FF=target/pkg/ffmpeg/windows/ffmpeg.exe
fi

rm -rf "$STAGE"
mkdir -p "$STAGE"
cp "target/$TARGET/release/bibicord.exe" "$STAGE/"
cp "$FF" "$STAGE/ffmpeg.exe"
cp packaging/icon/icon.ico "$STAGE/"
scripts/licenses.sh "$STAGE/licenses" "$TARGET"
scripts/ffmpeg-notice.sh "$STAGE/licenses" "$FF" "$VERSION"

OUT="dist/BibiCord-Setup-$VERSION.exe"
ROOT=$(pwd -W 2>/dev/null || pwd)  # Git Bash on Windows needs a native path
makensis -V2 -INPUTCHARSET UTF8 \
  -DVERSION="$VERSION" -DSRC="$ROOT/$STAGE" -DOUTFILE="$ROOT/$OUT" \
  packaging/windows/installer.nsi
ls -lh "$OUT"
