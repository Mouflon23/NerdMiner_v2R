//! Counters shared by the miners and the pool task, and their periodic log line.

use core::cell::RefCell;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::{Duration, Instant, Ticker};
use log::info;

use crate::miner;

/// What the miner is waiting for, shown on the screen.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Joining,
    Dhcp,
    Connecting,
    Mining,
    PoolError,
    /// The configuration portal is open.
    Portal,
}

impl Status {
    #[allow(dead_code, reason = "for the setup and loading screens, not ported yet")]
    pub fn label(self) -> &'static str {
        match self {
            Status::Joining => "Wi-Fi...",
            Status::Dhcp => "DHCP...",
            Status::Connecting => "Pool...",
            Status::Mining => "Mining",
            Status::PoolError => "Pool error",
            Status::Portal => "Setup",
        }
    }
}

#[derive(Clone, Copy)]
pub struct Stats {
    pub status: Status,
    pub ip: [u8; 4],
    pub accepted: u32,
    pub rejected: u32,
    /// Jobs received, the C++ firmware's `templates`.
    pub templates: u32,
    /// Accepted shares of difficulty 1 or more, its "32 bit shares".
    pub shares32: u32,
    /// Accepted shares that met the block target, its `valids`.
    pub blocks: u32,
    /// Best difficulty among accepted shares.
    pub best: f64,
    pub pool_difficulty: f64,
    pub height: u32,
    pub rssi: i32,
    pub drops: u32,
    /// Hits the software hash did not confirm, per miner.
    pub bad_hw_hits: u32,
    pub bad_sw_hits: u32,
    /// Chunks whose sample nonce the accelerator got wrong.
    pub bad_hw_checks: u32,
    /// Whether the Xtensa software loop matched PicoMiner's, once checked.
    pub sw_checked: Option<bool>,
}

static STATS: Mutex<CriticalSectionRawMutex, RefCell<Stats>> = Mutex::new(RefCell::new(Stats {
    status: Status::Joining,
    ip: [0; 4],
    accepted: 0,
    rejected: 0,
    templates: 0,
    shares32: 0,
    blocks: 0,
    best: 0.0,
    pool_difficulty: 0.0,
    height: 0,
    rssi: 0,
    drops: 0,
    bad_hw_hits: 0,
    bad_sw_hits: 0,
    bad_hw_checks: 0,
    sw_checked: None,
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
            "{:.1} kH/s (hw {:.1} + sw {:.1}), shares {}/{}, best {:.4}, pool diff {}, block {}, rssi {}, drops {}, bad hits {}/{}, bad checks {}, xtensa sw {:?}",
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
            stats.bad_hw_checks,
            stats.sw_checked,
        );
        #[cfg(feature = "tune")]
        info!("timing {:?}", miner::trial_name());
        last = hashes;
        since = now;
    }
}
