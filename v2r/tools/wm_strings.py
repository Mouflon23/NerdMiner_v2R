"""Copies WiFiManager v2.0.17's page strings into the firmware, so that the
Rust portal serves the same pages as the C++ firmware.

    git clone --depth 1 --branch v2.0.17 https://github.com/tzapu/WiFiManager.git
    python v2r/tools/wm_strings.py WiFiManager/wm_strings_en.h

It writes v2r/firmware/src/portal/strings.rs.
"""

import pathlib
import re
import sys

OUT = pathlib.Path(__file__).parent.parent / "firmware" / "src" / "portal" / "strings.rs"

NAMES = [
    "HTTP_HEAD_START", "HTTP_SCRIPT", "HTTP_HEAD_END", "HTTP_ROOT_MAIN", "HTTP_ITEM_QI", "HTTP_ITEM_QP",
    "HTTP_ITEM", "HTTP_FORM_START", "HTTP_FORM_WIFI", "HTTP_FORM_END", "HTTP_FORM_LABEL",
    "HTTP_FORM_PARAM_HEAD", "HTTP_FORM_PARAM", "HTTP_SCAN_LINK", "HTTP_SAVED", "HTTP_PARAMSAVED", "HTTP_END",
    "HTTP_ERASEBTN", "HTTP_BACKBTN", "HTTP_STATUS_ON", "HTTP_STATUS_OFF", "HTTP_STATUS_OFFPW",
    "HTTP_STATUS_OFFNOAP", "HTTP_STATUS_OFFFAIL", "HTTP_STATUS_NONE", "HTTP_BR", "HTTP_STYLE", "HTTP_HELP",
    "HTTP_INFO_esphead", "HTTP_INFO_chiprev", "HTTP_INFO_lastreset", "HTTP_INFO_aphost", "HTTP_INFO_temp",
    "HTTP_INFO_freeheap", "HTTP_INFO_wifihead", "HTTP_INFO_uptime", "HTTP_INFO_chipid", "HTTP_INFO_idesize",
    "HTTP_INFO_cpufreq", "HTTP_INFO_apip", "HTTP_INFO_apmac", "HTTP_INFO_apssid", "HTTP_INFO_apbssid",
    "HTTP_INFO_stassid", "HTTP_INFO_staip", "HTTP_INFO_stamac", "HTTP_INFO_conx", "HTTP_INFO_host",
    "HTTP_INFO_aboutver", "HTTP_INFO_aboutdate",
    "S_titlewifisaved", "S_titlewifisettings", "S_titlewifi", "S_titleinfo", "S_titleparam",
    "S_titleparamsaved", "S_titleexit", "S_titlereset", "S_titleerase", "S_titleclose", "S_nonetworks",
    "S_exiting", "S_resetting", "S_closing", "S_error", "S_notfound", "S_passph", "S_y", "S_n", "S_brand",
]
MENU = ["WIFI", "WIFINOSCAN", "INFO", "PARAM", "CLOSE", "RESTART", "EXIT", "ERASE", "UPDATE", "SEP"]
LITERAL = r'"((?:[^"\\]|\\.)*)"'


def main(header: str) -> None:
    lines = pathlib.Path(header).read_text(encoding="utf-8").splitlines()
    source = "\n".join(line for line in lines if not line.lstrip().startswith("//"))
    out = []
    for name in NAMES:
        found = re.search(r"const char\s+" + name + r"\[\]\s+PROGMEM\s*=\s*((?:\s*" + LITERAL + r")+)\s*;", source)
        if not found:
            sys.exit(f"{name} not found")
        text = "".join(re.findall(LITERAL, found.group(1)))
        out.append(f'pub const {name.upper()}: &str = "{text}";')
    menu = re.search(r"HTTP_PORTAL_MENU\[\] PROGMEM = \{(.*?)\};", source, re.S).group(1)
    for name, text in zip(MENU, re.findall(LITERAL, menu)):
        out.append(f'pub const MENU_{name}: &str = "{text}";')
    head = (
        "//! WiFiManager v2.0.17's page strings (`wm_strings_en.h`), as they are, so\n"
        "//! that the portal looks the same. Written by v2r/tools/wm_strings.py.\n\n"
        "#![allow(dead_code)]\n\n"
    )
    OUT.parent.mkdir(exist_ok=True)
    OUT.write_text(head + "\n".join(out) + "\n", encoding="utf-8", newline="\n")


if __name__ == "__main__":
    main(sys.argv[1])
