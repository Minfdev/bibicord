#!/bin/sh
# Writes <out>.tar: the complete corresponding source of the bundled FFmpeg
# (GPL): pinned FFmpeg, x264, x265, nv-codec-headers and AMF archives plus
# the scripts that build it. Attached to every release.
# Usage: scripts/ffmpeg-source.sh <out.tar>
set -eu
cd "$(dirname "$0")/.."
. packaging/ffmpeg.env
OUT="$1"
case "$OUT" in /*) ;; *) OUT="$(pwd)/$OUT" ;; esac
STAGE=target/pkg/ffmpeg-source
rm -rf "$STAGE"
mkdir -p "$STAGE/ffmpeg-source/sources" "$STAGE/ffmpeg-source/scripts" "$STAGE/ffmpeg-source/packaging"
for c in $FFMPEG_COMPONENTS; do
  eval "url=\$${c}_URL sum=\$${c}_SHA256"
  file="target/pkg/cache/ffmpeg-src/$(basename "$url")"
  scripts/fetch.sh "$url" "$sum" "$file"
  cp "$file" "$STAGE/ffmpeg-source/sources/"
done
cp scripts/build-ffmpeg.sh scripts/fetch.sh scripts/check-ffmpeg.sh "$STAGE/ffmpeg-source/scripts/"
cp packaging/ffmpeg.env "$STAGE/ffmpeg-source/packaging/"
cat > "$STAGE/ffmpeg-source/README.txt" <<TXT
Kod źródłowy FFmpeg dołączonego do BibiCord (GPL v3)
Source code of the FFmpeg bundled with BibiCord (GPL v3)

sources/    FFmpeg $FFMPEG_VERSION, x264 $X264_VERSION, x265 $X265_VERSION,
            nv-codec-headers $NVCODEC_VERSION, AMF headers $AMF_VERSION
            (niezmienione archiwa / unmodified upstream archives)
scripts/    build-ffmpeg.sh: skrypt budowania / build script
packaging/  ffmpeg.env: wersje i sumy SHA-256 / versions and SHA-256 sums

Budowanie / Build:
  mkdir -p target/pkg/cache/ffmpeg-src && cp sources/* target/pkg/cache/ffmpeg-src/
  scripts/build-ffmpeg.sh windows   # mingw-w64, nasm, cmake, pkg-config
  scripts/build-ffmpeg.sh macos     # Apple Silicon, cmake, pkg-config
TXT
mkdir -p "$(dirname "$OUT")"
tar -cf "$OUT" -C "$STAGE" ffmpeg-source
rm -rf "$STAGE"
ls -l "$OUT"
