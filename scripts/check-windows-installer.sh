#!/bin/sh
# Smoke test of the Windows installer without a release build: packs dummy
# bibicord.exe/ffmpeg.exe with the real NSIS script and license text, checks
# the license page text, installs silently (as the in-app update does) into
# a scratch folder and uninstalls again.
#
# Needs Git Bash on Windows and makensis (PATH, $MAKENSIS or the default NSIS
# folder). Uses the real per-user install side effects (HKCU keys, Start menu
# shortcut) and closes any running bibicord.exe: run it in CI or on a machine
# without BibiCord installed.
# Usage: scripts/check-windows-installer.sh
set -eu
cd "$(dirname "$0")/.."
# Git Bash would turn "/S" into a path ("S:/").
export MSYS2_ARG_CONV_EXCL='*'
STAGE=target/pkg/check-installer
INST=target/pkg/check-installer-dir
fail() { echo "BŁĄD: $*" >&2; exit 1; }

MAKENSIS=${MAKENSIS:-$(command -v makensis || true)}
[ -n "$MAKENSIS" ] || MAKENSIS="/c/Program Files (x86)/NSIS/makensis.exe"
[ -x "$MAKENSIS" ] || fail "brak makensis (ustaw MAKENSIS)"

rm -rf "$STAGE" "$INST"
mkdir -p "$STAGE/licenses"
echo dummy > "$STAGE/bibicord.exe"
echo dummy > "$STAGE/ffmpeg.exe"
cp packaging/icon/icon.ico "$STAGE/"
cp LICENSE "$STAGE/licenses/BibiCord-MIT.txt"
scripts/installer-license.sh "$STAGE/license.txt"

# ---- license page text
L="$STAGE/license.txt"
[ "$(head -c 3 "$L" | od -An -tx1 | tr -d ' \n')" = efbbbf ] || fail "license.txt bez BOM UTF-8"
# Byte-level check: grep in Git Bash does not see the CR before LF, so
# count bytes instead. Every LF must follow a CR and no other CR may remain.
lines=$(tr -cd '\n' < "$L" | wc -c | tr -d ' ')
crs=$(tr -cd '\r' < "$L" | wc -c | tr -d ' ')
rest=$(od -An -v -tx1 "$L" | tr -s ' \n' '  ' | sed 's/ 0d 0a//g')
case "$rest" in *' 0d'* | *' 0a'*) bare=1 ;; *) bare=0 ;; esac
[ "$lines" -gt 0 ] && [ "$crs" -eq "$lines" ] && [ "$bare" -eq 0 ] ||
  fail "license.txt: $lines wierszy, $crs znaków CR, nie wszystkie końce to CRLF"
grep -q 'bez żadnej gwarancji' "$L" || fail "brak polskiego podsumowania"
n=$(tr -d '\r' < LICENSE | wc -l)
tail -n "$n" "$L" | tr -d '\r' > "$STAGE/license-tail.txt"
tr -d '\r' < LICENSE | cmp -s - "$STAGE/license-tail.txt" || fail "końcówka license.txt różni się od LICENSE"

# ---- build
OUT="$STAGE/BibiCord-check.exe"
"$MAKENSIS" -V2 -INPUTCHARSET UTF8 \
  -DVERSION=0.0.0 -DSRC="$(cygpath -w "$(pwd)/$STAGE")" -DOUTFILE="$(cygpath -w "$(pwd)/$OUT")" \
  packaging/windows/installer.nsi

# ---- silent install (in-app update path): no page may wait for input.
# /D= must be last and unquoted, so the path must not contain spaces.
mkdir -p "$INST"
DIR=$(cygpath -w "$(pwd)/$INST")
case "$DIR" in *' '*) fail "ścieżka testowa ze spacją: $DIR" ;; esac
timeout 120 "$OUT" /S "/D=$DIR" || fail "cicha instalacja zakończyła się kodem $?"
[ -f "$INST/bibicord.exe" ] || fail "brak bibicord.exe po instalacji"
[ -f "$INST/licenses/BibiCord-MIT.txt" ] || fail "licencja nie została zainstalowana"
cmp -s LICENSE "$INST/licenses/BibiCord-MIT.txt" || fail "zainstalowana licencja różni się od LICENSE"
[ ! -e "$INST/license.txt" ] || fail "license.txt nie powinien być instalowany"
KEY='HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\BibiCord'
reg query "$KEY" >/dev/null 2>&1 || fail "brak wpisu w Aplikacje i funkcje"

# ---- silent uninstall; _?= keeps it in place so we can wait for it.
timeout 120 "$INST/Odinstaluj.exe" /S "_?=$DIR" || fail "cicha deinstalacja zakończyła się kodem $?"
[ ! -e "$INST/bibicord.exe" ] || fail "deinstalator nie usunął plików"
! reg query "$KEY" >/dev/null 2>&1 || fail "deinstalator nie usunął wpisu z rejestru"
rm -rf "$INST"
echo "OK: instalator Windows (strona licencji, /S, deinstalacja)"