"""Draws the Rust firmware's screens on the PC, to check the assets and the
text layout without flashing. Needs freetype-py and Pillow.

    python v2r/tools/preview.py [out-directory]

It applies the same layout as v2r/firmware/src/text.rs (OpenFontRender's and
TFT_eSPI's), to the same backgrounds and fonts that v2r/tools/assets.py
converts, with sample figures, and writes one PNG per screen.
"""

import pathlib
import re
import sys

import freetype
from PIL import Image

sys.path.insert(0, str(pathlib.Path(__file__).parent))
import assets  # noqa: E402

LIGHT, BLACK, WHITE, GREY = 0xDEDB, 0x0000, 0xFFFF, 0x9C92

# Items in drawing order, with sample figures, as display.rs lists them:
# ("smooth", size, x, y, color, right, text)
# ("gfx", font, size, datum, background, x, y, color, text)
# ("font2", datum, x, y, color, text)
# ("fill", x, y, width, height, color)
SCREENS = {
    "miner": [
        ("smooth", 35, 118, 114, BLACK, True, "352.4"),
        ("smooth", 18, 268, 138, BLACK, True, "1234"),
        ("smooth", 18, 186, 20, LIGHT, False, "42"),
        ("smooth", 18, 186, 48, LIGHT, False, "0.787"),
        ("smooth", 18, 186, 76, LIGHT, False, "0"),
        ("smooth", 14, 315, 104, LIGHT, True, "0  01:23:45"),
        ("smooth", 24, 285, 56, LIGHT, False, "0"),
        ("smooth", 10, 239, 1, BLACK, True, "41"),
        ("smooth", 4, 244, 3, BLACK, True, "0"),
        ("smooth", 10, 286, 1, BLACK, True, "23:47"),
    ],
    "clock": [
        ("smooth", 25, 94, 129, BLACK, True, "352.4"),
        ("gfx", "FreeSansBold9pt7b", 1, "TL", None, 202, 3, BLACK, "$101234"),
        ("smooth", 18, 254, 140, BLACK, True, "975223"),
        ("gfx", "FreeSansBold18pt7b", 2, "TL", BLACK, 130, 50, LIGHT, "23:47"),
    ],
    "global_hash": [
        ("gfx", "FreeSansBold9pt7b", 1, "TL", None, 198, 3, BLACK, "$101234"),
        ("gfx", "FreeSansBold9pt7b", 1, "TL", None, 268, 3, BLACK, "23:47"),
        ("gfx", "FreeSans9pt7b", 1, "TR", None, 302, 52, GREY, "3 sat/vB"),
        ("gfx", "FreeSans9pt7b", 1, "TR", None, 302, 88, GREY, "148.23T"),
        ("smooth", 17, 274, 145, BLACK, True, "1023"),
        ("smooth", 28, 140, 104, LIGHT, True, "975223"),
        ("fill", 2, 149, 2 + 138 * 64 // 100, 168, LIGHT),
        ("font2", "MC", 72, 159, BLACK, "74777 BLOCKS"),
    ],
    "price": [
        ("smooth", 25, 94, 129, BLACK, True, "352.4"),
        ("smooth", 18, 254, 138, WHITE, True, "975223"),
        ("gfx", "FreeSansBold9pt7b", 1, "TL", None, 222, 3, BLACK, "23:47"),
        ("gfx", "FreeSansBold24pt7b", 1, "TR", BLACK, 300, 58, LIGHT, "$101234"),
    ],
    "init": [
        ("font2", "TL", 24, 147, BLACK, "V0.1.0"),
    ],
}


def blend(alpha: int, fg: int, bg: int) -> int:
    def parts(c):
        return ((c >> 10) & 0x3E) + 1, ((c >> 4) & 0x7E) + 1, ((c << 1) & 0x3E) + 1

    f, b = parts(fg), parts(bg)
    r, g, bl = ((fc * alpha + bc * (255 - alpha)) >> 9 for fc, bc in zip(f, b))
    return (r << 11) | (g << 5) | bl


def draw(pixels, face, size, x, y, color, right, text):
    face.set_pixel_sizes(0, size)
    ascender = face.size.ascender >> 6
    metrics = []
    for ch in text:
        if face.get_char_index(ch) == 0 and ch != " ":
            continue
        face.load_char(ch, freetype.FT_LOAD_DEFAULT)
        bearing = face.glyph.metrics.horiBearingX >> 6
        box = face.glyph.get_glyph().get_cbox(freetype.FT_GLYPH_BBOX_PIXELS)
        metrics.append((ch, face.glyph.advance.x >> 6, box.xMin, box.xMax, bearing))
    if not metrics:
        return
    pen, x_min, x_max = x, 1 << 30, -(1 << 30)
    for _, advance, lo, hi, _ in metrics:
        x_min, x_max = min(x_min, pen + lo), max(x_max, pen + hi)
        pen += advance
    bearing = metrics[0][4]
    offset = x_min - x
    if right:
        offset += x_max - x_min
        pen = x - bearing
    else:
        pen = x
    offset -= bearing
    for ch, advance, *_ in metrics:
        face.load_char(ch, freetype.FT_LOAD_RENDER)
        bitmap = face.glyph.bitmap
        left = pen - offset + face.glyph.bitmap_left
        top = y + ascender - face.glyph.bitmap_top
        for row in range(bitmap.rows):
            for column in range(bitmap.width):
                alpha = bitmap.buffer[row * bitmap.pitch + column]
                px, py = left + column, top + row
                if alpha and 0 <= px < 320 and 0 <= py < 170:
                    pixels[py * 320 + px] = blend(alpha, color, 0)
        pen += advance


def main(out: str) -> None:
    source = (assets.MEDIA / "myFonts.h").read_text(encoding="utf-8", errors="replace")
    face = freetype.Face(assets._temporary("DigitalNumbers", bytes(assets.c_array(source, "DigitalNumbers"))))
    for name, items in SCREENS.items():
        data = (assets.OUT / f"{name}.rgb565").read_bytes()
        pixels = [int.from_bytes(data[i : i + 2], "big") for i in range(0, len(data), 2)]
        for kind, *item in items:
            if kind == "smooth":
                draw(pixels, face, *item)
            elif kind == "gfx":
                draw_gfx(pixels, *item)
            elif kind == "font2":
                draw_font2(pixels, *item)
            else:
                x, y, w, h, color = item
                for py in range(y, y + h):
                    for px in range(x, x + w):
                        put(pixels, px, py, color)
        image = Image.new("RGB", (320, 170))
        image.putdata([(((v >> 11) & 31) * 255 // 31, ((v >> 5) & 63) * 255 // 63, (v & 31) * 255 // 31) for v in pixels])
        image.resize((640, 340), Image.NEAREST).save(pathlib.Path(out) / f"{name}.png")
    for path in assets.OUT.glob(".*.ttf"):
        path.unlink()


def put(pixels, x, y, color):
    if 0 <= x < 320 and 0 <= y < 170:
        pixels[y * 320 + x] = color


def gfx_font(name):
    source = (assets.TFT_ESPI / "GFXFF" / f"{name}.h").read_text(encoding="utf-8", errors="replace")
    bitmap = assets.c_array(source, f"{name}Bitmaps")
    body = re.search(name + r"Glyphs\[\]\s*PROGMEM\s*=\s*\{(.*?)\};", source, re.S).group(1)
    body = re.sub(r"//[^\n]*", "", body)
    glyphs = [list(map(int, g.split(","))) for g in re.findall(r"\{([^}]*)\}", body)]
    return bitmap, glyphs, 0x20


def draw_gfx(pixels, name, size, datum, background, x, y, color, text):
    """TFT_eSPI drawString with a free font, as text.rs does it."""
    bitmap, glyphs, first = gfx_font(name)
    above = below = 0
    for g in glyphs[:-1]:
        ab = -g[5]
        above, below = max(above, ab), max(below, g[2] - ab)

    def get(ch):
        index = ord(ch) - first
        return glyphs[index] if 0 <= index < len(glyphs) else None

    width = 0
    for i, ch in enumerate(text):
        g = get(ch)
        if g:
            width += g[3] if i + 1 < len(text) else g[4] + g[1]
    width *= size
    y += above * size
    if datum == "TR":
        x -= width
    if background is not None and background != color and text and get(text[0]):
        xo = get(text[0])[4] * size
        if xo > 0:
            xo = 0
        else:
            width -= xo
        for py in range(y - above * size, y - above * size + (above + below) * size):
            for px in range(x + xo, x + xo + width):
                put(pixels, px, py, background)
    pen = x
    for ch in text:
        g = get(ch)
        if not g:
            continue
        offset, w, h, advance, xo, yo = g
        bit = 0
        for row in range(h):
            for column in range(w):
                if bitmap[offset + bit // 8] & (0x80 >> (bit % 8)):
                    for dy in range(size):
                        for dx in range(size):
                            put(pixels, pen + (xo + column) * size + dx, y + (yo + row) * size + dy, color)
                bit += 1
        pen += advance * size


def draw_font2(pixels, datum, x, y, color, text):
    """TFT_eSPI drawString with Font 2, as text.rs does it."""
    source = (assets.TFT_ESPI / "Font16.c").read_text(encoding="utf-8", errors="replace")
    source = assets.preprocess(source, assets.FONT2_DEFINES)
    widths = assets.c_array(source, "widtbl_f16")
    width = sum(widths[ord(c) - 32] for c in text)
    if datum == "MC":
        x -= width // 2
        y -= 8
    for ch in text:
        w = widths[ord(ch) - 32]
        rows = assets.c_array(source, f"chr_f16_{ord(ch):02X}")
        stride = (w + 6) // 8
        for row in range(16):
            for k in range(stride):
                line = rows[stride * row + k] if stride * row + k < len(rows) else 0
                for bit in range(8):
                    if line & (0x80 >> bit):
                        put(pixels, x + k * 8 + bit, y + row, color)
        x += w


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else ".")
