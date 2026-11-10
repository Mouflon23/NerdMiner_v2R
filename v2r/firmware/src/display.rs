//! The screens of the C++ firmware's T-Display driver (`tDisplayDriver.cpp`),
//! drawn the same way: a background image, then text and shapes at fixed
//! places, in the same order.
//!
//! The C++ firmware redraws the whole screen every second. Over a bit-banged
//! bus, that would cost core 1 about 25 ms a second. So this redraws only what
//! changed, together with the background beneath it and anything overlapping
//! it: the same pixels, for a fraction of the bus time.

use core::fmt::Write;

use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Timer};
use heapless::{String, Vec};
use log::info;
use static_cell::ConstStaticCell;

use crate::buttons::{Button, Event};
use crate::lcd::{HEIGHT, Lcd, WIDTH};
use crate::netdata::{self, WANT_GLOBAL, WANT_HEIGHT, WANT_PRICE};
use crate::stats::Status;
use crate::text::{self, Align, Canvas, Datum, Font, Font2, GfxFont, Rect};
use crate::{clock, fonts, miner, stats, temperature};

const REFRESH: Duration = Duration::from_secs(1);

const TFT_BLACK: u16 = 0x0000;
const TFT_WHITE: u16 = 0xffff;
/// The light grey most figures are written in.
const LIGHT: u16 = 0xdedb;
/// The grey of the fee and difficulty on the global hashrate screen.
const GREY: u16 = 0x9c92;

/// `HALVING_BLOCKS`.
const HALVING_BLOCKS: u32 = 210_000;

/// The version the loading screen prints, where the C++ firmware prints its
/// `CURRENT_VERSION`.
const VERSION: &str = concat!("V", env!("CARGO_PKG_VERSION"));

static MINER_SCREEN: &[u8] = include_bytes!("../assets/miner.rgb565");
static CLOCK_SCREEN: &[u8] = include_bytes!("../assets/clock.rgb565");
static GLOBAL_HASH_SCREEN: &[u8] = include_bytes!("../assets/global_hash.rgb565");
static PRICE_SCREEN: &[u8] = include_bytes!("../assets/price.rgb565");
static INIT_SCREEN: &[u8] = include_bytes!("../assets/init.rgb565");
static SETUP_SCREEN: &[u8] = include_bytes!("../assets/setup.rgb565");

/// Which button, and what it did.
type Press = (usize, Event);
static PRESSES: Channel<CriticalSectionRawMutex, Press, 4> = Channel::new();

/// Reports one button's gestures. It is a task of its own so that a refresh
/// never cuts a double click short.
#[embassy_executor::task(pool_size = 2)]
pub async fn watch(index: usize, mut button: Button) -> ! {
    loop {
        let event = button.next().await;
        PRESSES.send((index, event)).await;
    }
}

/// Pixels composed in RAM before each transfer: a fifth of the screen.
const BAND_PIXELS: usize = WIDTH as usize * 34;
static BAND: ConstStaticCell<[u16; BAND_PIXELS]> = ConstStaticCell::new([0; BAND_PIXELS]);

const MAX_ITEMS: usize = 12;

