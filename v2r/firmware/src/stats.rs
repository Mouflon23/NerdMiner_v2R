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
    /// Hashes since boot, as the log counts them every 15 s.
    pub total_hashes: u64,
    /// Hashes and seconds of uptime saved before this boot, `Mhashes` and
    /// `upTime` restored.
    pub hashes_before: u64,
    pub uptime_before: u64,
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
    total_hashes: 0,
    hashes_before: 0,
    uptime_before: 0,
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
        let done = u64::from(hashes[0].wrapping_sub(last[0])) + u64::from(hashes[1].wrapping_sub(last[1]));
        update(|stats| stats.total_hashes += done);
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

/// `saveIntervals`: the first save after 5 min, then further and further
/// apart, up to every 12 h.
const SAVE_INTERVALS: [u64; 7] = [5 * 60, 15 * 60, 30 * 60, 3600, 3 * 3600, 6 * 3600, 12 * 3600];

/// The figures `saveStat` keeps, one `key=value` line each.
fn encode() -> heapless::String<256> {
    let stats = get();
    let mut out = heapless::String::new();
    let _ = core::fmt::write(
        &mut out,
        format_args!(
            "best_diff={}
hashes={}
shares={}
valids={}
templates={}
upTime={}
",
            stats.best,
            stats.hashes_before + stats.total_hashes,
            stats.shares32,
            stats.blocks,
            stats.templates,
            stats.uptime_before + Instant::now().as_secs(),
        ),
    );
    out
}

/// `restoreStat`, before the miners start, when the settings ask for it.
pub fn restore() {
    let Some(text) = crate::settings::take_stats() else {
        return;
    };
    if !crate::settings::get().save_stats {
        return;
    }
    update(|stats| {
        for (key, value) in text.lines().filter_map(|line| line.split_once('=')) {
            match key {
                "best_diff" => stats.best = value.parse().unwrap_or(0.0),
                "hashes" => stats.hashes_before = value.parse().unwrap_or(0),
                "shares" => stats.shares32 = value.parse().unwrap_or(0),
                "valids" => stats.blocks = value.parse().unwrap_or(0),
                "templates" => stats.templates = value.parse().unwrap_or(0),
                "upTime" => stats.uptime_before = value.parse().unwrap_or(0),
                _ => {}
            }
        }
        info!(
            "statistics restored: {} templates, {} shares, best {:.4}, {} s up",
            stats.templates, stats.shares32, stats.best, stats.uptime_before
        );
    });
}

/// `saveStat` on the C++ schedule, when the settings ask for it.
#[embassy_executor::task]
pub async fn persist() {
    if !crate::settings::get().save_stats {
        return;
    }
    let mut next = Instant::now();
    let mut intervals = SAVE_INTERVALS.iter().copied().chain(core::iter::repeat(12 * 3600));
    loop {
        next += Duration::from_secs(intervals.next().unwrap_or(12 * 3600));
        embassy_time::Timer::at(next).await;
        if crate::settings::save_stats(&encode()) {
            info!("statistics saved");
        }
    }
}
