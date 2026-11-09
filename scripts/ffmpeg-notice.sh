#!/bin/sh
# Writes FFmpeg-GPL.txt and FFmpeg-SOURCE.txt for the bundled FFmpeg into <dir>.
# Usage: scripts/ffmpeg-notice.sh <dir> <win|mac> <bibicord-version>
set -eu
cd "$(dirname "$0")/.."
. packaging/ffmpeg.env
DIR="$1"
case "$2" in
  win) FILE=ffmpeg.exe; NAME=ffmpeg-source-windows.tar.gz
       VER=$FFMPEG_WIN_VERSION; BUILD=$FFMPEG_WIN_BUILD; SRC=$FFMPEG_WIN_SOURCE_URL ;;
  mac) FILE=ffmpeg; NAME=ffmpeg-source-macos.tar.xz
       VER=$FFMPEG_MAC_VERSION; BUILD=$FFMPEG_MAC_BUILD; SRC=$FFMPEG_MAC_SOURCE_URL ;;
  *) echo "Nieznany system: $2" >&2; exit 1 ;;
esac
REL="https://github.com/Minfdev/bibicord/releases/tag/v$3"
mkdir -p "$DIR"
cp packaging/licenses/GPL-3.0.txt "$DIR/FFmpeg-GPL.txt"
cat > "$DIR/FFmpeg-SOURCE.txt" <<TXT
FFmpeg ($FILE) dołączony do BibiCord

Licencja: GNU General Public License v3 (FFmpeg-GPL.txt).
Wersja: $VER

Kod źródłowy tej wersji FFmpeg jest dołączony do wydania BibiCord
jako $NAME:
  $REL
Ten sam kod: $SRC
Skrypty budowania i wersje wbudowanych bibliotek: $BUILD

----

FFmpeg ($FILE) bundled with BibiCord is licensed under the GNU General
Public License v3 (FFmpeg-GPL.txt). Version: $VER.
The corresponding source code is attached to the BibiCord release as
$NAME ($REL), also available at $SRC.
Build scripts and library versions: $BUILD
TXT
