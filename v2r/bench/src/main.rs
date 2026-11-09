//! Measures the SHA-256 mining kernels that NerdMiner v2R can run on the ESP32-S3,
//! after checking every one of them against the software reference.
//!
//! Prints one line per kernel on the USB serial port, then repeats.

#![no_std]
#![no_main]

use core::ptr::{addr_of_mut, read_volatile, write_volatile};
use core::sync::atomic::{AtomicU32, Ordering};

use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::main;
use esp_hal::sha::Sha;
use esp_hal::system::{CpuControl, Stack};
use esp_hal::time::Instant;
use esp_println::println;
use picominer_core::sha256::{IV, Work, compress};

esp_bootloader_esp_idf::esp_app_desc!();

/// Hashes the second core has done in software.
static SW_HASHES: AtomicU32 = AtomicU32::new(0);
static mut APP_STACK: Stack<16384> = Stack::new();

/// The software scan, kept in internal RAM rather than run from flash.
#[esp_hal::ram]
fn sw_scan(work: &Work, start: u32, count: u32, limit: u32) -> Option<u32> {
    work.scan(start, count, limit).map(|(nonce, _)| nonce)
}

/// The SHA accelerator's registers, from the ESP32-S3 technical reference manual.
const SHA_BASE: usize = 0x6003_b000;
const SHA_MODE: *mut u32 = SHA_BASE as *mut u32;
const SHA_START: *mut u32 = (SHA_BASE + 0x10) as *mut u32;
const SHA_CONTINUE: *mut u32 = (SHA_BASE + 0x14) as *mut u32;
const SHA_BUSY: *const u32 = (SHA_BASE + 0x18) as *const u32;
const SHA_H: *mut u32 = (SHA_BASE + 0x40) as *mut u32;
const SHA_M: *mut u32 = (SHA_BASE + 0x80) as *mut u32;
const MODE_SHA256: u32 = 2;

/// Words as the accelerator reads them: the bytes in memory order, so a
/// big-endian SHA word goes in byte-swapped.
const PAD_FIRST: u32 = 0x0000_0080;
const LEN_640: u32 = 0x8002_0000;
const LEN_256: u32 = 0x0001_0000;

/// The genesis block header, whose hash everybody knows.
const GENESIS: &str = "0100000000000000000000000000000000000000000000000000000000000000\
                       000000003ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa\
                       4b1e5e4a29ab5f49ffff001d1dac2b7c";
const GENESIS_NONCE: u32 = 0x7c2b_ac1d;
const GENESIS_HASH: &str = "6fe28c0ab6f1b372c1a6a246ae63f74f931e8365e15a089c68d6190000000000";

/// What the accelerator needs of a header: the midstate and bytes 64..76, both
/// already in register order.
#[derive(Clone, Copy)]
struct HwWork {
    midstate: [u32; 8],
    tail: [u32; 3],
}

impl HwWork {
    fn new(work: &Work) -> Self {
        let mut state = IV;
        compress(&mut state, work.first_block());
        let tail = work.tail();
        Self {
            midstate: state.map(u32::swap_bytes),
            tail: [tail[0].swap_bytes(), tail[1].swap_bytes(), tail[2].swap_bytes()],
        }
    }
}

#[inline(always)]
fn w(base: *mut u32, index: usize, value: u32) {
    unsafe { write_volatile(base.add(index), value) }
}

#[inline(always)]
fn r(base: *mut u32, index: usize) -> u32 {
    unsafe { read_volatile(base.add(index)) }
}

#[inline(always)]
fn wait_idle() {
    while unsafe { read_volatile(SHA_BUSY) } != 0 {}
}

#[inline(always)]
fn cycles() -> u32 {
    xtensa_lx::timer::get_cycle_count()
}

/// How a kernel fills the accelerator for one nonce.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kernel {
    /// Writes every message word of both blocks, as NerdMiner v1.8.3 does.
    Upstream,
    /// Never rewrites message words 9..=14, which are zero in both blocks.
    Minimal,
    /// Minimal, reading the whole first digest before writing it back.
    Batched,
    /// Batched, and fills the next nonce's first block while the second
    /// block of this one is being hashed.
    Pipe2,
    /// Pipe2, and also writes the second block's padding during the first.
    Pipe12,
}

