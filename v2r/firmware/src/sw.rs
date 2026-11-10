//! The software miner's SHA-256, for the Xtensa LX7 of core 1.
//!
//! It is PicoMiner's `Work::scan`, the same algorithm and shortcuts, with one
//! change: LLVM rotates a word with a shift left, a shift right and an or,
//! where the LX7's funnel shift does it in two instructions, `ssai` to set the
//! shift amount and `src` to shift the word against itself. SHA-256 rotates a
//! lot, so that saves about one instruction in five.
//!
//! Each rotation sets the shift amount register itself, and nothing else in
//! these loops shifts by a variable amount, so the register needs no saving.
//! [`SwWork::agrees`] checks the whole thing against PicoMiner's at boot.

use picominer_core::sha256::{IV, VERSIONS, Work, compress};

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

#[inline(always)]
fn rotr<const N: u32>(x: u32) -> u32 {
    let out: u32;
    // SAFETY: `src` with both halves the same word rotates it right by the
    // shift amount register, which `ssai` sets just before.
    unsafe {
        core::arch::asm!(
            "ssai {n}",
            "src {out}, {x}, {x}",
            n = const N,
            out = lateout(reg) out,
            x = in(reg) x,
            options(pure, nomem, nostack),
        );
    }
    out
}

#[inline(always)]
fn big0(x: u32) -> u32 {
    rotr::<2>(x) ^ rotr::<13>(x) ^ rotr::<22>(x)
}

#[inline(always)]
fn big1(x: u32) -> u32 {
    rotr::<6>(x) ^ rotr::<11>(x) ^ rotr::<25>(x)
}

#[inline(always)]
fn small0(x: u32) -> u32 {
    rotr::<7>(x) ^ rotr::<18>(x) ^ (x >> 3)
}

#[inline(always)]
fn small1(x: u32) -> u32 {
    rotr::<17>(x) ^ rotr::<19>(x) ^ (x >> 10)
}

#[inline(always)]
fn ch(e: u32, f: u32, g: u32) -> u32 {
    g ^ (e & (f ^ g))
}

#[inline(always)]
fn maj(a: u32, b: u32, c: u32) -> u32 {
    (a & b) | (c & (a | b))
}

macro_rules! round {
    ($a:ident, $b:ident, $c:ident, $d:ident, $e:ident, $f:ident, $g:ident, $h:ident, $ki:expr, $wi:expr) => {
        $h = $h.wrapping_add(big1($e)).wrapping_add(ch($e, $f, $g)).wrapping_add($ki).wrapping_add($wi);
        $d = $d.wrapping_add($h);
        $h = $h.wrapping_add(big0($a)).wrapping_add(maj($a, $b, $c));
    };
}

macro_rules! eight_rounds {
    ($s:ident, $w:ident, $i:expr) => {
        let [ref mut a, ref mut b, ref mut c, ref mut d, ref mut e, ref mut f, ref mut g, ref mut h] = $s;
        let (mut a_, mut b_, mut c_, mut d_, mut e_, mut f_, mut g_, mut h_) = (*a, *b, *c, *d, *e, *f, *g, *h);
        round!(a_, b_, c_, d_, e_, f_, g_, h_, K[$i], $w[$i]);
        round!(h_, a_, b_, c_, d_, e_, f_, g_, K[$i + 1], $w[$i + 1]);
        round!(g_, h_, a_, b_, c_, d_, e_, f_, K[$i + 2], $w[$i + 2]);
        round!(f_, g_, h_, a_, b_, c_, d_, e_, K[$i + 3], $w[$i + 3]);
        round!(e_, f_, g_, h_, a_, b_, c_, d_, K[$i + 4], $w[$i + 4]);
        round!(d_, e_, f_, g_, h_, a_, b_, c_, K[$i + 5], $w[$i + 5]);
        round!(c_, d_, e_, f_, g_, h_, a_, b_, K[$i + 6], $w[$i + 6]);
        round!(b_, c_, d_, e_, f_, g_, h_, a_, K[$i + 7], $w[$i + 7]);
        (*a, *b, *c, *d, *e, *f, *g, *h) = (a_, b_, c_, d_, e_, f_, g_, h_);
    };
}

#[inline(always)]
fn expand(w: &mut [u32; 64], from: usize, to: usize) {
    let mut i = from;
    while i < to {
        w[i] = small1(w[i - 2]).wrapping_add(w[i - 7]).wrapping_add(small0(w[i - 15])).wrapping_add(w[i - 16]);
        i += 1;
    }
}

/// What the loop needs of a header in up to [`VERSIONS`] versions, as
/// PicoMiner's `Work` keeps it.
#[derive(Clone, Copy)]
pub struct SwWork {
    versions: usize,
    midstate: [[u32; 8]; VERSIONS],
    /// State after rounds 0..=2 of the second block, which precede the nonce.
    state3: [[u32; 8]; VERSIONS],
    tail: [u32; 3],
    w16: u32,
    w17: u32,
}

