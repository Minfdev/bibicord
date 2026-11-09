#!/bin/sh
# Builds dist/BibiCord.app (Apple Silicon release binary + bundled FFmpeg).
# Usage: scripts/bundle-macos.sh [path/to/ffmpeg]
set -eu
cd "$(dirname "$0")/.."
cargo build --release
APP=dist/BibiCord.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/bibicord "$APP/Contents/MacOS/bibicord"
cp packaging/macos/Info.plist "$APP/Contents/Info.plist"
cp packaging/icon/BibiCord.icns "$APP/Contents/Resources/BibiCord.icns"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $VERSION" "$APP/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $VERSION" "$APP/Contents/Info.plist"

FF="${1:-}"
if [ -z "$FF" ]; then
  # Apple Silicon only (Intel Macs are not supported).
  CACHE=target/pkg/cache
  FF="$CACHE/ffmpeg-macos-arm64"
  if [ ! -f "$FF" ]; then
    mkdir -p "$CACHE"
    curl -fL --retry 3 -o "$CACHE/ffmpeg-mac.zip" https://www.osxexperts.net/ffmpeg9arm.zip
    unzip -q -o "$CACHE/ffmpeg-mac.zip" -d "$CACHE/ffmpeg-mac"
    cp "$CACHE/ffmpeg-mac/ffmpeg" "$FF"
  fi
fi
cp "$FF" "$APP/Contents/Resources/ffmpeg"
chmod +x "$APP/Contents/Resources/ffmpeg"
scripts/licenses.sh "$APP/Contents/Resources/licenses" aarch64-apple-darwin
echo "FFmpeg: GPL v3, https://ffmpeg.org/legal.html" > "$APP/Contents/Resources/licenses/FFmpeg-GPL.txt"
# Ad-hoc signature (no Apple Developer ID): Gatekeeper asks once on first launch.
codesign --force --deep --sign - "$APP"
echo "OK: $APP"
