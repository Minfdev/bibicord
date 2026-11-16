# dmgbuild settings. Paths come from -D defines passed by scripts/build-macos-dmg.sh.
import os

from dmgbuild import licensing

app = defines["app"]  # noqa: F821
bg = defines["background"]  # noqa: F821

format = "ULMO"  # LZMA, macOS 10.15+
filesystem = "APFS"
files = [app]
symlinks = {"Aplikacje": "/Applications"}
icon = defines["volicon"]  # noqa: F821
badge_icon = None
icon_locations = {os.path.basename(app): (170, 200), "Aplikacje": (470, 200)}
background = bg
window_rect = ((200, 160), (640, 400))
default_view = "icon-view"
show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False
show_icon_preview = False
arrange_by = None
icon_size = 112
text_size = 13


# License agreement shown when the DMG is opened: Finder mounts it only after
# "Zgadzam się" / "Agree". The text (Polish summary + MIT) comes from
# scripts/installer-license.sh (UTF-8, as for the Windows installer) and is
# embedded as RTF, which unlike the legacy Mac encodings covers Polish letters.
def rtf(text):
    out = []
    for ch in text:
        if ch in "\\{}":
            out.append("\\" + ch)
        elif ch == "\n":
            out.append("\\par\n")
        elif ord(ch) < 0x80:
            out.append(ch)
        else:
            b = ch.encode("utf-16-be")
            for i in range(0, len(b), 2):
                n = int.from_bytes(b[i : i + 2], "big")  # \uN: signed 16-bit unit
                out.append(f"\\u{n - 0x10000 if n > 0x7FFF else n}?")
    head = "{\\rtf1\\ansi\\ansicpg1252\\deff0{\\fonttbl{\\f0\\fswiss Helvetica;}}\\f0\\fs22\n"
    return (head + "".join(out) + "}").encode("ascii")


with open(defines["license"], encoding="utf-8-sig") as f:  # noqa: F821
    license_rtf = rtf(f.read())

# dmgbuild has no Polish entry: Poland's Mac region code (verPoland) and the
# Mac Central European encoding, used for the button labels below.
licensing.language_info_map.setdefault(
    "pl_PL", {"buttons": "Polish", "name": "Polish", "language_id": 42, "encoding": "mac_latin2"}
)

# dmgbuild 2.0.0 ignores "default-language": the fallback for other system
# languages is always en_US, hence that entry.
license = {
    "default-language": "pl_PL",
    # Polish buttons on a Polish system, English ones elsewhere; same text.
    "licenses": {"pl_PL": license_rtf, "en_US": license_rtf},
    "buttons": {
        "pl_PL": (
            "Polski",
            "Zgadzam się",
            "Nie zgadzam się",
            "Drukuj",
            "Zachowaj...",
            'Jeśli akceptujesz warunki licencji, kliknij "Zgadzam się", aby zainstalować '
            'program. Jeśli nie, kliknij "Nie zgadzam się".',
        ),
    },
}