const KERNELS: [Kernel; 5] =
    [Kernel::Upstream, Kernel::Minimal, Kernel::Batched, Kernel::Pipe2, Kernel::Pipe12];

impl Kernel {
    fn name(self) -> &'static str {
        match self {
            Kernel::Upstream => "hw upstream",
            Kernel::Minimal => "hw minimal ",
            Kernel::Batched => "hw batched ",
            Kernel::Pipe2 => "hw pipe2   ",
            Kernel::Pipe12 => "hw pipe12  ",
        }
    }

    /// Once before a run of [`Kernel::hash`]: the words no nonce touches.
    #[inline(always)]
    fn prepare(self) {
        unsafe { write_volatile(SHA_MODE, MODE_SHA256) };
        for i in 9..15 {
            w(SHA_M, i, 0);
        }
    }

    /// Leaves the double SHA-256 of the header with `nonce` in the H registers.
    #[inline(always)]
    fn hash(self, work: &HwWork, nonce: u32) {
        for (i, &word) in work.midstate.iter().enumerate() {
            w(SHA_H, i, word);
        }
        w(SHA_M, 0, work.tail[0]);
        w(SHA_M, 1, work.tail[1]);
        w(SHA_M, 2, work.tail[2]);
        w(SHA_M, 3, nonce);
        w(SHA_M, 4, PAD_FIRST);
        w(SHA_M, 5, 0);
        w(SHA_M, 6, 0);
        w(SHA_M, 7, 0);
        w(SHA_M, 8, 0);
        if self == Kernel::Upstream {
            for i in 9..15 {
                w(SHA_M, i, 0);
            }
        }
        w(SHA_M, 15, LEN_640);
        unsafe { write_volatile(SHA_CONTINUE, 1) };
        wait_idle();

        if self == Kernel::Upstream || self == Kernel::Minimal {
            for i in 0..8 {
                w(SHA_M, i, r(SHA_H, i));
            }
        } else {
            copy_digest();
        }
        w(SHA_M, 8, PAD_FIRST);
        if self == Kernel::Upstream {
            for i in 9..15 {
                w(SHA_M, i, 0);
            }
        }
        w(SHA_M, 15, LEN_256);
        unsafe { write_volatile(SHA_START, 1) };
        wait_idle();
    }

    fn digest(self, work: &HwWork, nonce: u32) -> [u8; 32] {
        self.prepare();
        self.hash(work, nonce);
        let mut out = [0u8; 32];
        for (i, bytes) in out.chunks_exact_mut(4).enumerate() {
            bytes.copy_from_slice(&r(SHA_H, i).to_le_bytes());
        }
        out
    }

    /// Same contract as [`Work::scan`] for one version: the first nonce whose
    /// top little-endian word is at most `limit`.
    fn scan(self, work: &HwWork, start: u32, count: u32, limit: u32) -> Option<u32> {
        match self {
            Kernel::Upstream => scan_plain::<0>(work, start, count, limit),
            Kernel::Minimal => scan_plain::<1>(work, start, count, limit),
            Kernel::Batched => scan_plain::<2>(work, start, count, limit),
            Kernel::Pipe2 => scan_pipe::<false>(work, start, count, limit),
            Kernel::Pipe12 => scan_pipe::<true>(work, start, count, limit),
        }
    }
}

const PLAIN: [Kernel; 3] = [Kernel::Upstream, Kernel::Minimal, Kernel::Batched];

#[inline(never)]
fn scan_plain<const K: usize>(work: &HwWork, start: u32, count: u32, limit: u32) -> Option<u32> {
    let kernel = PLAIN[K];
    kernel.scan_with(work, start, count, limit)
}

