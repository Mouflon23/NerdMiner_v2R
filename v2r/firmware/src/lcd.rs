//! The T-Display S3 panel: an ST7789 with 170 × 320 pixels on an 8-bit 8080
//! bus, driven here in landscape, 320 × 170.
//!
//! The bus is bit-banged through the GPIO set and clear registers. Writing the
//! eight data lines takes two register writes per byte, and so does the WR
//! strobe. That is about 50 cycles per byte, so a full screen takes some 25 ms.
//! Only the parts that change are redrawn, and only every few seconds.
//!
//! Pins and the init sequence come from `Setup206_LilyGo_T_Display_S3.h` and
//! `ST7789_Init.h` (`INIT_SEQUENCE_3`) in the C++ firmware's TFT_eSPI.

use core::ptr::write_volatile;

use embassy_time::Timer;
use esp_hal::gpio::{AnyPin, Level, Output, OutputConfig};

pub const WIDTH: u16 = 320;
pub const HEIGHT: u16 = 170;

/// Where the 170 visible rows start in the controller's 240-row memory.
const ROW_OFFSET: u16 = 35;

const GPIO_OUT_W1TS: *mut u32 = 0x6000_4008 as *mut u32;
const GPIO_OUT_W1TC: *mut u32 = 0x6000_400c as *mut u32;
const GPIO_OUT1_W1TS: *mut u32 = 0x6000_4014 as *mut u32;
const GPIO_OUT1_W1TC: *mut u32 = 0x6000_4018 as *mut u32;

const PIN_CS: u32 = 6;
const PIN_DC: u32 = 7;
const PIN_WR: u32 = 8;

/// D0..=D7 are GPIO 39..=42 and 45..=48, bits 7..=10 and 13..=16 of the
/// second output register.
const DATA_MASK: u32 = (0xf << 7) | (0xf << 13);

const fn data_bits(byte: u8) -> u32 {
    let byte = byte as u32;
    ((byte & 0xf) << 7) | ((byte >> 4) << 13)
}

static DATA_BITS: [u32; 256] = {
    let mut table = [0; 256];
    let mut i = 0;
    while i < 256 {
        table[i] = data_bits(i as u8);
        i += 1;
    }
    table
};

const SWRESET: u8 = 0x01;
const SLPIN: u8 = 0x10;
const SLPOUT: u8 = 0x11;
const NORON: u8 = 0x13;
const INVON: u8 = 0x21;
const DISPOFF: u8 = 0x28;
const DISPON: u8 = 0x29;
const CASET: u8 = 0x2a;
const RASET: u8 = 0x2b;
const RAMWR: u8 = 0x2c;
const MADCTL: u8 = 0x36;
const COLMOD: u8 = 0x3a;

/// Row and column exchange plus column mirror: landscape, USB on the left,
/// TFT_eSPI's rotation 1. Rotation 3, the other way up, mirrors rows instead.
const MADCTL_LANDSCAPE: u8 = 0x60;
const MADCTL_LANDSCAPE_FLIPPED: u8 = 0xa0;

/// Everything after sleep out in `INIT_SEQUENCE_3`, as command and data.
const INIT: &[(u8, &[u8])] = &[
    (NORON, &[]),
    (MADCTL, &[MADCTL_LANDSCAPE]),
    (COLMOD, &[0x55]),
    (0xb2, &[0x0b, 0x0b, 0x00, 0x33, 0x33]),
    (0xb7, &[0x75]),
    (0xbb, &[0x28]),
    (0xc0, &[0x2c]),
    (0xc2, &[0x01]),
    (0xc3, &[0x1f]),
    (0xc6, &[0x13]),
    (0xd0, &[0xa7]),
    (0xd0, &[0xa4, 0xa1]),
    (0xd6, &[0xa1]),
    (0xe0, &[0xf0, 0x05, 0x0a, 0x06, 0x06, 0x03, 0x2b, 0x32, 0x43, 0x36, 0x11, 0x10, 0x2b, 0x32]),
    (0xe1, &[0xf0, 0x08, 0x0c, 0x0b, 0x09, 0x24, 0x2b, 0x22, 0x43, 0x38, 0x15, 0x16, 0x2f, 0x37]),
    (INVON, &[]),
];

#[inline(always)]
fn set(register: *mut u32, bits: u32) {
    unsafe { write_volatile(register, bits) }
}

