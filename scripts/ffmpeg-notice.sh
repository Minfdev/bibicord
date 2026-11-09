#!/bin/sh
# Writes the license files of the bundled FFmpeg into <dir>: FFmpeg-GPL.txt,
# FFmpeg-SOURCE.txt and the licenses of its libraries (from build-ffmpeg.sh).
# Usage: scripts/ffmpeg-notice.sh <dir> <path/to/ffmpeg[.exe]> <bibicord-version>
set -eu
cd "$(dirname "$0")/.."
. packaging/ffmpeg.env
DIR="$1"
FF="$2"
FILE=$(basename "$FF")
REL="https://github.com/Minfdev/bibicord/releases/tag/v$3"
TREE="https://github.com/Minfdev/bibicord/tree/v$3"
mkdir -p "$DIR"
cp packaging/licenses/GPL-3.0.txt "$DIR/FFmpeg-GPL.txt"
if [ -d "$(dirname "$FF")/licenses" ]; then
  cp "$(dirname "$FF")"/licenses/* "$DIR/"
fi
cat > "$DIR/FFmpeg-SOURCE.txt" <<TXT
FFmpeg ($FILE) dołączony do BibiCord

Licencja: GNU General Public License v3 (FFmpeg-GPL.txt).
FFmpeg $FFMPEG_VERSION zbudowany przez BibiCord (scripts/build-ffmpeg.sh) z:
- x264 $X264_VERSION, GPL v2 lub nowsza (x264-GPL-2.0.txt),
- x265 $X265_VERSION, GPL v2 lub nowsza (x265-GPL-2.0.txt),
- tylko Windows: nv-codec-headers $NVCODEC_VERSION (nv-codec-headers-MIT.txt)
  i nagłówki AMF $AMF_VERSION (AMF-headers-MIT.txt), licencja MIT.

Kompletny kod źródłowy (FFmpeg, biblioteki, skrypty budowania) jest
dołączony do wydania BibiCord jako ffmpeg-source.tar:
  $REL
Skrypty budowania w repozytorium: $TREE

----

FFmpeg ($FILE) bundled with BibiCord: FFmpeg $FFMPEG_VERSION built by
BibiCord with x264 $X264_VERSION and x265 $X265_VERSION (Windows also
nv-codec-headers $NVCODEC_VERSION and AMF headers $AMF_VERSION), licensed
under the GNU General Public License v3 (FFmpeg-GPL.txt). The complete
corresponding source code, including build scripts, is attached to the
BibiCord release as ffmpeg-source.tar: $REL
TXT
