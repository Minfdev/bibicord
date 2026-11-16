#!/bin/sh
# Builds dist/BibiCord-<version>.dmg: one-file macOS installer (Apple Silicon,
# macOS 14+). Opening it shows the MIT license agreement (text from
# scripts/installer-license.sh); after "Zgadzam się" drag BibiCord into
# Applications.
# Usage: scripts/build-macos-dmg.sh [path/to/ffmpeg]
set -eu
cd "$(dirname "$0")/.."
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
scripts/bundle-macos.sh "$@"

# dmgbuild lays out the Finder window without scripting Finder.
# Prefer an installed one, else uv, else a venv. dmg_settings.py extends
# dmgbuild's license languages, so only a tested version is used.
DMGBUILD_VERSION=2.0.0
DMGBUILD=$(command -v dmgbuild || true)
if [ -n "$DMGBUILD" ] && ! "$DMGBUILD" --version 2>/dev/null | grep -qF "dmgbuild $DMGBUILD_VERSION"; then
  DMGBUILD=""
fi
if [ -z "$DMGBUILD" ] && command -v uvx >/dev/null 2>&1; then
  DMGBUILD="uvx --python 3.12 dmgbuild==$DMGBUILD_VERSION"
fi
if [ -z "$DMGBUILD" ]; then
  VENV=target/pkg/venv
  if ! "$VENV/bin/dmgbuild" --version 2>/dev/null | grep -qF "dmgbuild $DMGBUILD_VERSION"; then
    rm -rf "$VENV"
    PY=$(command -v python3.12 || command -v python3.13 || command -v python3)
    "$PY" -m venv "$VENV"
    "$VENV/bin/pip" -q install "dmgbuild==$DMGBUILD_VERSION"
  fi
  DMGBUILD="$VENV/bin/dmgbuild"
fi
OUT="dist/BibiCord-$VERSION.dmg"
LICENSE_TXT=target/pkg/macos/license.txt
scripts/installer-license.sh "$LICENSE_TXT"
rm -f "$OUT"
$DMGBUILD \
  -s packaging/macos/dmg_settings.py \
  -D app="$(pwd)/dist/BibiCord.app" \
  -D background="$(pwd)/packaging/macos/dmg-background.png" \
  -D license="$(pwd)/$LICENSE_TXT" \
  -D volicon="$(pwd)/packaging/icon/BibiCord.icns" \
  "BibiCord" "$OUT"
hdiutil verify -quiet "$OUT"
ls -lh "$OUT"