impl SwWork {
    pub fn new(header: &[u8; 80], versions: &[u32]) -> Self {
        let word = |at: usize| u32::from_be_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]]);
        let tail = [word(64), word(68), word(72)];
        let mut midstate = [[0; 8]; VERSIONS];
        let mut state3 = [[0; 8]; VERSIONS];
        for (index, (mid, state)) in midstate.iter_mut().zip(&mut state3).enumerate() {
            let version = versions[if index < versions.len() { index } else { 0 }];
            let mut block = [0u32; 16];
            for (i, w) in block.iter_mut().enumerate() {
                *w = word(i * 4);
            }
            block[0] = version.swap_bytes();
            *mid = IV;
            compress(mid, &block);
            let [a, mut b, mut c, mut d, e, mut f, mut g, mut h] = *mid;
            round!(a, b, c, d, e, f, g, h, K[0], tail[0]);
            round!(h, a, b, c, d, e, f, g, K[1], tail[1]);
            round!(g, h, a, b, c, d, e, f, K[2], tail[2]);
            *state = [f, g, h, a, b, c, d, e];
        }
        // Message words 5..=14 are zero, word 15 is the 640 bit length.
        let w16 = small0(tail[1]).wrapping_add(tail[0]);
        let w17 = small1(640).wrapping_add(small0(tail[2])).wrapping_add(tail[1]);
        Self { versions: versions.len(), midstate, state3, tail, w16, w17 }
    }

    /// Same contract as PicoMiner's `Work::scan`: the first nonce and version
    /// whose hash has a top little-endian word at most `limit`.
    pub fn scan(&self, start: u32, count: u32, limit: u32) -> Option<(u32, usize)> {
        let mut w = [0u32; 64];
        w[0] = self.tail[0];
        w[1] = self.tail[1];
        w[2] = self.tail[2];
        w[4] = 0x8000_0000;
        w[15] = 640;
        w[16] = self.w16;
        w[17] = self.w17;
        let mut w2 = [0u32; 64];
        w2[8] = 0x8000_0000;
        w2[15] = 256;

        let mut nonce = start;
        for _ in 0..count {
            w[3] = nonce.swap_bytes();
            expand(&mut w, 18, 64);
            for version in 0..self.versions {
                if self.scan_version(version, &w, &mut w2, limit) {
                    return Some((nonce, version));
                }
            }
            nonce = nonce.wrapping_add(1);
        }
        None
    }

    #[inline(always)]
    fn scan_version(&self, version: usize, w: &[u32; 64], w2: &mut [u32; 64], limit: u32) -> bool {
        let midstate = &self.midstate[version];
        let [mut f, mut g, mut h, mut a, mut b, mut c, mut d, mut e] = self.state3[version];
        round!(f, g, h, a, b, c, d, e, K[3], w[3]);
        round!(e, f, g, h, a, b, c, d, K[4], w[4]);
        round!(d, e, f, g, h, a, b, c, K[5], w[5]);
        round!(c, d, e, f, g, h, a, b, K[6], w[6]);
        round!(b, c, d, e, f, g, h, a, K[7], w[7]);
        let mut s = [a, b, c, d, e, f, g, h];
        eight_rounds!(s, w, 8);
        eight_rounds!(s, w, 16);
        eight_rounds!(s, w, 24);
        eight_rounds!(s, w, 32);
        eight_rounds!(s, w, 40);
        eight_rounds!(s, w, 48);
        eight_rounds!(s, w, 56);

        for i in 0..8 {
            w2[i] = midstate[i].wrapping_add(s[i]);
        }
        expand(w2, 16, 61);

        let mut s = IV;
        eight_rounds!(s, w2, 0);
        eight_rounds!(s, w2, 8);
        eight_rounds!(s, w2, 16);
        eight_rounds!(s, w2, 24);
        eight_rounds!(s, w2, 32);
        eight_rounds!(s, w2, 40);
        eight_rounds!(s, w2, 48);
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = s;
        round!(a, b, c, d, e, f, g, h, K[56], w2[56]);
        round!(h, a, b, c, d, e, f, g, K[57], w2[57]);
        round!(g, h, a, b, c, d, e, f, K[58], w2[58]);
        round!(f, g, h, a, b, c, d, e, K[59], w2[59]);
        round!(e, f, g, h, a, b, c, d, K[60], w2[60]);
        // The h leaving round 63 is the e leaving round 60: the last three
        // rounds are never run.
        let _ = (a, b, c, d, e, f, g);
        h.wrapping_add(IV[7]).swap_bytes() <= limit
    }

    /// Whether this loop finds what PicoMiner's does around `nonce`, alone
    /// and as the second nonce of a run, in every version.
    pub fn agrees(&self, work: &Work, nonce: u32) -> bool {
        work.agrees(nonce, |start, count, limit| self.scan(start, count, limit))
    }
}
