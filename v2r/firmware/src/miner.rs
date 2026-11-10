//! The two mining loops and the job they share.
//!
//! Core 0 runs [`mine_hw`] and nothing else: no scheduler and no interrupts. It
//! owns the SHA accelerator and the lower half of the nonces in the first header
//! version. Core 1 runs Wi-Fi and the pool, plus [`mine_sw`] at the lowest
//! priority. That loop takes the upper half of the nonces in every rolled
//! version. Hits wait in a queue that the Stratum loop polls.

use core::cell::{Cell, RefCell};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use heapless::Deque;
use picominer_core::sha256::Work;
use picominer_core::target::{hash_difficulty, share_limit};

use crate::config::LOCAL_MIN_DIFFICULTY;
use crate::hw::{self, HwWork, Timing};
use crate::stats;
use crate::sw::SwWork;

/// Nonces between two looks at the job slot: about 12 ms on either core.
const HW_CHUNK: u32 = 4096;
const SW_CHUNK: u32 = 128;

const UPPER_HALF: u32 = 1 << 31;

#[derive(Clone, Copy)]
pub struct Found {
    pub generation: u32,
    pub nonce: u32,
    /// Index of the header version, in the job's list.
    pub version: usize,
    pub hash: [u8; 32],
    pub difficulty: f64,
}

#[derive(Clone, Copy)]
struct Published {
    work: Work,
    hw: HwWork,
    sw: SwWork,
}

type Shared<T> = Mutex<CriticalSectionRawMutex, T>;

static GENERATION: AtomicU32 = AtomicU32::new(0);
static LIMIT: AtomicU32 = AtomicU32::new(0);
static JOB: Shared<Cell<Option<Published>>> = Mutex::new(Cell::new(None));
static FOUND: Shared<RefCell<Deque<Found, 8>>> = Mutex::new(RefCell::new(Deque::new()));
/// Hashes tried, by the accelerator and in software, wrapping.
static HASHES: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

/// Replaces the current work and returns its generation.
pub fn publish(header: &[u8; 80], versions: &[u32], work: Work) -> u32 {
    let hw = HwWork::new(header, versions[0]);
    let sw = SwWork::new(header, versions);
    let generation = JOB.lock(|slot| {
        slot.set(Some(Published { work, hw, sw }));
        let generation = GENERATION.load(Ordering::Relaxed) + 1;
        GENERATION.store(generation, Ordering::Release);
        generation
    });
    while take_found().is_some() {}
    generation
}

/// Stops both miners until the next [`publish`].
pub fn stop() {
    JOB.lock(|slot| {
        slot.set(None);
        GENERATION.store(GENERATION.load(Ordering::Relaxed) + 1, Ordering::Release);
    });
}

pub fn set_difficulty(difficulty: f64) {
    LIMIT.store(share_limit(difficulty.min(LOCAL_MIN_DIFFICULTY)), Ordering::Relaxed);
}

pub fn take_found() -> Option<Found> {
    FOUND.lock(|queue| queue.borrow_mut().pop_front())
}

/// Hashes tried by the accelerator and in software, wrapping.
pub fn hashes() -> [u32; 2] {
    [HASHES[0].load(Ordering::Relaxed), HASHES[1].load(Ordering::Relaxed)]
}

/// Checks a hit in software before queueing it, which also catches an
/// accelerator that something else disturbed.
fn report(generation: u32, work: &Work, nonce: u32, version: usize, limit: u32, from_hw: bool) {
    let hash = work.hash(version, nonce);
    let top = u32::from_le_bytes([hash[28], hash[29], hash[30], hash[31]]);
    if top > limit {
        stats::update(|stats| {
            if from_hw {
                stats.bad_hw_hits += 1;
            } else {
                stats.bad_sw_hits += 1;
            }
        });
        return;
    }
    let difficulty = hash_difficulty(&hash);
    let found = Found { generation, nonce, version, hash, difficulty };
    FOUND.lock(|queue| {
        // A full queue means nobody is reading: the newest hit can go.
        let _ = queue.borrow_mut().push_back(found);
    });
}

fn count(core: usize, hashes: u32) {
    // Only this loop writes its counter, so load then store is safe.
    let counter = &HASHES[core];
    counter.store(counter.load(Ordering::Relaxed).wrapping_add(hashes), Ordering::Relaxed);
}

