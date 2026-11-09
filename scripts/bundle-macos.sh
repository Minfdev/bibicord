#!/bin/sh
# Builds dist/BibiCord.app (Apple Silicon release binary + bundled FFmpeg).
# Usage: scripts/bundle-macos.sh [path/to/ffmpeg]
#   Without an argument FFmpeg is built by scripts/build-ffmpeg.sh macos
#   (needs cmake, pkg-config). The argument must be such a build.
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
  scripts/build-ffmpeg.sh macos
  FF=target/pkg/ffmpeg/macos/ffmpeg
fi
cp "$FF" "$APP/Contents/Resources/ffmpeg"
chmod +x "$APP/Contents/Resources/ffmpeg"
scripts/licenses.sh "$APP/Contents/Resources/licenses" aarch64-apple-darwin
scripts/ffmpeg-notice.sh "$APP/Contents/Resources/licenses" "$FF" "$VERSION"
# Ad-hoc signature (no Apple Developer ID): Gatekeeper asks once on first launch.
codesign --force --deep --sign - "$APP"
echo "OK: $APP"