pub struct Pins {
    pub power: AnyPin<'static>,
    pub backlight: AnyPin<'static>,
    pub reset: AnyPin<'static>,
    pub cs: AnyPin<'static>,
    pub dc: AnyPin<'static>,
    pub wr: AnyPin<'static>,
    pub rd: AnyPin<'static>,
    pub data: [AnyPin<'static>; 8],
}

pub struct Lcd {
    backlight: Output<'static>,
    reset: Output<'static>,
    // Configured as outputs once, then written through the registers.
    _bus: [Output<'static>; 13],
    on: bool,
    flipped: bool,
}

impl Lcd {
    pub fn new(pins: Pins) -> Self {
        let out = |pin: AnyPin<'static>, level| Output::new(pin, level, OutputConfig::default());
        let [d0, d1, d2, d3, d4, d5, d6, d7] = pins.data;
        Self {
            backlight: out(pins.backlight, Level::Low),
            reset: out(pins.reset, Level::High),
            _bus: [
                // The panel and its backlight run from the board's 5 V rail.
                out(pins.power, Level::High),
                out(pins.cs, Level::High),
                out(pins.dc, Level::High),
                out(pins.wr, Level::High),
                out(pins.rd, Level::High),
                out(d0, Level::Low),
                out(d1, Level::Low),
                out(d2, Level::Low),
                out(d3, Level::Low),
                out(d4, Level::Low),
                out(d5, Level::Low),
                out(d6, Level::Low),
                out(d7, Level::Low),
            ],
            on: false,
            flipped: false,
        }
    }

    #[inline(always)]
    fn byte(&mut self, byte: u8) {
        set(GPIO_OUT1_W1TC, DATA_MASK);
        set(GPIO_OUT1_W1TS, DATA_BITS[byte as usize]);
        set(GPIO_OUT_W1TC, 1 << PIN_WR);
        set(GPIO_OUT_W1TS, 1 << PIN_WR);
    }

    fn command(&mut self, command: u8, data: &[u8]) {
        set(GPIO_OUT_W1TC, (1 << PIN_CS) | (1 << PIN_DC));
        self.byte(command);
        set(GPIO_OUT_W1TS, 1 << PIN_DC);
        for &byte in data {
            self.byte(byte);
        }
        set(GPIO_OUT_W1TS, 1 << PIN_CS);
    }

    pub async fn init(&mut self) {
        self.reset.set_low();
        Timer::after_millis(10).await;
        self.reset.set_high();
        Timer::after_millis(120).await;
        self.command(SWRESET, &[]);
        Timer::after_millis(150).await;
        self.command(SLPOUT, &[]);
        Timer::after_millis(120).await;
        for (command, data) in INIT {
            self.command(*command, data);
        }
        self.fill(0, 0, WIDTH, HEIGHT, 0);
        self.command(DISPON, &[]);
        Timer::after_millis(120).await;
        self.set_on(true);
    }

    /// Turns the panel and its backlight on or off; off, it sleeps.
    pub fn set_on(&mut self, on: bool) {
        if on {
            self.command(SLPOUT, &[]);
            self.command(DISPON, &[]);
            self.backlight.set_high();
        } else {
            self.backlight.set_low();
            self.command(DISPOFF, &[]);
            self.command(SLPIN, &[]);
        }
        self.on = on;
    }

    /// Turns the picture upside down, as `alternateScreenRotation` does.
    pub fn flip(&mut self) {
        self.flipped = !self.flipped;
        let madctl = if self.flipped { MADCTL_LANDSCAPE_FLIPPED } else { MADCTL_LANDSCAPE };
        self.command(MADCTL, &[madctl]);
    }

    pub fn is_on(&self) -> bool {
        self.on
    }

    fn window(&mut self, x: u16, y: u16, width: u16, height: u16) {
        let (x1, y0) = (x + width - 1, y + ROW_OFFSET);
        let y1 = y0 + height - 1;
        let [xh, xl] = x.to_be_bytes();
        let [x1h, x1l] = x1.to_be_bytes();
        let [yh, yl] = y0.to_be_bytes();
        let [y1h, y1l] = y1.to_be_bytes();
        self.command(CASET, &[xh, xl, x1h, x1l]);
        self.command(RASET, &[yh, yl, y1h, y1l]);
    }

    /// Writes `pixels`, RGB565 row by row, into the rectangle at (`x`, `y`).
    pub fn blit(&mut self, x: u16, y: u16, width: u16, pixels: &[u16]) {
        let height = (pixels.len() / width as usize) as u16;
        if height == 0 {
            return;
        }
        self.window(x, y, width, height);
        set(GPIO_OUT_W1TC, (1 << PIN_CS) | (1 << PIN_DC));
        self.byte(RAMWR);
        set(GPIO_OUT_W1TS, 1 << PIN_DC);
        for &pixel in pixels {
            let [high, low] = pixel.to_be_bytes();
            self.byte(high);
            self.byte(low);
        }
        set(GPIO_OUT_W1TS, 1 << PIN_CS);
    }

    pub fn fill(&mut self, x: u16, y: u16, width: u16, height: u16, color: u16) {
        self.window(x, y, width, height);
        set(GPIO_OUT_W1TC, (1 << PIN_CS) | (1 << PIN_DC));
        self.byte(RAMWR);
        set(GPIO_OUT_W1TS, 1 << PIN_DC);
        let [high, low] = color.to_be_bytes();
        for _ in 0..u32::from(width) * u32::from(height) {
            self.byte(high);
            self.byte(low);
        }
        set(GPIO_OUT_W1TS, 1 << PIN_CS);
    }
}
