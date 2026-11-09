#!/bin/sh
# Builds the static FFmpeg bundled with the installers: FFmpeg + x264 + x265
# (GPL v3) from the pinned sources in packaging/ffmpeg.env, with only what
# BibiCord uses enabled explicitly (no autodetected system libraries).
#
#   windows: cross-compiled with mingw-w64 (Linux or macOS host); ddagrab,
#            dshow, D3D11VA/DXVA2, NVENC/NVDEC, AMF, Media Foundation.
#   macos:   native on Apple Silicon (macOS 14+); AVFoundation, VideoToolbox.
#
# Needs: curl, make, cmake, pkg-config, nasm (windows), mingw-w64 (windows).
# Output: target/pkg/ffmpeg/<os>/ffmpeg[.exe] and licenses/; skipped when
# the output matches the current packaging/ffmpeg.env and this script.
# Usage: scripts/build-ffmpeg.sh <windows|macos>
set -eu
cd "$(dirname "$0")/.."
ROOT=$(pwd)
OS="$1"
. packaging/ffmpeg.env

OUT="$ROOT/target/pkg/ffmpeg/$OS"
STAMP=$(cat packaging/ffmpeg.env scripts/build-ffmpeg.sh | cksum | cut -d' ' -f1)
if [ -f "$OUT/.stamp" ] && [ "$(cat "$OUT/.stamp")" = "$STAMP" ]; then
  echo "FFmpeg ($OS) aktualny: $OUT"
  exit 0
fi

WORK="$ROOT/target/pkg/ffmpeg-work/$OS"
SRC="$ROOT/target/pkg/cache/ffmpeg-src"
PREFIX="$WORK/prefix"
rm -rf "$WORK"
mkdir -p "$WORK" "$PREFIX/include" "$PREFIX/lib/pkgconfig"
JOBS=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)

unpack() {  # <COMPONENT>: fetch, verify and unpack; prints the source dir
  eval "url=\$${1}_URL sum=\$${1}_SHA256"
  file="$SRC/$(basename "$url")"
  scripts/fetch.sh "$url" "$sum" "$file" >&2
  dir=$(tar -tf "$file" | head -1 | cut -d/ -f1)
  tar -xf "$file" -C "$WORK"
  echo "$WORK/$dir"
}

export PKG_CONFIG_LIBDIR="$PREFIX/lib/pkgconfig"
export PKG_CONFIG_PATH=""
case "$OS" in
  windows)
    HOST=x86_64-w64-mingw32
    X264_FLAGS="--host=$HOST --cross-prefix=$HOST-"
    X265_FLAGS="-DCMAKE_SYSTEM_NAME=Windows -DCMAKE_SYSTEM_PROCESSOR=x86_64
      -DCMAKE_C_COMPILER=$HOST-gcc -DCMAKE_CXX_COMPILER=$HOST-g++
      -DCMAKE_RC_COMPILER=$HOST-windres -DCMAKE_ASM_NASM_COMPILER=nasm"
    FF_FLAGS="--target-os=mingw32 --arch=x86_64 --cross-prefix=$HOST-
      --extra-ldflags=-static --enable-w32threads
      --enable-d3d11va --enable-dxva2 --enable-mediafoundation
      --enable-ffnvcodec --enable-cuda --enable-nvenc --enable-nvdec --enable-amf"
    EXE=ffmpeg.exe
    ;;
  macos)
    export MACOSX_DEPLOYMENT_TARGET=14.0
    X264_FLAGS=""
    X265_FLAGS="-DCMAKE_OSX_DEPLOYMENT_TARGET=14.0"
    FF_FLAGS="--enable-pthreads --enable-zlib
      --enable-videotoolbox --enable-audiotoolbox --enable-avfoundation"
    EXE=ffmpeg
    ;;
  *) echo "Użycie: scripts/build-ffmpeg.sh <windows|macos>" >&2; exit 1 ;;
esac

echo "== x264 $X264_VERSION"
D=$(unpack X264)
(cd "$D" && ./configure --prefix="$PREFIX" $X264_FLAGS --enable-static --disable-cli \
  --disable-opencl --disable-lavf --disable-swscale --disable-ffms --disable-gpac --disable-lsmash \
  && make -j"$JOBS" && make install-lib-static)

echo "== x265 $X265_VERSION"
D=$(unpack X265)
# shellcheck disable=SC2086
cmake -S "$D/source" -B "$WORK/x265-build" -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_POLICY_VERSION_MINIMUM=3.5 -DCMAKE_INSTALL_PREFIX="$PREFIX" \
  -DENABLE_SHARED=OFF -DENABLE_CLI=OFF $X265_FLAGS
make -C "$WORK/x265-build" -j"$JOBS" install
if [ "$OS" = windows ]; then
  # x265.pc lists -lgcc_s, which clashes with libgcc_eh under -static.
  sed -i.bak 's/^Libs.private:.*/Libs.private: -lstdc++/' "$PREFIX/lib/pkgconfig/x265.pc"
fi

if [ "$OS" = windows ]; then
  echo "== nv-codec-headers $NVCODEC_VERSION, AMF $AMF_VERSION"
  D=$(unpack NVCODEC)
  make -C "$D" PREFIX="$PREFIX" install
  D=$(unpack AMF)
  cp -R "$D/AMF" "$PREFIX/include/"
fi

echo "== FFmpeg $FFMPEG_VERSION"
D=$(unpack FFMPEG)
# shellcheck disable=SC2086
(cd "$D" && ./configure --prefix="$PREFIX" --pkg-config=pkg-config --pkg-config-flags=--static \
  --extra-cflags="-I$PREFIX/include" --extra-ldflags="-L$PREFIX/lib" \
  --extra-version=bibicord --enable-gpl --enable-version3 \
  --disable-autodetect --disable-doc --disable-debug --disable-network \
  --disable-ffplay --disable-ffprobe --enable-libx264 --enable-libx265 $FF_FLAGS \
  && make -j"$JOBS" && make install)

rm -rf "$OUT"
mkdir -p "$OUT/licenses"
cp "$PREFIX/bin/$EXE" "$OUT/$EXE"
cp "$WORK"/x264-*/COPYING "$OUT/licenses/x264-GPL-2.0.txt"
cp "$WORK/x265_$X265_VERSION/COPYING" "$OUT/licenses/x265-GPL-2.0.txt"
if [ "$OS" = windows ]; then
  sed '/\*\//q' "$PREFIX/include/ffnvcodec/nvEncodeAPI.h" > "$OUT/licenses/nv-codec-headers-MIT.txt"
  awk '/^\/\//{print;next}{exit}' "$PREFIX/include/AMF/core/Factory.h" > "$OUT/licenses/AMF-headers-MIT.txt"
fi
echo "$STAMP" > "$OUT/.stamp"
rm -rf "$WORK"
echo "OK: $OUT/$EXE"
