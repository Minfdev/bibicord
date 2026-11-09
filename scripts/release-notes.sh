#!/bin/sh
# Prints release notes for version $1: docs/releases/<version>.md
# followed by the download/install section.
set -eu
cd "$(dirname "$0")/.."
V="$1"
if [ -f "docs/releases/$V.md" ]; then
  cat "docs/releases/$V.md"
  echo
fi
cat <<'MD'
## Pobieranie

| System | Plik |
|---|---|
| Windows 10 / 11 (x64) | [BibiCord-Setup.exe](https://github.com/Minfdev/bibicord/releases/latest/download/BibiCord-Setup.exe) |
| macOS 14+ (Apple Silicon) | [BibiCord.dmg](https://github.com/Minfdev/bibicord/releases/latest/download/BibiCord.dmg) |

Masz już BibiCord? Aplikacja sama zaproponuje aktualizację.

**Pierwsza instalacja** (aplikacja nie jest podpisana certyfikatem wydawcy):
- Windows: SmartScreen → „Więcej informacji” → „Uruchom mimo to”.
- macOS: po pierwszej próbie otwarcia Ustawienia systemowe → Prywatność i ochrona → „Otwórz mimo to”.

Wszyscy w pokoju muszą mieć tę samą wersję.

Instalatory zawierają FFmpeg na licencji GPL v3. Kompletny kod źródłowy
(FFmpeg, x264, x265, skrypty budowania) jest w pliku `ffmpeg-source.tar`.
MD
