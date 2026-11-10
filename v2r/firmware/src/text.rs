//! Anti-aliased text as OpenFontRender draws it in the C++ firmware.
//!
//! `v2r/tools/assets.py` renders the glyphs with FreeType ahead of time,
//! together with the metrics the layout reads: advance, grid-fitted box and
//! left bearing. [`draw`] follows `OpenFontRender::drawHString` step by step,
//! so text lands on the same pixels. That includes blending the edges with the
//! text's background colour, black by default, instead of with what lies under
//! them.

pub struct Glyph {
    pub ch: u8,
    pub advance: i16,
    pub x_min: i16,
    pub x_max: i16,
    pub bearing: i16,
    pub left: i16,
    pub top: i16,
    pub width: u16,
    pub rows: u16,
    pub alpha: &'static [u8],
}

pub struct Font {
    pub ascender: i16,
    pub glyphs: &'static [Glyph],
}

impl Font {
    fn glyph(&self, ch: u8) -> Option<&Glyph> {
        self.glyphs.iter().find(|glyph| glyph.ch == ch)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// A rectangle in screen pixels, `x1` and `y1` excluded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl Rect {
    pub const EMPTY: Rect = Rect { x0: 0, y0: 0, x1: 0, y1: 0 };

    pub fn is_empty(&self) -> bool {
        self.x0 >= self.x1 || self.y0 >= self.y1
    }

    pub fn union(self, other: Rect) -> Rect {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        Rect {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        !self.is_empty()
            && !other.is_empty()
            && self.x0 < other.x1
            && other.x0 < self.x1
            && self.y0 < other.y1
            && other.y0 < self.y1
    }

    pub fn clip(self, width: i32, height: i32) -> Rect {
        Rect { x0: self.x0.max(0), y0: self.y0.max(0), x1: self.x1.min(width), y1: self.y1.min(height) }
    }
}

/// Where pixels go: a window of the screen held in RAM.
pub struct Canvas<'a> {
    pub pixels: &'a mut [u16],
    pub area: Rect,
}

impl Canvas<'_> {
    #[inline]
    fn put(&mut self, x: i32, y: i32, color: u16) {
        let area = self.area;
        if x >= area.x0 && x < area.x1 && y >= area.y0 && y < area.y1 {
            let width = area.x1 - area.x0;
            self.pixels[((y - area.y0) * width + (x - area.x0)) as usize] = color;
        }
    }
}

/// `OpenFontRender::alphaBlend`, to the bit.
fn alpha_blend(alpha: u8, fg: u16, bg: u16) -> u16 {
    let alpha = u32::from(alpha);
    let channel = |color: u16, shift: u32, mask: u32| ((u32::from(color) >> shift) & mask) + 1;
    let (fg_r, fg_g, fg_b) = (channel(fg, 10, 0x3e), channel(fg, 4, 0x7e), ((u32::from(fg) << 1) & 0x3e) + 1);
    let (bg_r, bg_g, bg_b) = (channel(bg, 10, 0x3e), channel(bg, 4, 0x7e), ((u32::from(bg) << 1) & 0x3e) + 1);
    let mix = |f: u32, b: u32| (f * alpha + b * (255 - alpha)) >> 9;
    ((mix(fg_r, bg_r) << 11) | (mix(fg_g, bg_g) << 5) | mix(fg_b, bg_b)) as u16
}

/// Where each glyph's pen starts, as `drawHString` computes it.
struct Layout {
    pen: i32,
    offset: i32,
}

fn layout(font: &Font, text: &str, x: i32, align: Align) -> Layout {
    let (mut x_min, mut x_max) = (i32::MAX, i32::MIN);
    let mut pen = x;
    let mut bearing = 0;
    for (index, glyph) in text.bytes().filter_map(|ch| font.glyph(ch)).enumerate() {
        if index == 0 {
            bearing = i32::from(glyph.bearing);
        }
        x_min = x_min.min(pen + i32::from(glyph.x_min));
        x_max = x_max.max(pen + i32::from(glyph.x_max));
        pen += i32::from(glyph.advance);
    }
    if x_min > x_max {
        return Layout { pen: x, offset: 0 };
    }
    let mut offset = x_min - x;
    match align {
        Align::Left => Layout { pen: x, offset: offset - bearing },
        Align::Right => {
            offset += x_max - x_min;
            Layout { pen: x - bearing, offset: offset - bearing }
        }
    }
}

fn for_each_pixel(font: &Font, text: &str, x: i32, y: i32, align: Align, mut pixel: impl FnMut(i32, i32, u8)) {
    let Layout { mut pen, offset } = layout(font, text, x, align);
    let top = y + i32::from(font.ascender);
    for glyph in text.bytes().filter_map(|ch| font.glyph(ch)) {
        let left = pen - offset + i32::from(glyph.left);
        let row0 = top - i32::from(glyph.top);
        let width = usize::from(glyph.width);
        for (row, line) in glyph.alpha.chunks_exact(width.max(1)).enumerate().take(usize::from(glyph.rows)) {
            for (column, &alpha) in line.iter().enumerate() {
                if alpha != 0 {
                    pixel(left + column as i32, row0 + row as i32, alpha);
                }
            }
        }
        pen += i32::from(glyph.advance);
    }
}

/// An Adafruit GFX font, as TFT_eSPI's free fonts store them.
pub struct GfxFont {
    pub first: u16,
    pub last: u16,
    pub bitmap: &'static [u8],
    pub glyphs: &'static [GfxGlyph],
}

pub struct GfxGlyph {
    pub offset: u16,
    pub width: u8,
    pub height: u8,
    pub advance: u8,
    pub x_offset: i8,
    pub y_offset: i8,
}

