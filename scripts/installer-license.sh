#!/bin/sh
# Writes the license text the installers ask to accept into <out>: a short
# Polish summary above the full MIT license (LICENSE, binding). Windows shows
# it on the NSIS license page, macOS (packaging/macos/dmg_settings.py, as RTF)
# when the DMG is opened.
# UTF-8 with BOM and CRLF line endings, as NSIS's license page expects.
# BINMODE=3: gawk in Git Bash must not translate line endings itself.
# Usage: scripts/installer-license.sh <out>
set -eu
cd "$(dirname "$0")/.."
OUT="$1"
mkdir -p "$(dirname "$OUT")"
{
  printf '\357\273\277'
  printf '%s\n' \
    'BibiCord jest darmowy i dostarczany bez żadnej gwarancji.' \
    'Autorzy nie odpowiadają za szkody wynikające z jego używania.' \
    '' \
    'Powyższe podsumowanie ma charakter informacyjny.' \
    'Wiążący jest poniższy tekst licencji MIT w języku angielskim.' \
    ''
  cat LICENSE
} | tr -d '\r' | awk -v BINMODE=3 '{ printf "%s\r\n", $0 }' > "$OUT"