/// Timings `tune` builds try in turn, 45 s each: two clean 15 s log periods.
#[cfg(feature = "tune")]
const TRIALS: [Timing; 5] = [
    Timing { first: 146, poll_first: true, poll_second: false },
    Timing { first: 150, poll_first: true, poll_second: false },
    Timing { first: 154, poll_first: true, poll_second: false },
    Timing { first: 158, poll_first: true, poll_second: false },
    Timing { first: 162, poll_first: true, poll_second: false },
];

fn timing(_started: esp_hal::time::Instant) -> Timing {
    #[cfg(feature = "tune")]
    {
        let trial = (_started.elapsed().as_secs() / 45) as usize % TRIALS.len();
        TRIAL.store(trial as u32, Ordering::Relaxed);
        TRIALS[trial]
    }
    #[cfg(not(feature = "tune"))]
    if SAFE_TIMING.load(Ordering::Relaxed) { hw::SAFE_TIMING } else { hw::TIMING }
}

/// Set for good once the accelerator gets a sample nonce wrong: from then on
/// it waits for the busy flag after both blocks.
static SAFE_TIMING: AtomicBool = AtomicBool::new(false);

/// Which timing a `tune` build is trying.
#[cfg(feature = "tune")]
static TRIAL: AtomicU32 = AtomicU32::new(0);

#[cfg(feature = "tune")]
pub fn trial_name() -> Timing {
    TRIALS[TRIAL.load(Ordering::Relaxed) as usize]
}

/// The accelerator loop, for core 0. It never returns.
pub fn mine_hw() -> ! {
    let started = esp_hal::time::Instant::now();
    let mut generation = u32::MAX;
    let mut job: Option<Published> = None;
    let mut nonce = 0;
    loop {
        let current = GENERATION.load(Ordering::Acquire);
        if current != generation {
            job = JOB.lock(Cell::get);
            generation = current;
            nonce = 0;
        }
        let Some(published) = &job else {
            core::hint::spin_loop();
            continue;
        };
        let limit = LIMIT.load(Ordering::Relaxed);
        let timing = timing(started);
        let hit = hw::scan(&published.hw, nonce, HW_CHUNK, limit, timing);
        let tried = match hit {
            Some(found) => {
                report(generation, &published.work, found, 0, limit, true);
                found.wrapping_sub(nonce) + 1
            }
            None => HW_CHUNK,
        };
        count(0, tried);
        // One nonce per chunk checked in software: a cheap guard against a
        // timing too tight, or anything else disturbing the accelerator.
        if hw::digest(&published.hw, nonce, timing) != published.work.hash(0, nonce) {
            stats::update(|stats| stats.bad_hw_checks += 1);
            SAFE_TIMING.store(true, Ordering::Relaxed);
        }
        // The lower half wraps around: a job lasts far less than the 1.8 h it takes.
        nonce = nonce.wrapping_add(tried) & (UPPER_HALF - 1);
    }
}

/// The software loop, for the lowest-priority task of core 1. It never returns.
pub fn mine_sw() -> ! {
    let mut generation = u32::MAX;
    let mut job: Option<Published> = None;
    let mut nonce = UPPER_HALF;
    // The Xtensa loop, once it has matched PicoMiner's on the first job.
    let mut fast: Option<bool> = None;
    loop {
        let current = GENERATION.load(Ordering::Acquire);
        if current != generation {
            job = JOB.lock(Cell::get);
            generation = current;
            nonce = UPPER_HALF;
        }
        if let (None, Some(published)) = (fast, &job) {
            let agrees = (0..32u32).all(|i| published.sw.agrees(&published.work, i.wrapping_mul(0x9e37_79b9)));
            stats::update(|stats| stats.sw_checked = Some(agrees));
            fast = Some(agrees);
        }
        let Some(published) = &job else {
            esp_radio_rtos_driver::usleep(10_000);
            continue;
        };
        let limit = LIMIT.load(Ordering::Relaxed);
        let work = &published.work;
        let scan = if fast == Some(true) {
            published.sw.scan(nonce, SW_CHUNK, limit)
        } else {
            work.scan(nonce, SW_CHUNK, limit)
        };
        let tried = match scan {
            Some((hit, version)) => {
                report(generation, work, hit, version, limit, false);
                // The versions after the hit are skipped: hits are rare.
                hit.wrapping_sub(nonce) + 1
            }
            None => SW_CHUNK,
        };
        count(1, tried * work.versions() as u32);
        nonce = UPPER_HALF | (nonce.wrapping_add(tried) & (UPPER_HALF - 1));
    }
}
