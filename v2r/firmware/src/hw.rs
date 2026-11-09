//! The SHA accelerator kernel, measured as `hw pipe12` in `v2r/bench`.
//!
//! Register accesses cost more than the hashing: about 12 cycles per write and
//! 20 per read, against some 170 cycles per block. So the loop writes the next
//! words while the accelerator works. It takes in a block's message as soon as
//! the block starts, which `bench` checks on thousands of nonces.

use core::ptr::{read_volatile, write_volatile};

use picominer_core::sha256::{IV, compress};

const SHA_BASE: usize = 0x6003_b000;
const SHA_MODE: *mut u32 = SHA_BASE as *mut u32;
const SHA_START: *mut u32 = (SHA_BASE + 0x10) as *mut u32;
const SHA_CONTINUE: *mut u32 = (SHA_BASE + 0x14) as *mut u32;
const SHA_BUSY: *const u32 = (SHA_BASE + 0x18) as *const u32;
const SHA_H: *mut u32 = (SHA_BASE + 0x40) as *mut u32;
const SHA_M: *mut u32 = (SHA_BASE + 0x80) as *mut u32;
const MODE_SHA256: u32 = 2;

/// Words as the accelerator reads them: bytes in memory order.
const PAD_FIRST: u32 = 0x0000_0080;
const LEN_640: u32 = 0x8002_0000;
const LEN_256: u32 = 0x0001_0000;

/// Cycles the first block takes at 240 MHz, less a margin: the loop spins this
/// long before its single look at the busy flag, a slow read it would
/// otherwise repeat. `bench` found 156 to be the shortest delay that hashes right.
const FIRST_BLOCK_CYCLES: u32 = 140;

/// What the accelerator needs of one header version, in register order.
#[derive(Clone, Copy)]
pub struct HwWork {
    midstate: [u32; 8],
    tail: [u32; 3],
}

impl HwWork {
    pub fn new(header: &[u8; 80], version: u32) -> Self {
        let mut block = [0u32; 16];
        for (i, word) in block.iter_mut().enumerate() {
            let at = i * 4;
            *word = u32::from_be_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]]);
        }
        block[0] = version.swap_bytes();
        let mut state = IV;
        compress(&mut state, &block);
        let word = |at: usize| u32::from_le_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]]);
        Self { midstate: state.map(u32::swap_bytes), tail: [word(64), word(68), word(72)] }
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

/// Tries `count` nonces from `start` and returns the first whose hash has a
/// top little-endian word at most `limit`, like `Work::scan` for one version.
/// The caller owns the accelerator: nothing else may touch it meanwhile.
#[esp_hal::ram]
pub fn scan(work: &HwWork, start: u32, count: u32, limit: u32) -> Option<u32> {
    unsafe { write_volatile(SHA_MODE, MODE_SHA256) };
    for i in 9..15 {
        w(SHA_M, i, 0);
    }
    fill_first(work, start);
    let mut nonce = start;
    for _ in 0..count {
        for (i, &word) in work.midstate.iter().enumerate() {
            w(SHA_H, i, word);
        }
        let started = cycles();
        unsafe { write_volatile(SHA_CONTINUE, 1) };
        w(SHA_M, 8, PAD_FIRST);
        w(SHA_M, 15, LEN_256);
        while cycles().wrapping_sub(started) < FIRST_BLOCK_CYCLES {}
        wait_idle();

        let h = [r(SHA_H, 0), r(SHA_H, 1), r(SHA_H, 2), r(SHA_H, 3), r(SHA_H, 4), r(SHA_H, 5), r(SHA_H, 6), r(SHA_H, 7)];
        for (i, word) in h.into_iter().enumerate() {
            w(SHA_M, i, word);
        }
        unsafe { write_volatile(SHA_START, 1) };
        let next = nonce.wrapping_add(1);
        // Longer than the block takes, so the busy flag is read once.
        fill_first(work, next);
        wait_idle();
        if r(SHA_H, 7) <= limit {
            return Some(nonce);
        }
        nonce = next;
    }
    None
}