/// The first digest, from H to the start of the second block's message.
#[inline(always)]
fn copy_digest() {
    let h = [r(SHA_H, 0), r(SHA_H, 1), r(SHA_H, 2), r(SHA_H, 3), r(SHA_H, 4), r(SHA_H, 5), r(SHA_H, 6), r(SHA_H, 7)];
    for (i, word) in h.into_iter().enumerate() {
        w(SHA_M, i, word);
    }
}

/// Message words 0..=8 and 15 of the first block; 9..=14 stay zero.
#[inline(always)]
fn fill_first(work: &HwWork, nonce: u32) {
    w(SHA_M, 0, work.tail[0]);
    w(SHA_M, 1, work.tail[1]);
    w(SHA_M, 2, work.tail[2]);
    w(SHA_M, 3, nonce);
    w(SHA_M, 4, PAD_FIRST);
    w(SHA_M, 5, 0);
    w(SHA_M, 6, 0);
    w(SHA_M, 7, 0);
    w(SHA_M, 8, 0);
    w(SHA_M, 15, LEN_640);
}

/// Writes message words while the accelerator is busy, betting that it has
/// already taken them in. Validation says whether the bet holds.
#[esp_hal::ram]
fn scan_pipe<const EARLY_PAD: bool>(work: &HwWork, start: u32, count: u32, limit: u32) -> Option<u32> {
    Kernel::Minimal.prepare();
    fill_first(work, start);
    let mut nonce = start;
    for _ in 0..count {
        for (i, &word) in work.midstate.iter().enumerate() {
            w(SHA_H, i, word);
        }
        unsafe { write_volatile(SHA_CONTINUE, 1) };
        if EARLY_PAD {
            w(SHA_M, 8, PAD_FIRST);
            w(SHA_M, 15, LEN_256);
        }
        wait_idle();
        copy_digest();
        if !EARLY_PAD {
            w(SHA_M, 8, PAD_FIRST);
            w(SHA_M, 15, LEN_256);
        }
        unsafe { write_volatile(SHA_START, 1) };
        let next = nonce.wrapping_add(1);
        fill_first(work, next);
        wait_idle();
        if r(SHA_H, 7) <= limit {
            return Some(nonce);
        }
        nonce = next;
    }
    None
}

/// Pipe12 waiting a fixed number of cycles from each start instead of polling
/// the busy flag, which costs a slow peripheral read per poll.
#[esp_hal::ram]
fn scan_fixed(work: &HwWork, start: u32, count: u32, limit: u32, d1: u32, d2: u32) -> Option<u32> {
    Kernel::Minimal.prepare();
    fill_first(work, start);
    let mut nonce = start;
    for _ in 0..count {
        for (i, &word) in work.midstate.iter().enumerate() {
            w(SHA_H, i, word);
        }
        let t = cycles();
        unsafe { write_volatile(SHA_CONTINUE, 1) };
        w(SHA_M, 8, PAD_FIRST);
        w(SHA_M, 15, LEN_256);
        while cycles().wrapping_sub(t) < d1 {}
        copy_digest();
        let t = cycles();
        unsafe { write_volatile(SHA_START, 1) };
        let next = nonce.wrapping_add(1);
        fill_first(work, next);
        while cycles().wrapping_sub(t) < d2 {}
        if r(SHA_H, 7) <= limit {
            return Some(nonce);
        }
        nonce = next;
    }
    None
}

/// Whether `scan_fixed` with these delays finds exactly what the software finds.
fn fixed_valid(work: &Work, hw: &HwWork, d1: u32, d2: u32) -> bool {
    let mut seed = 0x9e37_79b9;
    (0..300).all(|_| {
        let nonce = next(&mut seed);
        work.agrees(nonce, |s, c, l| scan_fixed(hw, s, c, l, d1, d2).map(|n| (n, 0)))
    })
}

impl Kernel {
    #[inline(always)]
    fn scan_with(self, work: &HwWork, start: u32, count: u32, limit: u32) -> Option<u32> {
        self.prepare();
        let mut nonce = start;
        for _ in 0..count {
            self.hash(work, nonce);
            if r(SHA_H, 7) <= limit {
                return Some(nonce);
            }
            nonce = nonce.wrapping_add(1);
        }
        None
    }
}

