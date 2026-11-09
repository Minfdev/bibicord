#!/bin/sh
# Checks that an FFmpeg build has everything BibiCord uses and that the
# software encoders work (encode + decode a test pattern).
# Usage: scripts/check-ffmpeg.sh <path/to/ffmpeg>
set -eu
FF="$1"
FAIL=0
has() {  # <ffmpeg list option> <names...>
  opt="$1"; shift
  list=$("$FF" -hide_banner "$opt" 2>&1)
  for n in "$@"; do
    if printf '%s\n' "$list" | grep -qw -- "$n"; then echo "ok   $opt $n"
    else echo "BRAK $opt $n"; FAIL=1; fi
  done
}
case "$("$FF" -hide_banner -buildconf 2>&1)" in
  *mingw32*) WIN=1 ;;
  *) WIN=0 ;;
esac

"$FF" -hide_banner -version | head -1
if "$FF" -hide_banner -L | grep -q "either version 3"; then echo "ok   licencja GPL v3"
else echo "BRAK licencja GPL v3"; FAIL=1; fi
has -encoders libx264 libx265 wrapped_avframe rawvideo
has -decoders h264 hevc rawvideo mjpeg
has -demuxers h264 hevc rawvideo
has -muxers h264 hevc null rawvideo
has -filters color testsrc2 format scale null
has -protocols pipe file
if [ "$WIN" = 1 ]; then
  has -encoders h264_nvenc hevc_nvenc h264_amf hevc_amf h264_mf hevc_mf
  has -filters ddagrab hwdownload
  has -devices dshow lavfi
  has -hwaccels d3d11va dxva2 cuda
else
  has -encoders h264_videotoolbox hevc_videotoolbox
  has -devices avfoundation lavfi
  has -hwaccels videotoolbox
fi

TMP=$(mktemp -d)
for enc in libx264 libx265; do
  fmt=h264; [ "$enc" = libx265 ] && fmt=hevc
  if "$FF" -hide_banner -loglevel error -nostdin -f lavfi -i testsrc2=s=640x360:r=30 -frames:v 30 \
       -c:v "$enc" -preset ultrafast -pix_fmt yuv420p -f "$fmt" "$TMP/t.$fmt" &&
     n=$("$FF" -hide_banner -loglevel error -nostdin -hwaccel auto -f "$fmt" -i "$TMP/t.$fmt" \
       -f rawvideo -pix_fmt rgba - | wc -c | tr -d ' ') && [ "$n" -eq $((30 * 640 * 360 * 4)) ]; then
    echo "ok   $enc: kodowanie i dekodowanie 30 klatek"
  else
    echo "BRAK $enc: kodowanie lub dekodowanie nie działa"; FAIL=1
  fi
done
rm -rf "$TMP"
[ "$FAIL" = 0 ] && echo "FFmpeg OK" || { echo "FFmpeg niekompletny" >&2; exit 1; }
