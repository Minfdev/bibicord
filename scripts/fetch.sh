#!/bin/sh
# Downloads <url> to <out> and checks its SHA-256 (skipped when empty).
# Usage: scripts/fetch.sh <url> <sha256|""> <out>
set -eu
URL="$1"
SUM="$2"
OUT="$3"
if [ -f "$OUT" ] && [ -z "$SUM" ]; then
  exit 0
fi
sha() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1
}
if [ ! -f "$OUT" ] || [ "$(sha "$OUT")" != "$SUM" ]; then
  mkdir -p "$(dirname "$OUT")"
  curl -fsSL --retry 3 -o "$OUT.part" "$URL"
  mv "$OUT.part" "$OUT"
fi
if [ -n "$SUM" ] && [ "$(sha "$OUT")" != "$SUM" ]; then
  echo "Zła suma SHA-256: $OUT ($URL)" >&2
  rm -f "$OUT"
  exit 1
fi
