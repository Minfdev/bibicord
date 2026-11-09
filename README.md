# BibiCord

Minimalistyczny komunikator dla znajomych w Ruście, inspirowany Discordem,
**bez serwera**: czat tekstowy, głos i udostępnianie ekranu w jakości
źródła. Działa na Windows 10, Windows 11 i macOS 14+.

## Pobierz

| System | Instalator |
|---|---|
| Windows 10 / 11 (x64) | [**BibiCord-Setup.exe**](https://github.com/Minfdev/bibicord/releases/latest/download/BibiCord-Setup.exe) |
| macOS 14+ (Apple Silicon) | [**BibiCord.dmg**](https://github.com/Minfdev/bibicord/releases/latest/download/BibiCord.dmg) |

Linki zawsze wskazują najnowszą wersję. Zainstalowana aplikacja sama
proponuje aktualizację. Wszystkie wersje: [Releases](https://github.com/Minfdev/bibicord/releases).

![Oglądanie streamu](docs/screenshots/4-watch.png)

## Interfejs

Ciemny, zwarty układ znany z komunikatorów dla graczy: lista kanałów i uczestników po
lewej, czat albo scena głosowa w środku, panel połączenia i własny profil
(mikrofon, słuchawki, ustawienia) na dole paska.

* **Design system** – zasady w [`DESIGN.md`](DESIGN.md), tokeny w
  `src/ui/theme.rs` (kolory powierzchni, tekstu i stanów, skala typografii,
  odstępy w siatce 4 px, promienie, cienie, czas animacji 120 ms).
* **Typografia** – Inter (Regular/Medium/SemiBold/Bold) wbudowany w binarkę.
* **Ikony** – Phosphor (warianty regular i fill).
* **Komponenty** – `src/ui/widgets.rs`: przyciski, przyciski okrągłe, chipy,
  pola tekstowe, awatary z pierścieniem mówienia, odznaki NA ŻYWO, wiersze
  listy, menu, modale.
* **Ekrany** – start, czat (wiadomości grupowane), scena głosowa z kafelkami
  16:9, oglądanie streamu z paskiem uczestników i czatem obok, okna
  „Udostępnij ekran”, „Zaproś” i „Ustawienia głosu”.

| Czat | Kanał głosowy | Udostępnianie ekranu |
|---|---|---|
| ![](docs/screenshots/2-chat.png) | ![](docs/screenshots/3-voice.png) | ![](docs/screenshots/5-dialog-golive.png) |

## Jak to działa

- **Sieć: P2P.** Każdy uczestnik łączy się z każdym bezpośrednio
  ([iroh](https://iroh.computer), QUIC, szyfrowanie end-to-end, przebijanie NAT).
  Nie ma własnego serwera. Publiczne relaye n0 służą tylko do zestawienia
  połączenia, a gdy bezpośrednie połączenie jest niemożliwe, przenoszą
  zaszyfrowany ruch. Przy każdej osobie widać, czy łączysz się `P2P`, czy przez `relay`.
- **Pokój = zaproszenie.** Tworzysz pokój, klikasz „zaproś”, a tekst
  zaproszenia wysyłasz znajomym. Kto dołączy, sam połączy się z pozostałymi.
- **Stream w jakości źródła.** Ekran jest przechwytywany natywnie
  (Windows: Desktop Duplication; macOS: ScreenCaptureKit), kodowany
  **sprzętowo jeden raz** (NVENC / AMF / QuickSync / Media Foundation /
  VideoToolbox) w natywnej rozdzielczości i FPS (do 120 fps, do 120 Mb/s).
  Zakodowany strumień trafia do widzów bajt w bajt, **bez transkodowania**.
  Dekodowanie u widza też jest sprzętowe.
  Nowy widz od razu dostaje ostatnią klatkę kluczową. Widz, któremu nie
  wystarcza łącze, jest przewijany do kolejnej klatki kluczowej, zamiast
  zbierać opóźnienie.
- **Kamera.** Działa jak na Discordzie: przycisk „Kamera” na kanale głosowym
  (obok „Ekran”) pokazuje obraz z kamerki w Twoim kafelku u wszystkich na
  głosowym, bez klikania „Obejrzyj”. Ty widzisz podgląd (lustrzany). Kamera jest
  niezależna od udostępniania ekranu (oba naraz), 720p30, kodowana sprzętowo,
  wyłącza się po wyjściu z głosowego. Kamerę wybiera się w „Ustawieniach głosu”.
- **Głos.** Opus 48 kHz z FEC, wysyłany w datagramach QUIC (bez retransmisji),
  z buforem jitter i miksowaniem.
  Mikrofon przechodzi przez redukcję szumów (RNNoise, wyłączalna), a czułość
  wykrywania mowy jest automatyczna albo ustawiana ręcznie jako próg w dBFS
  (z podglądem poziomu). Ustawienia dźwięku są w „Ustawieniach głosu” i na
  stronie profilu, także przed dołączeniem do pokoju.
- **Dźwięki.** Krótkie sygnały przy wejściu i wyjściu z rozmowy (Twoim
  i znajomych), wyciszeniu mikrofonu i słuchawek, starcie i końcu streamu oraz
  nowej wiadomości. Są syntezowane w kodzie (`src/sounds.rs`), nie pochodzą
  z Discorda. Głośność i wyłączanie: „Ustawienia głosu”.
- **Profil.** Nazwa i zdjęcie profilowe (JPG, PNG lub WebP, także
  przeciągnięte do okna). Zdjęcie jest przycinane do kwadratu, zmniejszane do
  160 px i zapisywane lokalnie; inni dostają je przy połączeniu, a zmiany
  widzą od razu. Otwierasz go kliknięciem w swój awatar.

> Bez serwera każdy widz pobiera pełny bitrate od nadawcy. Przy 30 Mb/s
> i 3 widzach nadawca potrzebuje około 90 Mb/s wysyłania.

## Uruchomienie

```sh
cargo run --release
```

Wymagania do budowania: Rust (stable), CMake (dla Opusa), a na macOS
Xcode Command Line Tools. FFmpeg jest szukany obok pliku wykonywalnego,
w `PATH` albo (przy pierwszym uruchomieniu) pobierany automatycznie.
Można też podać własny: `BIBICORD_FFMPEG=/ścieżka/ffmpeg`.

## Instalatory

Każdy system instaluje się jednym plikiem z wbudowanym FFmpeg (nic nie jest
pobierane przy pierwszym uruchomieniu):

| System | Plik | Jak zbudować |
|---|---|---|
| Windows 10 / 11 (x64) | `dist/BibiCord-Setup-<wersja>.exe` | `scripts/build-windows-installer.sh` |
| macOS 14+ (Apple Silicon) | `dist/BibiCord-<wersja>.dmg` | `scripts/build-macos-dmg.sh` |

* **Windows** – instalator NSIS po polsku, instalacja dla bieżącego
  użytkownika do `%LOCALAPPDATA%\Programs\BibiCord` (bez pytania o
  uprawnienia administratora), skrót w menu Start, opcjonalnie na pulpicie,
  deinstalacja z „Aplikacje i funkcje”. Ponowne uruchomienie instalatora
  aktualizuje aplikację. Na macOS budowany krzyżowo (`brew install mingw-w64
  makensis nasm cmake pkgconf`, `rustup target add x86_64-pc-windows-gnu`).
* **macOS** – obraz DMG: otwórz i przeciągnij BibiCord do folderu Aplikacje.
  FFmpeg buduje się ze źródeł (`brew install cmake pkgconf`).

Oba skrypty wymagają `cargo-about` (`cargo install cargo-about --locked
--features cli`): generuje licencje bibliotek do folderu `licenses`
w instalatorze (`scripts/licenses.sh`).

Workflow `.github/workflows/release.yml` buduje oba instalatory przy każdym
pushu; tag `v*` publikuje je jako release na GitHubie (`BibiCord-Setup.exe`,
`BibiCord.dmg`, `SHA256SUMS.txt`, `ffmpeg-source.tar`).

### Wydanie nowej wersji

1. Podnieś `version` w `Cargo.toml`.
2. Opisz zmiany w `docs/releases/<wersja>.md` (pojawią się na stronie
   release i w oknie aktualizacji w aplikacji).
3. `git tag v<wersja> && git push origin main v<wersja>`.

### Aktualizacje w aplikacji

Przy starcie i co 6 godzin aplikacja pyta GitHub API o najnowszy release.
Gdy jest nowszy, w pasku bocznym (i na ekranie startowym) pojawia się baner.
Kliknięcie pobiera instalator, sprawdza jego sumę SHA-256 z
`SHA256SUMS.txt` i instaluje:

* Windows: `BibiCord-Setup.exe /S /RESTART` – cicha instalacja i restart.
* macOS: nowa paczka `.app` z DMG zastępuje starą, aplikacja startuje ponownie.

`BIBICORD_NO_UPDATE=1` wyłącza sprawdzanie.

**Brak podpisu wydawcy.** Instalatory nie są podpisane certyfikatem
(Windows Authenticode / Apple Developer ID), więc przy pierwszym uruchomieniu:

* Windows SmartScreen: „Więcej informacji” → „Uruchom mimo to”.
* macOS: po pierwszej próbie otwarcia Ustawienia systemowe → Prywatność
  i ochrona → „Otwórz mimo to”.

Na macOS przy pierwszym streamie trzeba zezwolić na **Nagrywanie ekranu**,
a przy głosie na **Mikrofon** (Ustawienia systemowe → Prywatność i ochrona).

## Testy

```sh
cargo test                       # testy jednostkowe
cargo test -- --ignored          # end-to-end: 3 węzły przez prawdziwą sieć, czat + stream 1080p60
```

Zmienne pomocnicze: `BIBICORD_TEST_SOURCE=1` streamuje obraz testowy zamiast
ekranu, `BIBICORD_DEBUG=1` włącza szczegółowe logi, `BIBICORD_FAKE_AUDIO=1` uruchamia
głos bez urządzeń audio (`BIBICORD_FAKE_TONE=1` dodaje sygnał testowy).
`BIBICORD_NO_SOUNDS=1` wyłącza dźwięki. `cargo test play_all -- --ignored` odtwarza
wszystkie dźwięki, a `BIBICORD_SOUNDS_OUT=<katalog> cargo test export_wavs -- --ignored`
zapisuje je jako WAV.
Skryptowane uruchomienia do zrzutów ekranu opisuje nagłówek `src/demo.rs`.

## Struktura

| Plik | Rola |
|---|---|
| `src/net.rs` | pokój P2P (siatka połączeń iroh), czat, sygnalizacja, przesyłanie wideo i głosu |
| `src/proto.rs` | protokół i zaproszenia |
| `src/video/encode.rs` | przechwytywanie ekranu i wybór enkodera sprzętowego |
| `src/video/hub.rs` | rozsyłanie strumienia do widzów (klatki kluczowe, wolni widzowie) |
| `src/video/decode.rs` | dekodowanie sprzętowe do RGBA |
| `src/voice.rs` | mikrofon/głośniki, Opus, bufor jitter, wykrywanie mówienia |
| `src/sounds.rs` | dźwięki interfejsu (synteza, odtwarzanie, ustawienia) |
| `src/ui/theme.rs` | tokeny design systemu i instalacja fontów |
| `src/ui/widgets.rs` | komponenty |
| `src/ui/{start,sidebar,chat,stage,dialogs}.rs` | ekrany i okna |
| `src/demo.rs` | skryptowane uruchomienia (testy, zrzuty ekranu) |

## Licencje

BibiCord jest udostępniany na licencji MIT (`LICENSE`).

Font Inter: SIL Open Font License 1.1 (`assets/fonts/Inter-LICENSE.txt`).
Ikony Phosphor: MIT (`assets/fonts/Phosphor-LICENSE.txt`).

Instalatory zawierają folder `licenses` z licencją BibiCord, fontów, FFmpeg
oraz `THIRD-PARTY-LICENSES.html` z licencjami wszystkich bibliotek Rusta,
generowanym przez `cargo-about` według `about.toml`. Biblioteka z licencją
spoza listy `accepted` przerywa budowanie instalatora.

### FFmpeg

Instalatory dołączają FFmpeg zbudowany przez BibiCord ze źródeł
(`scripts/build-ffmpeg.sh`, licencja GPL v3): FFmpeg z x264 i x265, a na
Windows także z nagłówkami NVENC/NVDEC i AMF. Wersje i sumy SHA-256
wszystkich źródeł są przypięte w `packaging/ffmpeg.env`. Włączone jest
tylko to, czego używa BibiCord, bez bibliotek systemowych wykrywanych
automatycznie:

* Windows: budowany krzyżowo (mingw-w64), `ddagrab`, `dshow`,
  D3D11VA/DXVA2, NVENC, AMF, Media Foundation,
* macOS: Apple Silicon, macOS 14+, AVFoundation, VideoToolbox.

`scripts/check-ffmpeg.sh` sprawdza, czy build ma wszystkie potrzebne
elementy i czy działa kodowanie x264/x265. CI buduje FFmpeg tylko po
zmianie `packaging/ffmpeg.env` lub skryptu (cache).

W folderze `licenses` jest pełny tekst GPL v3 (`FFmpeg-GPL.txt`),
licencje x264, x265 i nagłówków oraz `FFmpeg-SOURCE.txt` z wersjami.
Każdy release zawiera `ffmpeg-source.tar` (`scripts/ffmpeg-source.sh`):
niezmienione archiwa źródeł wszystkich składników i skrypty budowania.
Zmiana wersji: nowy adres, wersja i suma w `packaging/ffmpeg.env` naraz.

FFmpeg pobierany przez samą aplikację, gdy go brakuje (np. przy
uruchomieniu z `cargo run`), pochodzi od dostawców `ffmpeg-sidecar`.

## Znaki towarowe

BibiCord nie jest powiązany z Discord Inc. ani przez nią wspierany.
„Discord” jest znakiem towarowym Discord Inc. i pojawia się tu wyłącznie
w celu opisania projektu.