impl GfxFont {
    fn glyph(&self, ch: u8) -> Option<&GfxGlyph> {
        let ch = u16::from(ch);
        if ch < self.first || ch > self.last {
            return None;
        }
        self.glyphs.get(usize::from(ch - self.first))
    }

    /// `glyph_ab` and `glyph_bb` of `setFreeFont`: the most any glyph rises
    /// above the baseline and drops below it. Like TFT_eSPI, the last glyph
    /// is left out.
    fn extent(&self) -> (i32, i32) {
        let count = usize::from(self.last - self.first);
        self.glyphs.iter().take(count).fold((0, 0), |(above, below), glyph| {
            let ab = -i32::from(glyph.y_offset);
            (above.max(ab), below.max(i32::from(glyph.height) - ab))
        })
    }

    /// `textWidth`: the advances, except the last glyph's own width.
    fn width(&self, text: &str) -> i32 {
        let bytes = text.as_bytes();
        bytes
            .iter()
            .enumerate()
            .filter_map(|(index, &ch)| self.glyph(ch).map(|glyph| (index, glyph)))
            .map(|(index, glyph)| {
                if index + 1 < bytes.len() {
                    i32::from(glyph.advance)
                } else {
                    i32::from(glyph.x_offset) + i32::from(glyph.width)
                }
            })
            .sum()
    }
}

/// TFT_eSPI's Font 2: 16 rows of up to 16 pixels, MSB first.
pub struct Font2 {
    pub widths: [u8; 96],
    pub glyphs: [&'static [u8]; 96],
}

const FONT2_HEIGHT: i32 = 16;

/// Where TFT_eSPI anchors a string: `textdatum`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Datum {
    TopLeft,
    TopRight,
    MiddleCenter,
}

/// `drawString` with a free font, in a sprite: `size` is `setTextSize`, and a
/// `background` other than the text colour fills the text's box first.
pub fn for_each_gfx_pixel(
    font: &GfxFont,
    size: i32,
    text: &str,
    x: i32,
    y: i32,
    datum: Datum,
    color: u16,
    background: Option<u16>,
    mut pixel: impl FnMut(i32, i32, u16),
) {
    let (above, below) = font.extent();
    let mut width = font.width(text) * size;
    let height = above * size;
    let (mut x, mut y) = (x, y + height);
    match datum {
        Datum::TopLeft => {}
        Datum::TopRight => x -= width,
        Datum::MiddleCenter => {
            x -= width / 2;
            y -= height / 2;
        }
    }
    if let Some(background) = background.filter(|&bg| bg != color) {
        if let Some(first) = text.bytes().find_map(|ch| font.glyph(ch)) {
            let mut x_offset = i32::from(first.x_offset) * size;
            if x_offset > 0 {
                x_offset = 0;
            } else {
                width -= x_offset;
            }
            let top = y - above * size;
            for py in top..top + (above + below) * size {
                for px in x + x_offset..x + x_offset + width {
                    pixel(px, py, background);
                }
            }
        }
    }
    let mut pen = x;
    for ch in text.bytes() {
        let Some(glyph) = font.glyph(ch) else { continue };
        let mut bits = font.bitmap[usize::from(glyph.offset)..].iter().flat_map(|byte| (0..8).rev().map(move |bit| byte >> bit & 1 != 0));
        for row in 0..i32::from(glyph.height) {
            for column in 0..i32::from(glyph.width) {
                if bits.next() == Some(true) {
                    let gx = pen + (i32::from(glyph.x_offset) + column) * size;
                    let gy = y + (i32::from(glyph.y_offset) + row) * size;
                    for dy in 0..size {
                        for dx in 0..size {
                            pixel(gx + dx, gy + dy, color);
                        }
                    }
                }
            }
        }
        pen += i32::from(glyph.advance) * size;
    }
}

/// `drawString` with Font 2 at size 1, the text colour also its background.
pub fn for_each_font2_pixel(font: &Font2, text: &str, x: i32, y: i32, datum: Datum, color: u16, mut pixel: impl FnMut(i32, i32, u16)) {
    let index = |ch: u8| usize::from(if (32..128).contains(&ch) { ch - 32 } else { 0 });
    let width: i32 = text.bytes().map(|ch| i32::from(font.widths[index(ch)])).sum();
    let (mut x, mut y) = (x, y);
    match datum {
        Datum::TopLeft => {}
        Datum::TopRight => x -= width,
        Datum::MiddleCenter => {
            x -= width / 2;
            y -= FONT2_HEIGHT / 2;
        }
    }
    for ch in text.bytes() {
        if !(32..128).contains(&ch) {
            continue;
        }
        let glyph_width = i32::from(font.widths[index(ch)]);
        let stride = ((glyph_width + 6) / 8) as usize;
        let rows = font.glyphs[index(ch)];
        for row in 0..FONT2_HEIGHT as usize {
            for k in 0..stride {
                let line = rows.get(stride * row + k).copied().unwrap_or(0);
                for bit in 0..8 {
                    if line & (0x80 >> bit) != 0 {
                        pixel(x + (k * 8) as i32 + bit, y + row as i32, color);
                    }
                }
            }
        }
        x += glyph_width;
    }
}

/// Smooth text through the same callback as the bitmap fonts.
pub fn for_each_smooth_pixel(font: &Font, text: &str, x: i32, y: i32, align: Align, color: u16, mut pixel: impl FnMut(i32, i32, u16)) {
    for_each_pixel(font, text, x, y, align, |px, py, alpha| pixel(px, py, alpha_blend(alpha, color, 0)));
}

impl Canvas<'_> {
    pub fn set(&mut self, x: i32, y: i32, color: u16) {
        self.put(x, y, color);
    }
}
