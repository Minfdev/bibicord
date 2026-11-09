#!/bin/sh
# Writes the licenses shipped with the installers into <dir>:
# BibiCord (MIT), third-party crates (cargo-about), Inter and Phosphor fonts.
# FFmpeg's license is added by the installer scripts.
# Needs: cargo install cargo-about --locked --features cli
# Usage: scripts/licenses.sh <dir> <target-triple>
set -eu
cd "$(dirname "$0")/.."
DIR="$1"
TARGET="$2"
if ! cargo about --version >/dev/null 2>&1; then
  echo "Brak cargo-about: cargo install cargo-about --locked --features cli" >&2
  exit 1
fi
mkdir -p "$DIR"
cp LICENSE "$DIR/BibiCord-MIT.txt"
cp assets/fonts/Inter-LICENSE.txt "$DIR/Inter-OFL.txt"
cp assets/fonts/Phosphor-LICENSE.txt "$DIR/Phosphor-MIT.txt"
cargo about generate --locked --target "$TARGET" about.hbs -o "$DIR/THIRD-PARTY-LICENSES.html"