fn decode<const N: usize>(hex: &str) -> [u8; N] {
    let mut out = [0u8; N];
    picominer_core::hex::decode_into(hex, &mut out).expect("valid hex");
    out
}

/// A cheap generator for test nonces.
fn next(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

fn top(digest: &[u8; 32]) -> u32 {
    u32::from_le_bytes([digest[28], digest[29], digest[30], digest[31]])
}

/// Checks a kernel against the software hash on many nonces: the full digest,
/// and that the scan stops exactly at the limit.
fn validate(kernel: Kernel, work: &Work, hw: &HwWork) -> bool {
    let mut seed = 0x1234_5678;
    for round in 0..2000 {
        let nonce = if round == 0 { GENESIS_NONCE } else { next(&mut seed) };
        let expected = work.hash(0, nonce);
        if PLAIN.contains(&kernel) && kernel.digest(hw, nonce) != expected {
            println!("{}: wrong digest for nonce {:08x}", kernel.name(), nonce);
            return false;
        }
        let t = top(&expected);
        let agrees = work.agrees(nonce, |s, c, l| kernel.scan(hw, s, c, l).map(|n| (n, 0)));
        if !agrees
            || kernel.scan(hw, nonce, 1, t) != Some(nonce)
            || (t > 0 && kernel.scan(hw, nonce, 1, t - 1).is_some())
        {
            println!("{}: scan disagrees at nonce {:08x}", kernel.name(), nonce);
            return false;
        }
    }
    true
}

/// Hashes per second of `scan` over `count` nonces with a limit nothing meets.
fn rate(count: u32, mut scan: impl FnMut(u32, u32) -> Option<u32>) -> f32 {
    let started = Instant::now();
    let mut nonce = 0;
    let end = count;
    while nonce < end {
        match scan(nonce, end - nonce) {
            Some(hit) => nonce = hit + 1,
            None => break,
        }
    }
    let micros = started.elapsed().as_micros() as f32;
    count as f32 * 1_000_000.0 / micros
}

/// Cycles one nonce spends in each accelerator step, averaged.
fn profile(work: &HwWork) {
    let runs = 1000u32;
    let (mut fill1, mut busy1, mut fill2, mut busy2) = (0u32, 0u32, 0u32, 0u32);
    Kernel::Minimal.prepare();
    for nonce in 0..runs {
        let t0 = cycles();
        for (i, &word) in work.midstate.iter().enumerate() {
            w(SHA_H, i, word);
        }
        w(SHA_M, 0, work.tail[0]);
        w(SHA_M, 1, work.tail[1]);
        w(SHA_M, 2, work.tail[2]);
        w(SHA_M, 3, nonce);
        w(SHA_M, 4, PAD_FIRST);
        w(SHA_M, 5, 0);
        w(SHA_M, 6, 0);
        w(SHA_M, 7, 0);
        w(SHA_M, 8, 0);
        w(SHA_M, 15, LEN_640);
        let t1 = cycles();
        unsafe { write_volatile(SHA_CONTINUE, 1) };
        wait_idle();
        let t2 = cycles();
        for i in 0..8 {
            w(SHA_M, i, r(SHA_H, i));
        }
        w(SHA_M, 8, PAD_FIRST);
        w(SHA_M, 15, LEN_256);
        let t3 = cycles();
        unsafe { write_volatile(SHA_START, 1) };
        wait_idle();
        let t4 = cycles();
        fill1 += t1.wrapping_sub(t0);
        busy1 += t2.wrapping_sub(t1);
        fill2 += t3.wrapping_sub(t2);
        busy2 += t4.wrapping_sub(t3);
    }
    println!(
        "cycles per nonce: fill1 {} busy1 {} fill2 {} busy2 {} total {}",
        fill1 / runs,
        busy1 / runs,
        fill2 / runs,
        busy2 / runs,
        (fill1 + busy1 + fill2 + busy2) / runs
    );
}

#[main]
fn main() -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);
    // Enables the accelerator's clock; the kernels then drive its registers.
    let _sha = Sha::new(peripherals.SHA);

    println!("NerdMiner v2R SHA bench, CPU at 240 MHz");

    let header: [u8; 80] = decode(GENESIS);
    let expected: [u8; 32] = decode(GENESIS_HASH);
    let work = Work::from_header(&header);
    assert!(work.hash(0, GENESIS_NONCE) == expected, "software reference is wrong");
    let hw = HwWork::new(&work);

    let mut valid = [false; KERNELS.len()];
    for (slot, kernel) in valid.iter_mut().zip(KERNELS) {
        *slot = validate(kernel, &work, &hw);
        println!("{}: {}", kernel.name(), if *slot { "valid" } else { "INVALID" });
    }

    let version = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let rolled = Work::with_versions(
        &header,
        &[version, version | 0x2000, version | 0x4000, version | 0x6000],
    );

    profile(&hw);
    for (ok, kernel) in valid.iter().zip(KERNELS) {
        if *ok {
            let hs = rate(200_000, |start, count| kernel.scan(&hw, start, count, 0));
            println!("{}: {:.1} kH/s", kernel.name(), hs / 1000.0);
        }
    }
    let hs = rate(20_000, |start, count| work.scan(start, count, 0).map(|(n, _)| n));
    println!("sw rust x1 flash: {:.1} kH/s", hs / 1000.0);
    let hs = rate(20_000, |start, count| sw_scan(&work, start, count, 0));
    println!("sw rust x1 iram : {:.1} kH/s", hs / 1000.0);
    let hs = rate(5_000, |start, count| sw_scan(&rolled, start, count, 0)) * 4.0;
    println!("sw rust x4 iram : {:.1} kH/s", hs / 1000.0);

    // The shortest delays that still hash right, then a safety margin.
    let mut d1 = 100;
    while d1 < 400 && !fixed_valid(&work, &hw, d1, 400) {
        d1 += 4;
    }
    let mut d2 = 60;
    while d2 < 400 && !fixed_valid(&work, &hw, d1, d2) {
        d2 += 4;
    }
    println!("fixed delays: first valid at d1 {} d2 {}", d1, d2);
    for margin in [0u32, 8, 16] {
        let (a, b) = (d1 + margin, d2 + margin);
        let ok = fixed_valid(&work, &hw, a, b);
        let hs = rate(200_000, |start, count| scan_fixed(&hw, start, count, 0, a, b));
        println!("hw fixed d1 {} d2 {}: {} {:.1} kH/s", a, b, if ok { "valid" } else { "INVALID" }, hs / 1000.0);
    }

    let mut cpu_control = CpuControl::new(peripherals.CPU_CTRL);
    let stack = unsafe { &mut *addr_of_mut!(APP_STACK) };
    let _app_core = cpu_control
        .start_app_core(stack, move || {
            let mut nonce = 0x8000_0000u32;
            loop {
                let _ = sw_scan(&rolled, nonce, 64, 0);
                nonce = nonce.wrapping_add(64);
                SW_HASHES.fetch_add(64 * 4, Ordering::Relaxed);
            }
        })
        .expect("second core starts");
    println!("second core mining in software, x4 versions");

    loop {
        let before = SW_HASHES.load(Ordering::Relaxed);
        let started = Instant::now();
        let hw_rate = rate(1_000_000, |start, count| Kernel::Pipe12.scan(&hw, start, count, 0));
        let micros = started.elapsed().as_micros() as f32;
        let sw = SW_HASHES.load(Ordering::Relaxed).wrapping_sub(before) as f32;
        let sw_rate = sw * 1_000_000.0 / micros;
        println!(
            "dual core: hw {:.1} + sw {:.1} = {:.1} kH/s",
            hw_rate / 1000.0,
            sw_rate / 1000.0,
            (hw_rate + sw_rate) / 1000.0
        );
    }
}
