//! Counters shared by the miners and the pool task, and their periodic log line.

use core::cell::RefCell;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::{Duration, Instant, Ticker};
use log::info;

use crate::miner;

#[derive(Clone, Copy, Default)]
pub struct Stats {
    pub accepted: u32,
    pub rejected: u32,
    pub blocks: u32,
    pub best: f64,
    pub pool_difficulty: f64,
    pub height: u32,
    pub rssi: i32,
    pub drops: u32,
    /// Hits the software hash did not confirm, per miner.
    pub bad_hw_hits: u32,
    pub bad_sw_hits: u32,
}

static STATS: Mutex<CriticalSectionRawMutex, RefCell<Stats>> = Mutex::new(RefCell::new(Stats {
    accepted: 0,
    rejected: 0,
    blocks: 0,
    best: 0.0,
    pool_difficulty: 0.0,
    height: 0,
    rssi: 0,
    drops: 0,
    bad_hw_hits: 0,
    bad_sw_hits: 0,
}));

pub fn update(change: impl FnOnce(&mut Stats)) {
    STATS.lock(|stats| change(&mut stats.borrow_mut()));
}

pub fn get() -> Stats {
    STATS.lock(|stats| *stats.borrow())
}

const PERIOD: Duration = Duration::from_secs(15);

#[embassy_executor::task]
pub async fn report() -> ! {
    let mut ticker = Ticker::every(PERIOD);
    let mut last = miner::hashes();
    let mut since = Instant::now();
    loop {
        ticker.next().await;
        let now = Instant::now();
        let hashes = miner::hashes();
        let seconds = (now - since).as_micros() as f32 / 1e6;
        let rate = |i: usize| hashes[i].wrapping_sub(last[i]) as f32 / seconds / 1000.0;
        let (hw, sw) = (rate(0), rate(1));
        let stats = get();
        info!(
            "{:.1} kH/s (hw {:.1} + sw {:.1}), shares {}/{}, best {:.4}, pool diff {}, block {}, rssi {}, drops {}, bad hits {}/{}",
            hw + sw,
            hw,
            sw,
            stats.accepted,
            stats.accepted + stats.rejected,
            stats.best,
            stats.pool_difficulty,
            stats.height,
            stats.rssi,
            stats.drops,
            stats.bad_hw_hits,
            stats.bad_sw_hits,
        );
        last = hashes;
        since = now;
    }
}