#[derive(Clone)]
enum Shape {
    /// OpenFontRender text.
    Smooth { font: &'static Font, align: Align },
    /// A TFT_eSPI free font; `background` set fills the text box first.
    Gfx { font: &'static GfxFont, size: i32, datum: Datum, background: Option<u16> },
    /// TFT_eSPI's Font 2.
    Font2 { font: &'static Font2, datum: Datum },
    /// `fillRect`, with the width and height in `x1` and `y1`.
    Fill { x1: i32, y1: i32 },
}

/// Same font by address: comparing the glyph tables would be slow, and pointless.
impl PartialEq for Shape {
    fn eq(&self, other: &Self) -> bool {
        use core::ptr::eq;
        match (self, other) {
            (Shape::Smooth { font: a, align: x }, Shape::Smooth { font: b, align: y }) => eq(*a, *b) && x == y,
            (
                Shape::Gfx { font: a, size: s, datum: d, background: g },
                Shape::Gfx { font: b, size: t, datum: e, background: h },
            ) => eq(*a, *b) && s == t && d == e && g == h,
            (Shape::Font2 { font: a, datum: d }, Shape::Font2 { font: b, datum: e }) => eq(*a, *b) && d == e,
            (Shape::Fill { x1: a, y1: b }, Shape::Fill { x1: c, y1: d }) => a == c && b == d,
            _ => false,
        }
    }
}

#[derive(Clone, PartialEq)]
struct Item {
    shape: Shape,
    text: String<24>,
    x: i32,
    y: i32,
    color: u16,
}

impl Item {
    fn pixels(&self, mut pixel: impl FnMut(i32, i32, u16)) {
        let (text, x, y, color) = (self.text.as_str(), self.x, self.y, self.color);
        match self.shape {
            Shape::Smooth { font, align } => text::for_each_smooth_pixel(font, text, x, y, align, color, pixel),
            Shape::Gfx { font, size, datum, background } => {
                text::for_each_gfx_pixel(font, size, text, x, y, datum, color, background, pixel)
            }
            Shape::Font2 { font, datum } => text::for_each_font2_pixel(font, text, x, y, datum, color, pixel),
            Shape::Fill { x1, y1 } => {
                for py in y..y + y1 {
                    for px in x..x + x1 {
                        pixel(px, py, color);
                    }
                }
            }
        }
    }

    fn bounds(&self) -> Rect {
        let mut rect = Rect { x0: i32::MAX, y0: i32::MAX, x1: i32::MIN, y1: i32::MIN };
        if let Shape::Fill { x1, y1 } = self.shape {
            return Rect { x0: self.x, y0: self.y, x1: self.x + x1, y1: self.y + y1 };
        }
        self.pixels(|x, y, _| {
            rect.x0 = rect.x0.min(x);
            rect.y0 = rect.y0.min(y);
            rect.x1 = rect.x1.max(x + 1);
            rect.y1 = rect.y1.max(y + 1);
        });
        if rect.x0 > rect.x1 { Rect::EMPTY } else { rect }
    }
}

/// One screen's worth of items, in drawing order.
struct Items(Vec<Item, MAX_ITEMS>);

impl Items {
    fn add(&mut self, shape: Shape, x: i32, y: i32, color: u16, value: impl core::fmt::Display) {
        let mut text = String::new();
        let _ = write!(text, "{value}");
        let _ = self.0.push(Item { shape, text, x, y, color });
    }

    /// `render.drawString` or `render.rdrawString` at the current size.
    fn smooth(&mut self, font: &'static Font, align: Align, x: i32, y: i32, color: u16, value: impl core::fmt::Display) {
        self.add(Shape::Smooth { font, align }, x, y, color, value);
    }

    /// `background.drawString` with a free font.
    #[allow(clippy::too_many_arguments)]
    fn gfx(
        &mut self,
        font: &'static GfxFont,
        size: i32,
        datum: Datum,
        background: Option<u16>,
        x: i32,
        y: i32,
        color: u16,
        value: impl core::fmt::Display,
    ) {
        self.add(Shape::Gfx { font, size, datum, background }, x, y, color, value);
    }

    /// `fillRect(x, y, width, height)`.
    fn fill(&mut self, x: i32, y: i32, width: i32, height: i32, color: u16) {
        self.add(Shape::Fill { x1: width, y1: height }, x, y, color, "");
    }
}

/// `getCurrentHashRate`: the mean of the last ten one-second rates, with as
/// many decimals as fit once the rate has settled.
struct Hashrate {
    samples: Vec<f64, 10>,
    skip_first: u32,
    top: f64,
    decimals: u8,
}

impl Hashrate {
    fn new() -> Self {
        Self { samples: Vec::new(), skip_first: 3, top: 0.0, decimals: 2 }
    }

    fn update(&mut self, khs: f64) -> String<16> {
        if self.samples.is_full() {
            self.samples.remove(0);
        }
        let _ = self.samples.push(khs);
        let mean = (self.samples.iter().sum::<f64>() / self.samples.len() as f64).max(0.0);
        if self.skip_first > 0 {
            self.skip_first -= 1;
        } else if mean > self.top {
            self.top = mean;
            if mean > 999.9 {
                self.decimals = 0;
            } else if mean > 99.9 {
                self.decimals = 1;
            }
        }
        let mut out = String::new();
        let _ = match self.decimals {
            2 => write!(out, "{mean:.2}"),
            1 => write!(out, "{mean:.1}"),
            _ => write!(out, "{}", mean as u32),
        };
        out
    }
}

/// `suffix_string` with no significant-digit padding, as the best difficulty
/// is printed.
fn suffix_string(value: f64) -> String<16> {
    const STEPS: [(f64, char); 6] = [(1e18, 'E'), (1e15, 'P'), (1e12, 'T'), (1e9, 'G'), (1e6, 'M'), (1e3, 'K')];
    let (mut shown, mut suffix) = (value, None);
    for (scale, letter) in STEPS {
        if value >= scale {
            shown = value / scale;
            suffix = Some(letter);
            break;
        }
    }
    if suffix == Some('E') && shown > 999.99 {
        shown = 999.99;
    }
    if suffix.is_none() && shown < 0.001 {
        shown = 0.0;
    }
    let decimals = match suffix {
        Some(_) if shown > 99.999 => 1,
        Some(_) if shown > 9.999 => 2,
        Some(_) => 3,
        None if shown > 99.999 => 2,
        None if shown > 9.999 => 3,
        None => 4,
    };
    let mut out = String::new();
    let _ = write!(out, "{shown:.decimals$}");
    if let Some(letter) = suffix {
        let _ = out.push(letter);
    }
    out
}

/// `getTime`: `HH:MM`.
fn time() -> String<8> {
    let (hour, minute) = clock::hours_minutes();
    let mut out = String::new();
    let _ = write!(out, "{hour:02}:{minute:02}");
    out
}

/// `getBTCprice`: `$` and whole dollars.
fn price(data: &netdata::NetData) -> String<16> {
    let mut out = String::new();
    let _ = write!(out, "${}", data.btc_price);
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Miner,
    Clock,
    GlobalHash,
    Price,
}

impl Screen {
    /// `tDisplayCyclicScreens`, in order.
    fn next(self) -> Self {
        match self {
            Screen::Miner => Screen::Clock,
            Screen::Clock => Screen::GlobalHash,
            Screen::GlobalHash => Screen::Price,
            Screen::Price => Screen::Miner,
        }
    }

    fn background(self) -> &'static [u8] {
        match self {
            Screen::Miner => MINER_SCREEN,
            Screen::Clock => CLOCK_SCREEN,
            Screen::GlobalHash => GLOBAL_HASH_SCREEN,
            Screen::Price => PRICE_SCREEN,
        }
    }

    fn wants(self) -> u8 {
        match self {
            Screen::Miner => 0,
            Screen::Clock | Screen::Price => WANT_PRICE | WANT_HEIGHT,
            Screen::GlobalHash => WANT_PRICE | WANT_HEIGHT | WANT_GLOBAL,
        }
    }
}

/// The figures every screen draws from, gathered once a second like the C++
/// monitor task does.
struct Monitor {
    hashrate: Hashrate,
    last_hashes: [u32; 2],
    last_time: Instant,
    total_hashes: u64,
    started: Instant,
}

impl Monitor {
    fn new() -> Self {
        let now = Instant::now();
        Self { hashrate: Hashrate::new(), last_hashes: miner::hashes(), last_time: now, total_hashes: 0, started: now }
    }

    /// The hashrate as the screens print it.
    fn sample(&mut self) -> String<16> {
        let (now, hashes) = (Instant::now(), miner::hashes());
        let done = u64::from(hashes[0].wrapping_sub(self.last_hashes[0]))
            + u64::from(hashes[1].wrapping_sub(self.last_hashes[1]));
        let millis = (now - self.last_time).as_millis().max(1);
        self.total_hashes += done;
        self.last_hashes = hashes;
        self.last_time = now;
        // elapsedKHs * 1000 / mElapsed, in kH/s.
        self.hashrate.update(done as f64 / millis as f64)
    }

    fn items(&mut self, screen: Screen) -> Items {
        let rate = self.sample();
        let mut items = Items(Vec::new());
        match screen {
            Screen::Miner => self.miner(&mut items, rate),
            Screen::Clock => clock_screen(&mut items, rate),
            Screen::GlobalHash => global_hash_screen(&mut items),
            Screen::Price => price_screen(&mut items, rate),
        }
        items
    }

    /// `tDisplay_MinerScreen`.
    fn miner(&self, items: &mut Items, rate: String<16>) {
        let stats = stats::get();
        let uptime = stats.uptime_before + self.started.elapsed().as_secs();
        let (days, hours, minutes, seconds) = (uptime / 86_400, uptime / 3600 % 24, uptime / 60 % 60, uptime % 60);
        let mut mining_time: String<16> = String::new();
        let _ = write!(mining_time, "{days}  {hours:02}:{minutes:02}:{seconds:02}");

        items.smooth(&fonts::DIGITAL_NUMBERS_35, Align::Right, 118, 114, TFT_BLACK, rate);
        items.smooth(&fonts::DIGITAL_NUMBERS_18, Align::Right, 268, 138, TFT_BLACK, (stats.hashes_before + self.total_hashes) / 1_000_000);
        items.smooth(&fonts::DIGITAL_NUMBERS_18, Align::Left, 186, 20, LIGHT, stats.templates);
        items.smooth(&fonts::DIGITAL_NUMBERS_18, Align::Left, 186, 48, LIGHT, suffix_string(stats.best));
        items.smooth(&fonts::DIGITAL_NUMBERS_18, Align::Left, 186, 76, LIGHT, stats.shares32);
        items.smooth(&fonts::DIGITAL_NUMBERS_14, Align::Right, 315, 104, LIGHT, mining_time);
        items.smooth(&fonts::DIGITAL_NUMBERS_24, Align::Left, 285, 56, LIGHT, stats.blocks);
        items.smooth(&fonts::DIGITAL_NUMBERS_10, Align::Right, 239, 1, TFT_BLACK, temperature::read() as i32);
        items.smooth(&fonts::DIGITAL_NUMBERS_4, Align::Right, 244, 3, TFT_BLACK, 0);
        items.smooth(&fonts::DIGITAL_NUMBERS_10, Align::Right, 286, 1, TFT_BLACK, time());
    }
}

/// `tDisplay_ClockScreen`.
fn clock_screen(items: &mut Items, rate: String<16>) {
    let data = netdata::get();
    items.smooth(&fonts::DIGITAL_NUMBERS_25, Align::Right, 94, 129, TFT_BLACK, rate);
    items.gfx(&fonts::FREE_SANS_BOLD_9, 1, Datum::TopLeft, None, 202, 3, TFT_BLACK, price(&data));
    items.smooth(&fonts::DIGITAL_NUMBERS_18, Align::Right, 254, 140, TFT_BLACK, &data.block_height);
    items.gfx(&fonts::FREE_SANS_BOLD_18, 2, Datum::TopLeft, Some(TFT_BLACK), 130, 50, LIGHT, time());
}

/// `tDisplay_GlobalHashScreen`.
fn global_hash_screen(items: &mut Items) {
    let data = netdata::get();
    let height: u32 = data.block_height.parse().unwrap_or(0);
    let remaining = (height / HALVING_BLOCKS + 1) * HALVING_BLOCKS - height;
    let percent = (HALVING_BLOCKS - remaining) * 100 / HALVING_BLOCKS;
    let mut fee: String<24> = String::new();
    let _ = write!(fee, "{} sat/vB", data.half_hour_fee);
    let mut blocks: String<24> = String::new();
    let _ = write!(blocks, "{remaining} BLOCKS");

    items.gfx(&fonts::FREE_SANS_BOLD_9, 1, Datum::TopLeft, None, 198, 3, TFT_BLACK, price(&data));
    items.gfx(&fonts::FREE_SANS_BOLD_9, 1, Datum::TopLeft, None, 268, 3, TFT_BLACK, time());
    items.gfx(&fonts::FREE_SANS_9, 1, Datum::TopRight, None, 302, 52, GREY, fee);
    items.gfx(&fonts::FREE_SANS_9, 1, Datum::TopRight, None, 302, 88, GREY, &data.difficulty);
    items.smooth(&fonts::DIGITAL_NUMBERS_17, Align::Right, 274, 145, TFT_BLACK, &data.global_hash);
    items.smooth(&fonts::DIGITAL_NUMBERS_28, Align::Right, 140, 104, LIGHT, &data.block_height);
    // fillRect(2, 149, x2, 168): x2 and 168 are a width and a height.
    let x2 = 2 + (138 * percent / 100) as i32;
    items.fill(2, 149, x2, 168, LIGHT);
    items.add(Shape::Font2 { font: &fonts::FONT_2, datum: Datum::MiddleCenter }, 72, 159, TFT_BLACK, blocks);
}

/// `tDisplay_BTCprice`.
fn price_screen(items: &mut Items, rate: String<16>) {
    let data = netdata::get();
    items.smooth(&fonts::DIGITAL_NUMBERS_25, Align::Right, 94, 129, TFT_BLACK, rate);
    items.smooth(&fonts::DIGITAL_NUMBERS_18, Align::Right, 254, 138, TFT_WHITE, &data.block_height);
    items.gfx(&fonts::FREE_SANS_BOLD_9, 1, Datum::TopLeft, None, 222, 3, TFT_BLACK, time());
    items.gfx(&fonts::FREE_SANS_BOLD_24, 1, Datum::TopRight, Some(TFT_BLACK), 300, 58, LIGHT, price(&data));
}

const FULL: Rect = Rect { x0: 0, y0: 0, x1: WIDTH as i32, y1: HEIGHT as i32 };

/// Draws `area` of the screen: background, then every item over it.
fn compose(lcd: &mut Lcd, band: &mut [u16], background: &[u8], items: &Items, area: Rect) {
    let area = area.clip(i32::from(WIDTH), i32::from(HEIGHT));
    if area.is_empty() {
        return;
    }
    let width = (area.x1 - area.x0) as usize;
    let rows_per_band = (band.len() / width).max(1) as i32;
    let mut y0 = area.y0;
    while y0 < area.y1 {
        let y1 = (y0 + rows_per_band).min(area.y1);
        let part = Rect { x0: area.x0, y0, x1: area.x1, y1 };
        let pixels = &mut band[..width * (y1 - y0) as usize];
        for (row, line) in pixels.chunks_exact_mut(width).enumerate() {
            let start = ((y0 as usize + row) * usize::from(WIDTH) + area.x0 as usize) * 2;
            for (pixel, bytes) in line.iter_mut().zip(background[start..start + width * 2].chunks_exact(2)) {
                *pixel = u16::from_be_bytes([bytes[0], bytes[1]]);
            }
        }
        let mut canvas = Canvas { pixels, area: part };
        for item in &items.0 {
            if item.bounds().intersects(&part) {
                item.pixels(|x, y, color| canvas.set(x, y, color));
            }
        }
        lcd.blit(area.x0 as u16, y0 as u16, width as u16, canvas.pixels);
        y0 = y1;
    }
}

/// Redraws what changed since `shown`, then remembers what is on screen.
fn refresh(lcd: &mut Lcd, band: &mut [u16], background: &[u8], items: Items, shown: &mut Option<Items>) {
    match shown {
        None => compose(lcd, band, background, &items, FULL),
        Some(previous) => {
            for (index, now) in items.0.iter().enumerate() {
                let before = previous.0.get(index);
                if before == Some(now) {
                    continue;
                }
                let dirty = before.map_or(Rect::EMPTY, Item::bounds).union(now.bounds());
                compose(lcd, band, background, &items, dirty);
            }
        }
    }
    *shown = Some(items);
}

/// `tDisplay_LoadingScreen`, until Wi-Fi is up, or `tDisplay_SetupScreen`
/// for good once the portal opens: the board restarts when it closes.
async fn loading(lcd: &mut Lcd, band: &mut [u16]) {
    let mut items = Items(Vec::new());
    items.add(Shape::Font2 { font: &fonts::FONT_2, datum: Datum::TopLeft }, 24, 147, TFT_BLACK, VERSION);
    compose(lcd, band, INIT_SCREEN, &items, FULL);
    loop {
        match stats::get().status {
            Status::Joining => Timer::after_millis(200).await,
            Status::Portal => {
                compose(lcd, band, SETUP_SCREEN, &Items(Vec::new()), FULL);
                loop {
                    // The second button's long press still resets everything.
                    if let (1, Event::LongPress) = PRESSES.receive().await {
                        reset_configuration();
                    }
                }
            }
            _ => return,
        }
    }
}

/// `reset_configuration`: settings and Wi-Fi forgotten, then a restart into
/// the portal.
fn reset_configuration() -> ! {
    info!("erasing the configuration, restarting");
    crate::settings::erase();
    esp_hal::system::software_reset()
}

#[embassy_executor::task]
pub async fn run(mut lcd: Lcd) -> ! {
    let band = BAND.take();
    temperature::enable();
    lcd.init().await;
    loading(&mut lcd, band).await;

    let mut monitor = Monitor::new();
    let mut screen = Screen::Miner;
    let mut shown: Option<Items> = None;
    let mut next = Instant::now();
    loop {
        netdata::want(screen.wants());
        let items = monitor.items(screen);
        if lcd.is_on() {
            refresh(&mut lcd, band, screen.background(), items, &mut shown);
        }
        next += REFRESH;
        loop {
            match select(Timer::at(next), PRESSES.receive()).await {
                Either::First(()) => break,
                // Two-button boards: a click on the first turns the screen off
                // and on, a double click turns it upside down.
                Either::Second((0, Event::Click)) => {
                    let on = !lcd.is_on();
                    lcd.set_on(on);
                    shown = None;
                }
                Either::Second((0, Event::DoubleClick)) => {
                    lcd.flip();
                    shown = None;
                    break;
                }
                // A click on the second shows the next screen.
                Either::Second((1, Event::Click)) => {
                    screen = screen.next();
                    shown = None;
                    break;
                }
                // A long press on it forgets the configuration.
                Either::Second((1, Event::LongPress)) => reset_configuration(),
                Either::Second(_) => {}
            }
        }
    }
}
